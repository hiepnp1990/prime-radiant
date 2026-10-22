use std::collections::HashMap;
use std::fmt;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Read, Seek, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags, OptionalExtension, Row, params};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::Digest;

use crate::{Error, Result};

pub const HEAD_LEN: u64 = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WriteMode {
    Append,
    Replace,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Source {
    ClaudeCode,
    Codex,
    Grok,
    T3Code,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::ClaudeCode => "claude-code",
            Source::Codex => "codex",
            Source::Grok => "grok",
            Source::T3Code => "t3-code",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        match name {
            "claude-code" => Some(Source::ClaudeCode),
            "codex" => Some(Source::Codex),
            "grok" => Some(Source::Grok),
            "t3-code" => Some(Source::T3Code),
            _ => None,
        }
    }
}

/// The protocol names a source the way the raw layer stores it.
impl Serialize for Source {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Source {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let name = String::deserialize(deserializer)?;
        Self::parse(&name)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown source {name:?}")))
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Sha256([u8; 32]);

impl Sha256 {
    pub fn of(bytes: &[u8]) -> Self {
        Self(sha2::Sha256::digest(bytes).into())
    }

    pub fn from_hex(hex: &str) -> Option<Self> {
        let mut digest = [0; 32];
        hex::decode_to_slice(hex, &mut digest).ok()?;
        Some(Self(digest))
    }

    pub fn to_hex(self) -> String {
        hex::encode(self.0)
    }
}

impl Serialize for Sha256 {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for Sha256 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let hex = String::deserialize(deserializer)?;
        Self::from_hex(&hex)
            .ok_or_else(|| serde::de::Error::custom("expected 64 hex digits of a SHA-256"))
    }
}

impl fmt::Debug for Sha256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Sha256({})", self.to_hex())
    }
}

/// The SHA-256 and length of the first `min(HEAD_LEN, confirmed_offset)` bytes of an append stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeadCheck {
    pub len: u64,
    pub sha256: Sha256,
}

