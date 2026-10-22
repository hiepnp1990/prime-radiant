pub mod claude_code;
pub mod codex;
pub mod t3_code;

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::ops::Range;
use std::path::Path;

use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::raw::{Progress, RawRef, Sha256, Source, Stream, StreamId, View};
use crate::{Error, Result};

/// What one source's parser knows: which session a stream feeds, and what its bytes mean.
pub trait Parser {
    /// The session a stream belongs to, or `None` for a stream the parser does not read.
    /// `first_line` is the first complete line of an append stream, without its newline.
    fn session(&self, locator: &str, first_line: Option<&[u8]>) -> Option<Identity>;
    /// One complete line of a stream of `session`, without its newline.
    fn line(&self, session: &Identity, line: &[u8]) -> Line;
    /// The latest version of a replace stream of `session`.
    fn version(&self, session: &Identity, bytes: &[u8]) -> Version;
}

pub fn parser(source: Source) -> &'static dyn Parser {
    match source {
        Source::ClaudeCode => &claude_code::ClaudeCode,
        Source::Codex => &codex::Codex,
        Source::T3Code => &t3_code::T3Code,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    pub native_id: String,
    pub lineage: Lineage,
    pub aliases: Vec<Alias>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Lineage {
    Main,
    Subagent { parent_native_id: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Alias {
    pub kind: AliasKind,
    pub value: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AliasKind {
    NativeId,
    AgentId,
}

impl AliasKind {
    pub fn as_str(self) -> &'static str {
        match self {
            AliasKind::NativeId => "native_id",
            AliasKind::AgentId => "agent_id",
        }
    }
}

/// Session attributes found in a stream. A title replaces the one before it, and every other fact keeps its first value.
/// The first timestamp marks when the session started, which decides who owns history that several sessions hold.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Facts {
    pub title: Option<String>,
    pub project: Option<String>,
    pub started: Option<Timestamp>,
    /// The title to use while the session has no `title`.
    pub fallback_title: Option<String>,
    /// Replaces the identity's lineage, but only in the batch of lines that creates the session.
    pub lineage: Option<Lineage>,
}

impl Facts {
    fn then(self, later: Facts) -> Facts {
        Facts {
            title: later.title.or(self.title),
            project: self.project.or(later.project),
            started: self.started.or(later.started),
            fallback_title: self.fallback_title.or(later.fallback_title),
            lineage: self.lineage.or(later.lineage),
        }
    }
}

/// What one version of a replace stream says. Its messages and session artifacts replace the ones its stream's previous version gave.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Version {
    pub facts: Facts,
    /// `None` when the stream does not track whether its session is archived or deleted.
    pub lifecycle: Option<Lifecycle>,
    /// Each message with its byte range in the version, after the parser decoded it, such as from zstd.
    pub messages: Vec<(Message, Range<u64>)>,
    /// Artifacts of the whole session, rather than of one message.
    pub artifacts: Vec<Artifact>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Lifecycle {
    pub archived_at: Option<Timestamp>,
    pub deleted_at: Option<Timestamp>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Line {
    pub message: Option<Message>,
    pub facts: Facts,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub id: String,
    pub parent_id: Option<String>,
    pub role: Role,
    pub at: Option<Timestamp>,
    pub searchable_text: String,
    pub artifacts: Vec<Artifact>,
    pub media: Vec<Media>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
    ToolCall,
    ToolResult,
    Attachment,
    System,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::ToolCall => "tool_call",
            Role::ToolResult => "tool_result",
            Role::Attachment => "attachment",
            Role::System => "system",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Artifact {
    pub kind: ArtifactKind,
    pub value: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArtifactKind {
    PullRequest,
    ChangedFile,
    /// The key of a session that another harness ran for this one, such as a Claude Code session behind a T3 Code thread.
    UnderlyingSession,
}

impl ArtifactKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ArtifactKind::PullRequest => "pull_request",
            ArtifactKind::ChangedFile => "changed_file",
            ArtifactKind::UnderlyingSession => "underlying_session",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Media {
    pub hash: Sha256,
    pub mime: String,
    pub bytes: u64,
}

/// Milliseconds since the Unix epoch, in UTC.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Timestamp(pub i64);

impl Timestamp {
    /// Parses `YYYY-MM-DDTHH:MM:SS[.fff]Z`, the form JavaScript's `toISOString` writes.
    pub fn parse_utc(text: &str) -> Option<Self> {
        let rest = text.strip_suffix('Z')?;
        let (clock, fraction) = rest.split_once('.').unwrap_or((rest, ""));
        let (date, time) = clock.split_once('T')?;
        let number = |part: &str, len: usize| {
            (part.len() == len && part.bytes().all(|b| b.is_ascii_digit()))
                .then(|| part.parse::<i64>().ok())
                .flatten()
        };
        let mut date = date.split('-');
        let mut time = time.split(':');
        let (year, month, day) = (
            number(date.next()?, 4)?,
            number(date.next()?, 2)?,
            number(date.next()?, 2)?,
        );
        let (hour, minute, second) = (
            number(time.next()?, 2)?,
            number(time.next()?, 2)?,
            number(time.next()?, 2)?,
        );
        if date.next().is_some()
            || time.next().is_some()
            || !(1..=12).contains(&month)
            || !(1..=31).contains(&day)
            || hour > 23
            || minute > 59
            || second > 60
        {
            return None;
        }
        let millis = match fraction {
            "" => 0,
            digits => {
                let digits = &digits[..digits.len().min(3)];
                number(digits, digits.len())? * 10_i64.pow(3 - digits.len() as u32)
            }
        };
        let days = days_from_civil(year, month, day);
        Some(Self(
            ((days * 24 + hour) * 60 + minute) * 60_000 + second * 1000 + millis,
        ))
    }
}

impl serde::Serialize for Timestamp {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl fmt::Display for Timestamp {
    /// Writes `YYYY-MM-DDTHH:MM:SSZ`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let seconds = self.0.div_euclid(1000);
        let (days, clock) = (seconds.div_euclid(86_400), seconds.rem_euclid(86_400));
        let (year, month, day) = civil_from_days(days);
        write!(
            f,
            "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
            clock / 3600,
            clock / 60 % 60,
            clock % 60
        )
    }
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The session key, `<source>/<native-id>`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SessionKey(String);

impl SessionKey {
    pub fn new(source: Source, native_id: &str) -> Self {
        Self(format!("{}/{native_id}", source.as_str()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct SyncSummary {
    pub streams_synced: usize,
    pub messages_indexed: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MessageRef {
    Log(RawRef),
    Version {
        stream: StreamId,
        sha256: Sha256,
        decoded_start: u64,
        decoded_end: u64,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Indexed {
    Append {
        generation: u64,
        complete_lines_end: u64,
    },
    Replace {
        checksum: Sha256,
    },
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS sessions (
    id INTEGER PRIMARY KEY,
    key TEXT NOT NULL UNIQUE,
    source TEXT NOT NULL,
    native_id TEXT NOT NULL,
    ran_on TEXT NOT NULL,
    title TEXT,
    project TEXT,
    kind TEXT NOT NULL CHECK (kind IN ('main', 'fork', 'subagent')),
    parent_key TEXT,
    started_at INTEGER,
    last_activity INTEGER,
    message_count INTEGER NOT NULL DEFAULT 0,
    own_message_count INTEGER NOT NULL DEFAULT 0,
    archived_at INTEGER,
    deleted_at INTEGER,
    CHECK ((kind = 'main') = (parent_key IS NULL))
);
CREATE INDEX IF NOT EXISTS sessions_by_activity ON sessions (last_activity);
CREATE INDEX IF NOT EXISTS sessions_by_project ON sessions (project, last_activity);
CREATE INDEX IF NOT EXISTS sessions_by_origin ON sessions (ran_on, last_activity);
CREATE INDEX IF NOT EXISTS sessions_by_parent ON sessions (parent_key);

CREATE TABLE IF NOT EXISTS messages (
    id INTEGER PRIMARY KEY,
    session_key TEXT NOT NULL REFERENCES sessions (key),
    message_id TEXT NOT NULL,
    parent_id TEXT,
    position INTEGER NOT NULL,
    role TEXT NOT NULL CHECK (role IN ('user', 'assistant', 'tool_call', 'tool_result', 'attachment', 'system')),
    at INTEGER,
    searchable_text TEXT NOT NULL,
    raw_stream INTEGER NOT NULL,
    raw_generation INTEGER,
    raw_version TEXT,
    raw_start INTEGER NOT NULL,
    raw_end INTEGER NOT NULL,
    copied_from TEXT REFERENCES sessions (key),
    UNIQUE (session_key, message_id),
    UNIQUE (session_key, position),
    CHECK ((raw_generation IS NULL) <> (raw_version IS NULL))
);
CREATE INDEX IF NOT EXISTS messages_by_stream ON messages (raw_stream);
CREATE INDEX IF NOT EXISTS messages_by_id ON messages (message_id);

CREATE TABLE IF NOT EXISTS aliases (
    value TEXT PRIMARY KEY,
    kind TEXT NOT NULL CHECK (kind IN ('native_id', 'agent_id')),
    session_key TEXT NOT NULL REFERENCES sessions (key)
);

CREATE TABLE IF NOT EXISTS artifacts (
    session_key TEXT NOT NULL REFERENCES sessions (key),
    kind TEXT NOT NULL CHECK (kind IN ('pull_request', 'changed_file', 'underlying_session')),
    value TEXT NOT NULL,
    message INTEGER REFERENCES messages (id) ON DELETE CASCADE,
    raw_stream INTEGER,
    PRIMARY KEY (session_key, kind, value),
    CHECK ((message IS NULL) <> (raw_stream IS NULL))
);
CREATE INDEX IF NOT EXISTS artifacts_by_message ON artifacts (message);
CREATE INDEX IF NOT EXISTS artifacts_by_stream ON artifacts (raw_stream);
CREATE INDEX IF NOT EXISTS artifacts_by_value ON artifacts (value);

CREATE TABLE IF NOT EXISTS media (
    message INTEGER NOT NULL REFERENCES messages (id) ON DELETE CASCADE,
    hash TEXT NOT NULL,
    mime TEXT NOT NULL,
    bytes INTEGER NOT NULL,
    PRIMARY KEY (message, hash)
);
CREATE INDEX IF NOT EXISTS media_by_hash ON media (hash);

CREATE TABLE IF NOT EXISTS progress (
    stream INTEGER PRIMARY KEY,
    generation INTEGER,
    indexed_offset INTEGER,
    indexed_checksum TEXT,
    CHECK ((generation IS NULL) = (indexed_offset IS NULL)),
    CHECK ((indexed_offset IS NULL) <> (indexed_checksum IS NULL))
);

CREATE VIRTUAL TABLE IF NOT EXISTS message_search USING fts5 (
    searchable_text, session_key UNINDEXED, content = 'messages', content_rowid = 'id'
);
CREATE TRIGGER IF NOT EXISTS messages_search_insert AFTER INSERT ON messages
WHEN new.copied_from IS NULL BEGIN
    INSERT INTO message_search (rowid, searchable_text, session_key)
    VALUES (new.id, new.searchable_text, new.session_key);
END;
CREATE TRIGGER IF NOT EXISTS messages_search_delete AFTER DELETE ON messages
WHEN old.copied_from IS NULL BEGIN
    INSERT INTO message_search (message_search, rowid, searchable_text, session_key)
    VALUES ('delete', old.id, old.searchable_text, old.session_key);
END;
CREATE TRIGGER IF NOT EXISTS messages_search_copied AFTER UPDATE OF copied_from ON messages
WHEN old.copied_from IS NULL AND new.copied_from IS NOT NULL BEGIN
    INSERT INTO message_search (message_search, rowid, searchable_text, session_key)
    VALUES ('delete', old.id, old.searchable_text, old.session_key);
END;
CREATE TRIGGER IF NOT EXISTS messages_search_uncopied AFTER UPDATE OF copied_from ON messages
WHEN old.copied_from IS NOT NULL AND new.copied_from IS NULL BEGIN
    INSERT INTO message_search (rowid, searchable_text, session_key)
    VALUES (new.id, new.searchable_text, new.session_key);
END;

CREATE VIRTUAL TABLE IF NOT EXISTS title_search USING fts5 (
    title, key UNINDEXED, content = 'sessions', content_rowid = 'id'
);
CREATE TRIGGER IF NOT EXISTS sessions_search_insert AFTER INSERT ON sessions BEGIN
    INSERT INTO title_search (rowid, title, key) VALUES (new.id, new.title, new.key);
END;
CREATE TRIGGER IF NOT EXISTS sessions_search_update AFTER UPDATE OF title ON sessions BEGIN
    INSERT INTO title_search (title_search, rowid, title, key) VALUES ('delete', old.id, old.title, old.key);
    INSERT INTO title_search (rowid, title, key) VALUES (new.id, new.title, new.key);
END;

CREATE TABLE IF NOT EXISTS briefs (
    session_key TEXT PRIMARY KEY REFERENCES sessions (key),
    body TEXT NOT NULL,
    built_through INTEGER,
    built_at INTEGER NOT NULL,
    method TEXT NOT NULL
);
";

/// The only writer of the session layer, in `index.db` beside the raw layer.
pub struct Indexer {
    raw: View,
    db: Connection,
}

impl Indexer {
    pub fn open(dir: &Path) -> Result<Self> {
        let raw = View::open(dir)?;
        let db = open_db(dir)?;
        Ok(Self { raw, db })
    }

    /// Indexes whatever the raw layer holds past each stream's progress. Running it again changes nothing.
    pub fn sync(&mut self) -> Result<SyncSummary> {
        let indexed = self.indexed()?;
        let mut summary = SyncSummary::default();
        for stream in self.raw.streams()? {
            let parser = parser(stream.key.source);
            let before = indexed.get(&stream.id).copied();
            let added = match stream.state.progress {
                Progress::Append {
                    confirmed_offset, ..
                } => self.sync_append(parser, &stream, before, confirmed_offset)?,
                Progress::Replace { latest_checksum } => {
                    self.sync_replace(parser, &stream, before, latest_checksum)?
                }
            };
            if let Some(added) = added {
                summary.streams_synced += 1;
                summary.messages_indexed += added;
            }
        }
        Ok(summary)
    }

    fn indexed(&self) -> Result<HashMap<StreamId, Indexed>> {
        let mut query = self
            .db
            .prepare("SELECT stream, generation, indexed_offset, indexed_checksum FROM progress")?;
        let rows = query
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|(stream, generation, offset, checksum)| {
                let indexed = match (generation, offset, checksum.as_deref()) {
                    (Some(generation), Some(offset), None) => Indexed::Append {
                        generation: generation as u64,
                        complete_lines_end: offset as u64,
                    },
                    (None, None, Some(hex)) => Indexed::Replace {
                        checksum: Sha256::from_hex(hex).ok_or_else(|| {
                            Error::Corrupt(format!("index progress of stream {stream}"))
                        })?,
                    },
                    _ => {
                        return Err(Error::Corrupt(format!("index progress of stream {stream}")));
                    }
                };
                Ok((StreamId(stream), indexed))
            })
            .collect()
    }

    fn sync_append(
        &mut self,
        parser: &dyn Parser,
        stream: &Stream,
        before: Option<Indexed>,
        confirmed_offset: u64,
    ) -> Result<Option<usize>> {
        let generation = stream.state.generation;
        let (restart, start) = match before {
            Some(Indexed::Append {
                generation: indexed,
                complete_lines_end,
            }) if indexed == generation => (false, complete_lines_end),
            Some(_) => (true, 0),
            None => (false, 0),
        };
        let bytes = if start < confirmed_offset {
            self.raw.read(&RawRef {
                stream: stream.id,
                generation,
                start,
                end: confirmed_offset,
            })?
        } else {
            Vec::new()
        };
        let complete = bytes
            .iter()
            .rposition(|b| *b == b'\n')
            .map_or(0, |last| last + 1);
        if complete == 0 && !restart {
            return Ok(None);
        }
        let first_line = self.first_line(stream, confirmed_offset)?;
        let Some(identity) = parser.session(&stream.key.locator, Some(&first_line)) else {
            return Ok(None);
        };

        let (facts, lines) = read_lines(parser, &identity, &bytes[..complete]);
        let messages: Vec<_> = lines
            .into_iter()
            .map(|(message, range)| {
                let raw = MessageRef::Log(RawRef {
                    stream: stream.id,
                    generation,
                    start: start + range.start,
                    end: start + range.end,
                });
                (message, raw)
            })
            .collect();
        let tx = self.db.transaction()?;
        let key = ensure_session(&tx, stream, &identity, &facts)?;
        let mut touched: Vec<String> = Vec::new();
        if restart {
            let mut delete =
                tx.prepare("DELETE FROM messages WHERE raw_stream = ?1 RETURNING message_id")?;
            touched = delete
                .query_map([stream.id.0], |row| row.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            drop(delete);
            tx.execute(
                "UPDATE sessions SET started_at = NULL WHERE key = ?1",
                [key.as_str()],
            )?;
        }
        let added = index_lines(&tx, stream.key.source, &key, &facts, &messages, touched)?;
        tx.execute(
            "INSERT INTO progress (stream, generation, indexed_offset) VALUES (?1, ?2, ?3)
             ON CONFLICT (stream) DO UPDATE SET generation = excluded.generation,
                indexed_offset = excluded.indexed_offset, indexed_checksum = NULL",
            params![
                stream.id.0,
                generation as i64,
                (start + complete as u64) as i64
            ],
        )?;
        tx.commit()?;
        Ok(Some(added))
    }

    fn sync_replace(
        &mut self,
        parser: &dyn Parser,
        stream: &Stream,
        before: Option<Indexed>,
        latest_checksum: Sha256,
    ) -> Result<Option<usize>> {
        if before
            == Some(Indexed::Replace {
                checksum: latest_checksum,
            })
        {
            return Ok(None);
        }
        let Some(identity) = parser.session(&stream.key.locator, None) else {
            return Ok(None);
        };
        let version = parser.version(&identity, &self.raw.version(latest_checksum)?);
        let messages: Vec<_> = version
            .messages
            .into_iter()
            .map(|(message, range)| {
                let raw = MessageRef::Version {
                    stream: stream.id,
                    sha256: latest_checksum,
                    decoded_start: range.start,
                    decoded_end: range.end,
                };
                (message, raw)
            })
            .collect();
        let tx = self.db.transaction()?;
        let key = ensure_session(&tx, stream, &identity, &version.facts)?;
        if let Some(lifecycle) = version.lifecycle {
            tx.execute(
                "UPDATE sessions SET archived_at = ?2, deleted_at = ?3 WHERE key = ?1",
                params![
                    key.as_str(),
                    lifecycle.archived_at.map(|at| at.0),
                    lifecycle.deleted_at.map(|at| at.0)
                ],
            )?;
        }
        let mut delete =
            tx.prepare("DELETE FROM messages WHERE raw_stream = ?1 RETURNING message_id")?;
        let touched: Vec<String> = delete
            .query_map([stream.id.0], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        drop(delete);
        tx.execute("DELETE FROM artifacts WHERE raw_stream = ?1", [stream.id.0])?;
        for artifact in &version.artifacts {
            tx.execute(
                "INSERT OR IGNORE INTO artifacts (session_key, kind, value, raw_stream) VALUES (?1, ?2, ?3, ?4)",
                params![key.as_str(), artifact.kind.as_str(), artifact.value, stream.id.0],
            )?;
        }
        let added = index_lines(
            &tx,
            stream.key.source,
            &key,
            &version.facts,
            &messages,
            touched,
        )?;
        tx.execute(
            "INSERT INTO progress (stream, indexed_checksum) VALUES (?1, ?2)
             ON CONFLICT (stream) DO UPDATE SET generation = NULL, indexed_offset = NULL,
                indexed_checksum = excluded.indexed_checksum",
            params![stream.id.0, latest_checksum.to_hex()],
        )?;
        tx.commit()?;
        Ok(Some(added))
    }

    /// The bytes before the first newline of the stream's current generation, or all of them if it has none yet.
    fn first_line(&self, stream: &Stream, end: u64) -> Result<Vec<u8>> {
        const CHUNK: u64 = 64 * 1024;
        let mut line = Vec::new();
        let mut start = 0;
        while start < end {
            let chunk = self.raw.read(&RawRef {
                stream: stream.id,
                generation: stream.state.generation,
                start,
                end: (start + CHUNK).min(end),
            })?;
            if let Some(newline) = chunk.iter().position(|b| *b == b'\n') {
                line.extend_from_slice(&chunk[..newline]);
                break;
            }
            start += chunk.len() as u64;
            line.extend(chunk);
        }
        Ok(line)
    }
}

/// Raise this when `SCHEMA` changes. An index built for another schema is emptied, and the next sync rebuilds it from the raw layer.
const SCHEMA_VERSION: i64 = 1;

const TABLES: [&str; 9] = [
    "message_search",
    "title_search",
    "briefs",
    "progress",
    "media",
    "artifacts",
    "aliases",
    "messages",
    "sessions",
];

/// Opens `index.db` in `dir`, creating its tables if they are missing.
pub fn open_db(dir: &Path) -> Result<Connection> {
    let db = Connection::open(dir.join("index.db"))?;
    db.pragma_update_and_check(None, "journal_mode", "WAL", |_| Ok(()))?;
    let version: i64 = db.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version != SCHEMA_VERSION {
        for table in TABLES {
            db.execute_batch(&format!("DROP TABLE IF EXISTS {table}"))?;
        }
        db.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    }
    db.pragma_update(None, "foreign_keys", "ON")?;
    db.execute_batch(SCHEMA)?;
    Ok(db)
}

/// The facts and messages of JSONL `bytes`, each message with the byte range of its line.
fn read_lines(
    parser: &dyn Parser,
    session: &Identity,
    bytes: &[u8],
) -> (Facts, Vec<(Message, Range<u64>)>) {
    let mut facts = Facts::default();
    let mut messages = Vec::new();
    let mut at = 0;
    for line in bytes.split(|b| *b == b'\n') {
        let len = line.len() as u64;
        if len > 0 {
            let parsed = parser.line(session, line);
            facts = facts.then(parsed.facts);
            if let Some(message) = parsed.message {
                messages.push((message, at..at + len));
            }
        }
        at += len + 1;
    }
    (facts, messages)
}

fn index_lines(
    tx: &Transaction,
    source: Source,
    key: &SessionKey,
    facts: &Facts,
    messages: &[(Message, MessageRef)],
    mut touched: Vec<String>,
) -> Result<usize> {
    let mut position: i64 = tx.query_row(
        "SELECT COALESCE(MAX(position) + 1, 0) FROM messages WHERE session_key = ?1",
        [key.as_str()],
        |row| row.get(0),
    )?;
    let mut inserted = Vec::new();
    for (message, raw) in messages {
        if insert_if_new(tx, key, position, message, raw)? {
            position += 1;
            inserted.push(message.id.clone());
        }
    }
    let added = inserted.len();
    if apply_facts(tx, key, facts)? {
        let mut all = tx.prepare("SELECT message_id FROM messages WHERE session_key = ?1")?;
        inserted = all
            .query_map([key.as_str()], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
    }
    touched.extend(inserted);
    attribute_copies(tx, source, key, &touched)?;
    tx.execute(
        "UPDATE sessions SET (last_activity, message_count) =
            (SELECT MAX(at), COUNT(*) FROM messages WHERE session_key = ?1)
         WHERE key = ?1",
        [key.as_str()],
    )?;
    Ok(added)
}

fn ensure_session(
    tx: &Transaction,
    stream: &Stream,
    identity: &Identity,
    facts: &Facts,
) -> Result<SessionKey> {
    let source = stream.key.source;
    let key = SessionKey::new(source, &identity.native_id);
    let (kind, parent) = match facts.lineage.as_ref().unwrap_or(&identity.lineage) {
        Lineage::Main => ("main", None),
        Lineage::Subagent { parent_native_id } => {
            ("subagent", Some(SessionKey::new(source, parent_native_id)))
        }
    };
    tx.execute(
        "INSERT INTO sessions (key, source, native_id, ran_on, kind, parent_key)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6) ON CONFLICT (key) DO NOTHING",
        params![
            key.as_str(),
            source.as_str(),
            identity.native_id,
            stream.key.origin,
            kind,
            parent.as_ref().map(SessionKey::as_str),
        ],
    )?;
    for alias in &identity.aliases {
        tx.execute(
            "INSERT OR IGNORE INTO aliases (value, kind, session_key) VALUES (?1, ?2, ?3)",
            params![alias.value, alias.kind.as_str(), key.as_str()],
        )?;
    }
    Ok(key)
}

/// Records `facts` on the session. Returns whether this moved the session's start, which can change who owns its messages.
fn apply_facts(tx: &Transaction, key: &SessionKey, facts: &Facts) -> Result<bool> {
    if let Some(title) = &facts.title {
        tx.execute(
            "UPDATE sessions SET title = ?2 WHERE key = ?1 AND title IS NOT ?2",
            params![key.as_str(), title],
        )?;
    }
    if let Some(title) = &facts.fallback_title {
        tx.execute(
            "UPDATE sessions SET title = ?2 WHERE key = ?1 AND title IS NULL",
            params![key.as_str(), title],
        )?;
    }
    if let Some(project) = &facts.project {
        tx.execute(
            "UPDATE sessions SET project = ?2 WHERE key = ?1 AND project IS NULL",
            params![key.as_str(), project],
        )?;
    }
    let started = match facts.started {
        Some(at) => {
            tx.execute(
                "UPDATE sessions SET started_at = ?2
                 WHERE key = ?1 AND (started_at IS NULL OR started_at > ?2)",
                params![key.as_str(), at.0],
            )? > 0
        }
        None => false,
    };
    Ok(started)
}

/// A message ID that several sessions hold is one message, copied when Claude Code forks or resumes a session.
/// The session that started first owns it, and every other holder marks its copy with the owner, which keeps
/// the copy readable but out of search. A main session that holds copies is a fork of the session it copied last.
fn attribute_copies(
    tx: &Transaction,
    source: Source,
    key: &SessionKey,
    message_ids: &[String],
) -> Result<()> {
    let mut holders = tx.prepare_cached(
        "SELECT m.session_key, m.copied_from FROM messages m JOIN sessions s ON s.key = m.session_key
         WHERE m.message_id = ?1 AND s.source = ?2
         ORDER BY s.started_at IS NULL, s.started_at, s.key",
    )?;
    let mut mark = tx.prepare_cached(
        "UPDATE messages SET copied_from = ?3 WHERE message_id = ?1 AND session_key = ?2",
    )?;
    let mut changed: HashSet<String> = HashSet::from([key.as_str().to_owned()]);
    for id in message_ids {
        let rows = holders
            .query_map(params![id, source.as_str()], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let Some((owner, _)) = rows.first() else {
            continue;
        };
        for (session, copied_from) in &rows {
            let origin = (session != owner).then_some(owner);
            if copied_from.as_ref() != origin {
                mark.execute(params![id, session, origin])?;
                changed.insert(session.clone());
            }
        }
    }
    let mut lineage = tx.prepare_cached(
        "UPDATE sessions SET (kind, parent_key) = (
            SELECT CASE WHEN origin IS NULL THEN 'main' ELSE 'fork' END, origin FROM (
                SELECT (SELECT copied_from FROM messages WHERE session_key = ?1 AND copied_from IS NOT NULL
                        ORDER BY position DESC LIMIT 1) AS origin))
         WHERE key = ?1 AND kind <> 'subagent'",
    )?;
    let mut own = tx.prepare_cached(
        "UPDATE sessions SET own_message_count =
            (SELECT COUNT(*) FROM messages WHERE session_key = ?1 AND copied_from IS NULL)
         WHERE key = ?1",
    )?;
    for session in &changed {
        lineage.execute([session])?;
        own.execute([session])?;
    }
    Ok(())
}

fn insert_if_new(
    tx: &Transaction,
    key: &SessionKey,
    position: i64,
    message: &Message,
    raw: &MessageRef,
) -> Result<bool> {
    let (stream, generation, version, start, end) = match *raw {
        MessageRef::Log(raw) => (
            raw.stream,
            Some(raw.generation as i64),
            None,
            raw.start,
            raw.end,
        ),
        MessageRef::Version {
            stream,
            sha256,
            decoded_start,
            decoded_end,
        } => (
            stream,
            None,
            Some(sha256.to_hex()),
            decoded_start,
            decoded_end,
        ),
    };
    let id: Option<i64> = tx
        .query_row(
            "INSERT INTO messages (session_key, message_id, parent_id, position, role, at,
                searchable_text, raw_stream, raw_generation, raw_version, raw_start, raw_end)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT (session_key, message_id) DO NOTHING
             RETURNING id",
            params![
                key.as_str(),
                message.id,
                message.parent_id,
                position,
                message.role.as_str(),
                message.at.map(|at| at.0),
                message.searchable_text,
                stream.0,
                generation,
                version,
                start as i64,
                end as i64,
            ],
            |row| row.get(0),
        )
        .optional()?;
    let Some(id) = id else {
        return Ok(false);
    };
    for artifact in &message.artifacts {
        tx.execute(
            "INSERT OR IGNORE INTO artifacts (session_key, kind, value, message) VALUES (?1, ?2, ?3, ?4)",
            params![key.as_str(), artifact.kind.as_str(), artifact.value, id],
        )?;
    }
    for media in &message.media {
        tx.execute(
            "INSERT OR IGNORE INTO media (message, hash, mime, bytes) VALUES (?1, ?2, ?3, ?4)",
            params![id, media.hash.to_hex(), media.mime, media.bytes as i64],
        )?;
    }
    Ok(true)
}
