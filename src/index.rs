pub mod claude_code;

use std::collections::HashMap;
use std::path::Path;

use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::raw::{Progress, RawRef, Sha256, Source, Stream, StreamId, View};
use crate::{Error, Result};

/// What one source's parser knows: which session a stream feeds, and what its bytes mean.
pub trait Parser {
    /// The session a stream belongs to, or `None` for a stream the parser does not read.
    fn session(&self, locator: &str) -> Option<Identity>;
    /// One complete line of an append stream, without its newline.
    fn line(&self, line: &[u8]) -> Line;
    /// The latest version of a replace stream.
    fn version(&self, bytes: &[u8]) -> Facts;
}

pub fn parser(source: Source) -> &'static dyn Parser {
    match source {
        Source::ClaudeCode => &claude_code::ClaudeCode,
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

/// Session attributes found in a stream. A title replaces the one before it, and the first project wins.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Facts {
    pub title: Option<String>,
    pub project: Option<String>,
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
}

impl ArtifactKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ArtifactKind::PullRequest => "pull_request",
            ArtifactKind::ChangedFile => "changed_file",
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
    kind TEXT NOT NULL CHECK (kind IN ('main', 'subagent')),
    parent_key TEXT,
    started_at INTEGER,
    last_activity INTEGER,
    message_count INTEGER NOT NULL DEFAULT 0,
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
    raw_generation INTEGER NOT NULL,
    raw_start INTEGER NOT NULL,
    raw_end INTEGER NOT NULL,
    UNIQUE (session_key, message_id),
    UNIQUE (session_key, position)
);
CREATE INDEX IF NOT EXISTS messages_by_stream ON messages (raw_stream);

CREATE TABLE IF NOT EXISTS aliases (
    value TEXT PRIMARY KEY,
    kind TEXT NOT NULL CHECK (kind IN ('native_id', 'agent_id')),
    session_key TEXT NOT NULL REFERENCES sessions (key)
);

CREATE TABLE IF NOT EXISTS artifacts (
    session_key TEXT NOT NULL REFERENCES sessions (key),
    kind TEXT NOT NULL CHECK (kind IN ('pull_request', 'changed_file')),
    value TEXT NOT NULL,
    message INTEGER NOT NULL REFERENCES messages (id) ON DELETE CASCADE,
    PRIMARY KEY (session_key, kind, value)
);
CREATE INDEX IF NOT EXISTS artifacts_by_message ON artifacts (message);

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
CREATE TRIGGER IF NOT EXISTS messages_search_insert AFTER INSERT ON messages BEGIN
    INSERT INTO message_search (rowid, searchable_text, session_key)
    VALUES (new.id, new.searchable_text, new.session_key);
END;
CREATE TRIGGER IF NOT EXISTS messages_search_delete AFTER DELETE ON messages BEGIN
    INSERT INTO message_search (message_search, rowid, searchable_text, session_key)
    VALUES ('delete', old.id, old.searchable_text, old.session_key);
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
";

/// The only writer of the session layer, in `index.db` beside the raw layer.
pub struct Indexer {
    raw: View,
    db: Connection,
}

impl Indexer {
    pub fn open(dir: &Path) -> Result<Self> {
        let raw = View::open(dir)?;
        let db = Connection::open(dir.join("index.db"))?;
        db.pragma_update_and_check(None, "journal_mode", "WAL", |_| Ok(()))?;
        db.pragma_update(None, "foreign_keys", "ON")?;
        db.execute_batch(SCHEMA)?;
        Ok(Self { raw, db })
    }

