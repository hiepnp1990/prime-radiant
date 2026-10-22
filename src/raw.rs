use std::collections::HashMap;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, Row, params};
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
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::ClaudeCode => "claude-code",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        match name {
            "claude-code" => Some(Source::ClaudeCode),
            _ => None,
        }
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

impl fmt::Debug for Sha256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Sha256({})", self.to_hex())
    }
}

/// The SHA-256 and length of the first `min(HEAD_LEN, confirmed_offset)` bytes of an append stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct StreamKey {
    pub origin: String,
    pub source: Source,
    /// The path relative to the source root, with `/` separators.
    pub locator: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamState {
    pub generation: u64,
    pub progress: Progress,
}

#[derive(Clone, Debug, PartialEq, Eq)]
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Applied {
    Appended { confirmed_offset: u64 },
    AlreadyHad,
    Rejected(Rejection),
    VersionStored,
    VersionUnchanged,
    NewGenerationStarted { generation: u64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

const STREAM_COLUMNS: &str = "id, source, locator, write_mode, generation, confirmed_offset, head_len, head_sha256, latest_checksum";

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

/// The only writer of the raw layer.
pub struct Store {
    dir: PathBuf,
    db: Connection,
}

impl Store {
    pub fn open(dir: &Path) -> Result<Self> {
        fs::create_dir_all(dir.join("streams"))?;
        fs::create_dir_all(dir.join("blobs"))?;
        let db = Connection::open(dir.join("raw.db"))?;
        db.pragma_update_and_check(None, "journal_mode", "WAL", |_| Ok(()))?;
        db.pragma_update(None, "synchronous", "FULL")?;
        db.pragma_update(None, "foreign_keys", "ON")?;
        db.execute_batch(SCHEMA)?;
        let store = Self {
            dir: dir.to_owned(),
            db,
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

    pub fn state(&self, origin: &str) -> Result<HashMap<StreamKey, StreamState>> {
        let mut query = self.db.prepare(&format!(
            "SELECT {STREAM_COLUMNS} FROM streams WHERE origin = ?1"
        ))?;
        let rows = query
            .query_map([origin], StreamRow::read)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.iter()
            .map(|row| {
                let source = Source::parse(&row.source).ok_or_else(|| {
                    Error::Corrupt(format!(
                        "stream {} has unknown source {}",
                        row.id, row.source
                    ))
                })?;
                let key = StreamKey {
                    origin: origin.to_owned(),
                    source,
                    locator: row.locator.clone(),
                };
                Ok((key, row.state()?))
            })
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
            } => self.read_log(id, state.generation, 0, confirmed_offset)?,
            Progress::Replace { latest_checksum } => fs::read(self.blob_path(latest_checksum))?,
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
        let end = start + bytes.len() as u64;
        if start > confirmed_offset {
            return Ok(Applied::Rejected(Rejection::Gap { confirmed_offset }));
        }
        if start < confirmed_offset {
            if end > confirmed_offset {
                return Ok(Applied::Rejected(Rejection::Overlap { confirmed_offset }));
            }
            let id = id.expect("a stream with confirmed bytes has a row");
            return Ok(if self.read_log(id, generation, start, end)? == bytes {
                Applied::AlreadyHad
            } else {
                Applied::Rejected(Rejection::ChecksumMismatch)
            });
        }
        if bytes.is_empty() {
            return Ok(Applied::AlreadyHad);
        }

        let id = match id {
            Some(id) => id,
            None => self.create_append_stream(key)?,
        };
        let dir = self.stream_dir(id);
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
        let path = self.blob_path(sha256);
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
                    .open(self.log_path(id, generation))
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

    fn read_log(&self, id: i64, generation: u64, start: u64, end: u64) -> Result<Vec<u8>> {
        let mut file = File::open(self.log_path(id, generation))?;
        file.seek(SeekFrom::Start(start))?;
        let mut bytes = vec![0; (end - start) as usize];
        file.read_exact(&mut bytes)?;
        Ok(bytes)
    }

    fn stream_dir(&self, id: i64) -> PathBuf {
        self.dir.join("streams").join(id.to_string())
    }

    fn log_path(&self, id: i64, generation: u64) -> PathBuf {
        self.stream_dir(id).join(format!("{generation}.log"))
    }

    fn blob_path(&self, sha256: Sha256) -> PathBuf {
        self.dir.join("blobs").join(sha256.to_hex())
    }
}

fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}