impl HeadCheck {
    pub fn of(bytes: &[u8]) -> Self {
        Self {
            len: bytes.len() as u64,
            sha256: Sha256::of(bytes),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StreamKey {
    pub origin: String,
    pub source: Source,
    /// The path relative to the source root, with `/` separators.
    pub locator: String,
}

/// The raw layer's row ID for a stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StreamId(pub i64);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stream {
    pub id: StreamId,
    pub key: StreamKey,
    pub state: StreamState,
}

/// Bytes `start..end` of one generation of an append stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RawRef {
    pub stream: StreamId,
    pub generation: u64,
    pub start: u64,
    pub end: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamState {
    pub generation: u64,
    pub progress: Progress,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Progress {
    Append {
        confirmed_offset: u64,
        head: HeadCheck,
    },
    Replace {
        latest_checksum: Sha256,
    },
}

impl StreamState {
    pub fn write_mode(&self) -> WriteMode {
        match self.progress {
            Progress::Append { .. } => WriteMode::Append,
            Progress::Replace { .. } => WriteMode::Replace,
        }
    }

    fn empty_append() -> Self {
        Self {
            generation: 0,
            progress: Progress::Append {
                confirmed_offset: 0,
                head: HeadCheck::of(&[]),
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Write {
    Segment {
        key: StreamKey,
        generation: u64,
        start: u64,
        bytes: Vec<u8>,
        sha256: Sha256,
    },
    Version {
        key: StreamKey,
        sha256: Sha256,
        bytes: Vec<u8>,
    },
    /// Starts `generation`, which must be newer than the stream's current one, at offset 0.
    NewGeneration { key: StreamKey, generation: u64 },
}

impl Write {
    pub fn key(&self) -> &StreamKey {
        match self {
            Write::Segment { key, .. }
            | Write::Version { key, .. }
            | Write::NewGeneration { key, .. } => key,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Applied {
    Appended { confirmed_offset: u64 },
    AlreadyHad,
    Rejected(Rejection),
    VersionStored,
    VersionUnchanged,
    NewGenerationStarted { generation: u64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Rejection {
    Gap { confirmed_offset: u64 },
    Overlap { confirmed_offset: u64 },
    ChecksumMismatch,
    WriteModeMismatch,
    GenerationMismatch { current: u64 },
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS origins (
    id TEXT PRIMARY KEY
);
CREATE TABLE IF NOT EXISTS streams (
    id INTEGER PRIMARY KEY,
    origin TEXT NOT NULL REFERENCES origins (id),
    source TEXT NOT NULL,
    locator TEXT NOT NULL,
    write_mode TEXT NOT NULL CHECK (write_mode IN ('append', 'replace')),
    generation INTEGER NOT NULL DEFAULT 0,
    confirmed_offset INTEGER NOT NULL DEFAULT 0,
    head_len INTEGER NOT NULL DEFAULT 0,
    head_sha256 TEXT,
    latest_checksum TEXT,
    UNIQUE (origin, source, locator)
);
CREATE TABLE IF NOT EXISTS segments (
    stream INTEGER NOT NULL REFERENCES streams (id),
    generation INTEGER NOT NULL,
    start_offset INTEGER NOT NULL,
    end_offset INTEGER NOT NULL,
    sha256 TEXT NOT NULL,
    PRIMARY KEY (stream, generation, start_offset)
);
CREATE TABLE IF NOT EXISTS versions (
    stream INTEGER NOT NULL REFERENCES streams (id),
    sha256 TEXT NOT NULL,
    captured_at INTEGER NOT NULL,
    PRIMARY KEY (stream, sha256)
);
";

const STREAM_COLUMNS: &str = "id, source, locator, write_mode, generation, confirmed_offset, head_len, head_sha256, latest_checksum, origin";

struct StreamRow {
    id: i64,
    source: String,
    locator: String,
    write_mode: String,
    generation: i64,
    confirmed_offset: i64,
    head_len: i64,
    head_sha256: Option<String>,
    latest_checksum: Option<String>,
    origin: String,
}

impl StreamRow {
    fn read(row: &Row) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            source: row.get(1)?,
            locator: row.get(2)?,
            write_mode: row.get(3)?,
            generation: row.get(4)?,
            confirmed_offset: row.get(5)?,
            head_len: row.get(6)?,
            head_sha256: row.get(7)?,
            latest_checksum: row.get(8)?,
            origin: row.get(9)?,
        })
    }

    fn stream(&self) -> Result<Stream> {
        let source = Source::parse(&self.source).ok_or_else(|| {
            Error::Corrupt(format!(
                "stream {} has unknown source {}",
                self.id, self.source
            ))
        })?;
        Ok(Stream {
            id: StreamId(self.id),
            key: StreamKey {
                origin: self.origin.clone(),
                source,
                locator: self.locator.clone(),
            },
            state: self.state()?,
        })
    }

    fn state(&self) -> Result<StreamState> {
        let corrupt = || Error::Corrupt(format!("stream {} has an invalid state", self.id));
        let count = |n: i64| u64::try_from(n).map_err(|_| corrupt());
        let checksum = |hex: &Option<String>| {
            hex.as_deref()
                .and_then(Sha256::from_hex)
                .ok_or_else(corrupt)
        };
        let progress = match self.write_mode.as_str() {
            "append" => Progress::Append {
                confirmed_offset: count(self.confirmed_offset)?,
                head: HeadCheck {
                    len: count(self.head_len)?,
                    sha256: checksum(&self.head_sha256)?,
                },
            },
            "replace" => Progress::Replace {
                latest_checksum: checksum(&self.latest_checksum)?,
            },
            _ => return Err(corrupt()),
        };
        Ok(StreamState {
            generation: count(self.generation)?,
            progress,
        })
    }
}

/// The only writer of the raw layer. It holds a lock on `raw.lock`, so a second `Store` on the same directory, in
/// any process, fails to open instead of trimming bytes the first one is still writing.
pub struct Store {
    dir: PathBuf,
    db: Connection,
    _lock: File,
}

impl Store {
    pub fn open(dir: &Path) -> Result<Self> {
        fs::create_dir_all(dir.join("streams"))?;
        fs::create_dir_all(dir.join("blobs"))?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join("raw.lock"))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(Error::Usage(format!(
                    "another process is writing {}; is a hub serving it?",
                    dir.display()
                )));
            }
            Err(TryLockError::Error(error)) => return Err(error.into()),
        }
        let db = Connection::open(dir.join("raw.db"))?;
        db.pragma_update_and_check(None, "journal_mode", "WAL", |_| Ok(()))?;
        db.pragma_update(None, "synchronous", "FULL")?;
        db.pragma_update(None, "foreign_keys", "ON")?;
        db.execute_batch(SCHEMA)?;
        let store = Self {
            dir: dir.to_owned(),
            db,
            _lock: lock,
        };
        store.trim_uncommitted()?;
        Ok(store)
    }

    pub fn apply(&mut self, write: &Write) -> Result<Applied> {
        match write {
            Write::Segment {
                key,
                generation,
                start,
                bytes,
                sha256,
            } => self.apply_segment(key, *generation, *start, bytes, *sha256),
            Write::Version { key, sha256, bytes } => self.apply_version(key, *sha256, bytes),
            Write::NewGeneration { key, generation } => self.apply_new_generation(key, *generation),
        }
    }

    /// One stream's confirmed state, or `None` if the stream does not exist.
    pub fn stream_state(&self, key: &StreamKey) -> Result<Option<StreamState>> {
        Ok(self.find(key)?.map(|(_, state)| state))
    }

    pub fn state(&self, origin: &str) -> Result<HashMap<StreamKey, StreamState>> {
        let mut query = self.db.prepare(&format!(
            "SELECT {STREAM_COLUMNS} FROM streams WHERE origin = ?1"
        ))?;
        let rows = query
            .query_map([origin], StreamRow::read)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.iter()
            .map(|row| row.stream().map(|stream| (stream.key, stream.state)))
            .collect()
    }

    /// The stream's confirmed bytes: the current generation of an append stream, or the latest version of a replace stream.
    pub fn contents(&self, key: &StreamKey) -> Result<Option<Vec<u8>>> {
        let Some((id, state)) = self.find(key)? else {
            return Ok(None);
        };
        let bytes = match state.progress {
            Progress::Append {
                confirmed_offset: 0,
                ..
            } => Vec::new(),
            Progress::Append {
                confirmed_offset, ..
            } => read_log(&self.dir, id, state.generation, 0, confirmed_offset)?,
            Progress::Replace { latest_checksum } => {
                fs::read(blob_path(&self.dir, latest_checksum))?
            }
        };
        Ok(Some(bytes))
    }

    fn find(&self, key: &StreamKey) -> Result<Option<(i64, StreamState)>> {
        let row = self
            .db
            .query_row(
                &format!("SELECT {STREAM_COLUMNS} FROM streams WHERE origin = ?1 AND source = ?2 AND locator = ?3"),
                params![key.origin, key.source.as_str(), key.locator],
                StreamRow::read,
            )
            .optional()?;
        row.map(|row| Ok((row.id, row.state()?))).transpose()
    }

    fn find_or_empty_append(&self, key: &StreamKey) -> Result<(Option<i64>, StreamState)> {
        Ok(match self.find(key)? {
            Some((id, state)) => (Some(id), state),
            None => (None, StreamState::empty_append()),
        })
    }

    fn create_append_stream(&mut self, key: &StreamKey) -> Result<i64> {
        let tx = self.db.transaction()?;
        tx.execute(
            "INSERT OR IGNORE INTO origins (id) VALUES (?1)",
            [&key.origin],
        )?;
        let id = tx.query_row(
            "INSERT INTO streams (origin, source, locator, write_mode, head_sha256)
             VALUES (?1, ?2, ?3, 'append', ?4) RETURNING id",
            params![
                key.origin,
                key.source.as_str(),
                key.locator,
                Sha256::of(&[]).to_hex()
            ],
            |row| row.get(0),
        )?;
        tx.commit()?;
        Ok(id)
    }

    fn apply_segment(
        &mut self,
        key: &StreamKey,
        generation: u64,
        start: u64,
        bytes: &[u8],
        sha256: Sha256,
    ) -> Result<Applied> {
        if Sha256::of(bytes) != sha256 {
            return Ok(Applied::Rejected(Rejection::ChecksumMismatch));
        }
        let (id, state) = self.find_or_empty_append(key)?;
        let Progress::Append {
            confirmed_offset,
            head,
        } = state.progress
        else {
            return Ok(Applied::Rejected(Rejection::WriteModeMismatch));
        };
        if generation != state.generation {
            return Ok(Applied::Rejected(Rejection::GenerationMismatch {
                current: state.generation,
            }));
        }
        if start > confirmed_offset {
            return Ok(Applied::Rejected(Rejection::Gap { confirmed_offset }));
        }
        let end = start + bytes.len() as u64;
        if start < confirmed_offset {
            if end > confirmed_offset {
                return Ok(Applied::Rejected(Rejection::Overlap { confirmed_offset }));
            }
            let id = id.expect("a stream with confirmed bytes has a row");
            return Ok(
                if read_log(&self.dir, id, generation, start, end)? == bytes {
                    Applied::AlreadyHad
                } else {
                    Applied::Rejected(Rejection::ChecksumMismatch)
                },
            );
        }
        if bytes.is_empty() {
            return Ok(Applied::AlreadyHad);
        }

        let id = match id {
            Some(id) => id,
            None => self.create_append_stream(key)?,
        };
        let dir = stream_dir(&self.dir, id);
        fs::create_dir_all(&dir)?;
        let mut log = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join(format!("{generation}.log")))?;
        log.seek(SeekFrom::Start(confirmed_offset))?;
        log.write_all(bytes)?;
        log.sync_all()?;
        if confirmed_offset == 0 {
            sync_dir(&dir)?;
            sync_dir(&self.dir.join("streams"))?;
        }
        let head = if head.len < HEAD_LEN {
            let mut prefix = vec![0; HEAD_LEN.min(end) as usize];
            log.seek(SeekFrom::Start(0))?;
            log.read_exact(&mut prefix)?;
            HeadCheck::of(&prefix)
        } else {
            head
        };

        let tx = self.db.transaction()?;
        tx.execute(
            "UPDATE streams SET confirmed_offset = ?1, head_len = ?2, head_sha256 = ?3 WHERE id = ?4",
            params![end as i64, head.len as i64, head.sha256.to_hex(), id],
        )?;
        tx.execute(
            "INSERT INTO segments (stream, generation, start_offset, end_offset, sha256) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, generation as i64, start as i64, end as i64, sha256.to_hex()],
        )?;
        tx.commit()?;
        Ok(Applied::Appended {
            confirmed_offset: end,
        })
    }

    fn apply_version(&mut self, key: &StreamKey, sha256: Sha256, bytes: &[u8]) -> Result<Applied> {
        if Sha256::of(bytes) != sha256 {
            return Ok(Applied::Rejected(Rejection::ChecksumMismatch));
        }
        match self.find(key)?.map(|(_, state)| state.progress) {
            Some(Progress::Append { .. }) => {
                return Ok(Applied::Rejected(Rejection::WriteModeMismatch));
            }
            Some(Progress::Replace { latest_checksum }) if latest_checksum == sha256 => {
                return Ok(Applied::VersionUnchanged);
            }
            _ => {}
        }
        self.write_blob(sha256, bytes)?;

        let captured_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_millis() as i64);
        let tx = self.db.transaction()?;
        tx.execute(
            "INSERT OR IGNORE INTO origins (id) VALUES (?1)",
            [&key.origin],
        )?;
        let id: i64 = tx.query_row(
            "INSERT INTO streams (origin, source, locator, write_mode, latest_checksum)
             VALUES (?1, ?2, ?3, 'replace', ?4)
             ON CONFLICT (origin, source, locator) DO UPDATE SET latest_checksum = excluded.latest_checksum
             RETURNING id",
            params![key.origin, key.source.as_str(), key.locator, sha256.to_hex()],
            |row| row.get(0),
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO versions (stream, sha256, captured_at) VALUES (?1, ?2, ?3)",
            params![id, sha256.to_hex(), captured_at],
        )?;
        tx.commit()?;
        Ok(Applied::VersionStored)
    }

    fn apply_new_generation(&mut self, key: &StreamKey, generation: u64) -> Result<Applied> {
        let (id, state) = self.find_or_empty_append(key)?;
        if !matches!(state.progress, Progress::Append { .. }) {
            return Ok(Applied::Rejected(Rejection::WriteModeMismatch));
        }
        if generation <= state.generation {
            return Ok(Applied::AlreadyHad);
        }
        // Generations count up by one, which keeps the trim at open proportional to the rewrites that happened.
        if generation != state.generation + 1 {
            return Ok(Applied::Rejected(Rejection::GenerationMismatch {
                current: state.generation,
            }));
        }
        let id = match id {
            Some(id) => id,
            None => self.create_append_stream(key)?,
        };
        self.db.execute(
            "UPDATE streams SET generation = ?1, confirmed_offset = 0, head_len = 0, head_sha256 = ?2 WHERE id = ?3",
            params![generation as i64, Sha256::of(&[]).to_hex(), id],
        )?;
        Ok(Applied::NewGenerationStarted { generation })
    }

    fn write_blob(&self, sha256: Sha256, bytes: &[u8]) -> Result<()> {
        let path = blob_path(&self.dir, sha256);
        if path.exists() {
            return Ok(());
        }
        let partial = path.with_extension("partial");
        let mut file = File::create(&partial)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&partial, &path)?;
        sync_dir(&self.dir.join("blobs"))?;
        Ok(())
    }

    fn trim_uncommitted(&self) -> Result<()> {
        let mut streams = self
            .db
            .prepare("SELECT id, generation FROM streams WHERE write_mode = 'append'")?;
        let streams = streams
            .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut committed = self.db.prepare(
            "SELECT COALESCE(MAX(end_offset), 0) FROM segments WHERE stream = ?1 AND generation = ?2",
        )?;
        for (id, current) in streams {
            for generation in 0..=current as u64 {
                let end: i64 =
                    committed.query_row(params![id, generation as i64], |row| row.get(0))?;
                let file = match OpenOptions::new()
                    .write(true)
                    .open(log_path(&self.dir, id, generation))
                {
                    Ok(file) => file,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error.into()),
                };
                if file.metadata()?.len() > end as u64 {
                    file.set_len(end as u64)?;
                    file.sync_all()?;
                }
            }
        }
        Ok(())
    }
}

/// A read-only view of the raw layer. Any number can be open beside the one `Store`.
pub struct View {
    dir: PathBuf,
    db: Connection,
}

impl View {
    pub fn open(dir: &Path) -> Result<Self> {
        let db = Connection::open_with_flags(
            dir.join("raw.db"),
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        Ok(Self {
            dir: dir.to_owned(),
            db,
        })
    }

    pub fn streams(&self) -> Result<Vec<Stream>> {
        let mut query = self.db.prepare(&format!(
            "SELECT {STREAM_COLUMNS} FROM streams ORDER BY origin, source, locator"
        ))?;
        let rows = query
            .query_map([], StreamRow::read)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.iter().map(StreamRow::stream).collect()
    }

    /// Reads committed bytes only, so a reference past the committed end is an error.
    pub fn read(&self, raw: &RawRef) -> Result<Vec<u8>> {
        let committed: i64 = self.db.query_row(
            "SELECT COALESCE(MAX(end_offset), 0) FROM segments WHERE stream = ?1 AND generation = ?2",
            params![raw.stream.0, raw.generation as i64],
            |row| row.get(0),
        )?;
        if raw.start > raw.end || raw.end > committed as u64 {
            return Err(Error::Corrupt(format!(
                "{raw:?} is outside the committed bytes of its stream"
            )));
        }
        read_log(&self.dir, raw.stream.0, raw.generation, raw.start, raw.end)
    }

    pub fn version(&self, sha256: Sha256) -> Result<Vec<u8>> {
        Ok(fs::read(blob_path(&self.dir, sha256))?)
    }

    /// Row counts of streams, segments, and versions.
    pub fn counts(&self) -> Result<[(&'static str, u64); 3]> {
        let count = |table: &'static str| -> Result<(&'static str, u64)> {
            let n: i64 =
                self.db
                    .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                        row.get(0)
                    })?;
            Ok((table, n as u64))
        };
        Ok([count("streams")?, count("segments")?, count("versions")?])
    }
}

fn read_log(dir: &Path, id: i64, generation: u64, start: u64, end: u64) -> Result<Vec<u8>> {
    let mut file = File::open(log_path(dir, id, generation))?;
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = vec![0; (end - start) as usize];
    file.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn stream_dir(dir: &Path, id: i64) -> PathBuf {
    dir.join("streams").join(id.to_string())
}

fn log_path(dir: &Path, id: i64, generation: u64) -> PathBuf {
    stream_dir(dir, id).join(format!("{generation}.log"))
}

fn blob_path(dir: &Path, sha256: Sha256) -> PathBuf {
    dir.join("blobs").join(sha256.to_hex())
}

fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}