    /// Indexes whatever the raw layer holds past each stream's progress. Running it again changes nothing.
    pub fn sync(&mut self) -> Result<SyncSummary> {
        let indexed = self.indexed()?;
        let mut summary = SyncSummary::default();
        for stream in self.raw.streams()? {
            let parser = parser(stream.key.source);
            let Some(identity) = parser.session(&stream.key.locator) else {
                continue;
            };
            let before = indexed.get(&stream.id).copied();
            let added = match stream.state.progress {
                Progress::Append {
                    confirmed_offset, ..
                } => self.sync_append(parser, &stream, &identity, before, confirmed_offset)?,
                Progress::Replace { latest_checksum } => {
                    self.sync_replace(parser, &stream, &identity, before, latest_checksum)?
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
        identity: &Identity,
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

        let tx = self.db.transaction()?;
        let key = ensure_session(&tx, stream, identity)?;
        if restart {
            tx.execute("DELETE FROM messages WHERE raw_stream = ?1", [stream.id.0])?;
        }
        let mut position: i64 = tx.query_row(
            "SELECT COALESCE(MAX(position) + 1, 0) FROM messages WHERE session_key = ?1",
            [key.as_str()],
            |row| row.get(0),
        )?;
        let mut facts = Facts::default();
        let mut added = 0;
        let mut line_start = start;
        for line in bytes[..complete].split_inclusive(|b| *b == b'\n') {
            let raw = RawRef {
                stream: stream.id,
                generation,
                start: line_start,
                end: line_start + line.len() as u64 - 1,
            };
            line_start += line.len() as u64;
            let parsed = parser.line(&line[..line.len() - 1]);
            facts.title = parsed.facts.title.or(facts.title);
            facts.project = facts.project.or(parsed.facts.project);
            if let Some(message) = parsed.message
                && insert_if_new(&tx, &key, position, &message, &raw)?
            {
                position += 1;
                added += 1;
            }
        }
        apply_facts(&tx, &key, &facts)?;
        tx.execute(
            "UPDATE sessions SET (started_at, last_activity, message_count) =
                (SELECT MIN(at), MAX(at), COUNT(*) FROM messages WHERE session_key = ?1)
             WHERE key = ?1",
            [key.as_str()],
        )?;
        tx.execute(
            "INSERT INTO progress (stream, generation, indexed_offset) VALUES (?1, ?2, ?3)
             ON CONFLICT (stream) DO UPDATE SET generation = excluded.generation,
                indexed_offset = excluded.indexed_offset, indexed_checksum = NULL",
            params![stream.id.0, generation as i64, line_start as i64],
        )?;
        tx.commit()?;
        Ok(Some(added))
    }

    fn sync_replace(
        &mut self,
        parser: &dyn Parser,
        stream: &Stream,
        identity: &Identity,
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
        let facts = parser.version(&self.raw.version(latest_checksum)?);
        let tx = self.db.transaction()?;
        let key = ensure_session(&tx, stream, identity)?;
        apply_facts(&tx, &key, &facts)?;
        tx.execute(
            "INSERT INTO progress (stream, indexed_checksum) VALUES (?1, ?2)
             ON CONFLICT (stream) DO UPDATE SET generation = NULL, indexed_offset = NULL,
                indexed_checksum = excluded.indexed_checksum",
            params![stream.id.0, latest_checksum.to_hex()],
        )?;
        tx.commit()?;
        Ok(Some(0))
    }
}

fn ensure_session(tx: &Transaction, stream: &Stream, identity: &Identity) -> Result<SessionKey> {
    let source = stream.key.source;
    let key = SessionKey::new(source, &identity.native_id);
    let (kind, parent) = match &identity.lineage {
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

fn apply_facts(tx: &Transaction, key: &SessionKey, facts: &Facts) -> Result<()> {
    if let Some(title) = &facts.title {
        tx.execute(
            "UPDATE sessions SET title = ?2 WHERE key = ?1 AND title IS NOT ?2",
            params![key.as_str(), title],
        )?;
    }
    if let Some(project) = &facts.project {
        tx.execute(
            "UPDATE sessions SET project = ?2 WHERE key = ?1 AND project IS NULL",
            params![key.as_str(), project],
        )?;
    }
    Ok(())
}

fn insert_if_new(
    tx: &Transaction,
    key: &SessionKey,
    position: i64,
    message: &Message,
    raw: &RawRef,
) -> Result<bool> {
    let id: Option<i64> = tx
        .query_row(
            "INSERT INTO messages (session_key, message_id, parent_id, position, role, at,
                searchable_text, raw_stream, raw_generation, raw_start, raw_end)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
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
                raw.stream.0,
                raw.generation as i64,
                raw.start as i64,
                raw.end as i64,
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
