use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use prime_radiant::collect::import_claude_code;
use prime_radiant::index::{Indexer, SyncSummary};
use prime_radiant::raw::{RawRef, Sha256, Store, StreamId, View};
use rusqlite::Connection;
use serde_json::Value;
use tempfile::TempDir;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/claude-code");
const ORIGIN: &str = "example-origin";
const SESSION_ID: &str = "33831ab7-5c2a-48a4-a8d7-0b5125040f4d";
const AGENT_ID: &str = "a75ce3543dd257a2d";
const MAIN_KEY: &str = "claude-code/33831ab7-5c2a-48a4-a8d7-0b5125040f4d";
const SUBAGENT_KEY: &str =
    "claude-code/33831ab7-5c2a-48a4-a8d7-0b5125040f4d/agent-a75ce3543dd257a2d";
const COUNTED: [&str; 6] = [
    "sessions",
    "messages",
    "aliases",
    "artifacts",
    "media",
    "progress",
];

struct Setup {
    _temp: TempDir,
    root: PathBuf,
    data: PathBuf,
}

impl Setup {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("temp directory is created");
        let root = temp.path().join("projects");
        copy_dir(Path::new(FIXTURES), &root);
        let data = temp.path().join("data");
        Self {
            _temp: temp,
            root,
            data,
        }
    }

    fn main_transcript(&self) -> PathBuf {
        self.root
            .join("-home-dev-example-project")
            .join(format!("{SESSION_ID}.jsonl"))
    }

    fn subagent_transcript(&self) -> PathBuf {
        self.root
            .join("-home-dev-example-project")
            .join(SESSION_ID)
            .join("subagents")
            .join(format!("agent-{AGENT_ID}.jsonl"))
    }

    fn import(&self) {
        let mut store = Store::open(&self.data).expect("store opens");
        let summary = import_claude_code(&self.root, ORIGIN, &mut store).expect("import runs");
        assert!(summary.rejected.is_empty(), "no write is rejected");
    }

    fn sync(&self) -> SyncSummary {
        Indexer::open(&self.data)
            .expect("indexer opens")
            .sync()
            .expect("sync runs")
    }

    fn index(&self) -> Connection {
        Connection::open(self.data.join("index.db")).expect("index.db opens")
    }

    fn counts(&self) -> Vec<i64> {
        let db = self.index();
        COUNTED
            .iter()
            .map(|table| {
                db.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .expect("count query runs")
            })
            .collect()
    }

    fn messages(&self, session: &str) -> Vec<IndexedMessage> {
        let db = self.index();
        let mut query = db
            .prepare(
                "SELECT position, message_id, role, searchable_text,
                    raw_stream, raw_generation, raw_start, raw_end
                 FROM messages WHERE session_key = ?1 ORDER BY position",
            )
            .expect("message query prepares");
        query
            .query_map([session], |row| {
                Ok(IndexedMessage {
                    position: row.get(0)?,
                    id: row.get(1)?,
                    role: row.get(2)?,
                    text: row.get(3)?,
                    raw: RawRef {
                        stream: StreamId(row.get(4)?),
                        generation: row.get::<_, i64>(5)? as u64,
                        start: row.get::<_, i64>(6)? as u64,
                        end: row.get::<_, i64>(7)? as u64,
                    },
                })
            })
            .expect("message query runs")
            .collect::<rusqlite::Result<_>>()
            .expect("message rows decode")
    }

    fn strings(&self, sql: &str, arg: &str) -> Vec<String> {
        let db = self.index();
        let mut query = db.prepare(sql).expect("query prepares");
        query
            .query_map([arg], |row| row.get(0))
            .expect("query runs")
            .collect::<rusqlite::Result<_>>()
            .expect("rows decode")
    }

    fn search(&self, words: &str) -> Vec<String> {
        self.strings(
            "SELECT DISTINCT session_key FROM message_search WHERE message_search MATCH ?1 ORDER BY session_key",
            words,
        )
    }
}

#[derive(Debug)]
struct IndexedMessage {
    position: i64,
    id: String,
    role: String,
    text: String,
    raw: RawRef,
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("destination directory is created");
    for entry in fs::read_dir(from).expect("fixture directory is readable") {
        let path = entry.expect("directory entry is readable").path();
        let target = to.join(path.file_name().expect("entry has a name"));
        if path.is_dir() {
            copy_dir(&path, &target);
        } else {
            fs::copy(&path, &target).expect("fixture file is copied");
        }
    }
}

fn append(path: &Path, bytes: &[u8]) {
    OpenOptions::new()
        .append(true)
        .open(path)
        .expect("transcript opens for append")
        .write_all(bytes)
        .expect("bytes are appended");
}

fn user_line(uuid: &str, parent: &str, text: &str) -> String {
    let line = serde_json::json!({
        "parentUuid": parent,
        "isSidechain": false,
        "cwd": "/home/dev/example-project",
        "sessionId": SESSION_ID,
        "type": "user",
        "message": {"role": "user", "content": text},
        "uuid": uuid,
        "timestamp": "2026-01-15T10:02:00.000Z",
    });
    format!("{line}\n")
}

fn lines_by_uuid(path: &Path) -> (Vec<String>, HashMap<String, Vec<u8>>) {
    let bytes = fs::read(path).expect("transcript is readable");
    let mut order = Vec::new();
    let mut lines = HashMap::new();
    for line in bytes.split(|b| *b == b'\n').filter(|line| !line.is_empty()) {
        let json: Value = serde_json::from_slice(line).expect("line is JSON");
        if let Some(uuid) = json["uuid"].as_str() {
            order.push(uuid.to_owned());
            lines.insert(uuid.to_owned(), line.to_vec());
        }
    }
    (order, lines)
}

#[test]
fn fixture_yields_a_titled_main_session_and_its_subagent() {
    let setup = Setup::new();
    setup.import();
    setup.sync();

    type SessionRow = (
        String,
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        i64,
        Option<i64>,
        Option<i64>,
    );
    let db = setup.index();
    let mut query = db
        .prepare(
            "SELECT key, kind, ran_on, title, project, parent_key, message_count, started_at, last_activity
             FROM sessions ORDER BY key",
        )
        .expect("session query prepares");
    let sessions: Vec<SessionRow> = query
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
                row.get(8)?,
            ))
        })
        .expect("session query runs")
        .collect::<rusqlite::Result<_>>()
        .expect("session rows decode");

    let project = Some("/home/dev/example-project".to_owned());
    assert_eq!(
        sessions,
        vec![
            (
                MAIN_KEY.to_owned(),
                "main".to_owned(),
                ORIGIN.to_owned(),
                Some("Add a JSON output flag".to_owned()),
                project.clone(),
                None,
                10,
                Some(1_768_471_200_000),
                Some(1_768_471_263_331),
            ),
            (
                SUBAGENT_KEY.to_owned(),
                "subagent".to_owned(),
                ORIGIN.to_owned(),
                Some("Find the list rendering code".to_owned()),
                project,
                Some(MAIN_KEY.to_owned()),
                6,
                Some(1_768_471_270_590),
                Some(1_768_471_305_885),
            ),
        ],
        "one main session titled by its custom title, and one subagent whose parent is the main session"
    );

    let aliases = setup.strings(
        "SELECT value || ' ' || kind FROM aliases WHERE session_key = ?1 ORDER BY value",
        SUBAGENT_KEY,
    );
    assert_eq!(
        aliases,
        [
            format!("{SESSION_ID}/agent-{AGENT_ID} native_id"),
            format!("{AGENT_ID} agent_id"),
        ],
        "the subagent is found by its native ID and its agent ID"
    );
    assert_eq!(
        setup.strings(
            "SELECT session_key FROM aliases WHERE value = ?1",
            SESSION_ID
        ),
        [MAIN_KEY],
        "the session ID is an alias of the main session"
    );
    assert_eq!(
        setup.strings(
            "SELECT kind || ' ' || value FROM artifacts WHERE session_key = ?1",
            MAIN_KEY
        ),
        ["changed_file /home/dev/example-project/src/cli.rs"],
        "the Edit tool call records the changed file"
    );
}

#[test]
fn positions_are_contiguous_and_raw_refs_read_back_their_source_lines() {
    let setup = Setup::new();
    setup.import();
    setup.sync();
    let raw = View::open(&setup.data).expect("raw view opens");

    for (session, path) in [
        (MAIN_KEY, setup.main_transcript()),
        (SUBAGENT_KEY, setup.subagent_transcript()),
    ] {
        let messages = setup.messages(session);
        let (order, lines) = lines_by_uuid(&path);
        assert_eq!(
            messages.iter().map(|m| m.position).collect::<Vec<_>>(),
            (0..order.len() as i64).collect::<Vec<_>>(),
            "{session} positions run from 0 with no gaps"
        );
        assert_eq!(
            messages.iter().map(|m| m.id.clone()).collect::<Vec<_>>(),
            order,
            "{session} positions follow the transcript order"
        );
        for message in &messages {
            assert_eq!(
                raw.read(&message.raw).expect("raw ref reads"),
                lines[&message.id],
                "{session} message {} reads back exactly its source line",
                message.position
            );
        }
    }

    let roles: Vec<String> = setup
        .messages(MAIN_KEY)
        .into_iter()
        .map(|m| m.role)
        .collect();
    assert_eq!(
        roles,
        [
            "user",
            "assistant",
            "tool_result",
            "tool_call",
            "attachment",
            "tool_result",
            "tool_call",
            "tool_result",
            "user",
            "assistant",
        ],
        "each line gets the role of what it holds"
    );
}

#[test]
fn full_text_search_finds_a_word_only_in_a_tool_result_preview() {
    let setup = Setup::new();
    let word = "unoptimized";
    let transcripts = [setup.main_transcript(), setup.subagent_transcript()]
        .map(|path| fs::read_to_string(path).expect("transcript is readable"));
    let lines_with_word: Vec<&str> = transcripts
        .iter()
        .flat_map(|text| text.lines())
        .filter(|line| line.contains(word))
        .collect();
    assert_eq!(lines_with_word.len(), 1, "one transcript line has the word");
    assert!(
        lines_with_word[0].contains("<persisted-output>"),
        "the word is in the persisted-output preview"
    );

    setup.import();
    setup.sync();

    assert_eq!(
        setup.search(word),
        [MAIN_KEY],
        "the preview word finds the main session"
    );
    assert_eq!(
        setup.strings(
            "SELECT key FROM title_search WHERE title_search MATCH ?1",
            "json"
        ),
        [MAIN_KEY],
        "a title word finds the session"
    );
}

#[test]
fn second_sync_changes_no_count() {
    let setup = Setup::new();
    setup.import();
    let first = setup.sync();
    assert!(
        first.messages_indexed > 0,
        "the first sync indexes messages"
    );
    let counts = setup.counts();

    let store = Store::open(&setup.data).expect("store stays open beside the indexer");
    let second = setup.sync();
    drop(store);

    assert_eq!(
        second,
        SyncSummary::default(),
        "a second sync syncs no stream"
    );
    assert_eq!(setup.counts(), counts, "no table count changes");
}

#[test]
fn appended_line_indexes_exactly_one_more_message() {
    let setup = Setup::new();
    setup.import();
    setup.sync();
    let counts = setup.counts();
    let (order, _) = lines_by_uuid(&setup.main_transcript());
    let pull_request = "https://github.com/example/todo-cli/pull/12";
    append(
        &setup.main_transcript(),
        user_line(
            "4b1d2a6e-0000-4000-8000-000000000001",
            order.last().expect("transcript has messages"),
            &format!("Opened {pull_request} for review."),
        )
        .as_bytes(),
    );

    setup.import();
    let summary = setup.sync();

    assert_eq!(summary.messages_indexed, 1, "one message is indexed");
    let mut expected = counts.clone();
    expected[1] += 1;
    expected[3] += 1;
    assert_eq!(
        setup.counts(),
        expected,
        "one more message and one more artifact"
    );
    let messages = setup.messages(MAIN_KEY);
    assert_eq!(
        messages.last().map(|m| m.position),
        Some(10),
        "the new message takes the next position"
    );
    assert_eq!(
        setup.strings(
            "SELECT value FROM artifacts WHERE session_key = ?1 AND kind = 'pull_request'",
            MAIN_KEY
        ),
        [pull_request],
        "the pull request URL becomes an artifact"
    );
}

#[test]
fn trailing_line_without_newline_waits_until_completed() {
    let setup = Setup::new();
    setup.import();
    setup.sync();
    let before = setup.messages(MAIN_KEY).len();
    let line = user_line(
        "4b1d2a6e-0000-4000-8000-000000000002",
        "1ba1c71c-5c5a-4d58-aa20-8f86f83a4023",
        "Half written",
    );
    let (head, tail) = line.split_at(line.len() / 2);

    append(&setup.main_transcript(), head.as_bytes());
    setup.import();
    let partial = setup.sync();
    assert_eq!(
        partial.messages_indexed, 0,
        "a line without a newline is not indexed"
    );
    assert_eq!(setup.messages(MAIN_KEY).len(), before);

    append(&setup.main_transcript(), tail.as_bytes());
    setup.import();
    let completed = setup.sync();
    assert_eq!(
        completed.messages_indexed, 1,
        "the completed line is indexed"
    );
    let messages = setup.messages(MAIN_KEY);
    let raw = View::open(&setup.data).expect("raw view opens");
    assert_eq!(
        raw.read(&messages.last().expect("a message").raw)
            .expect("raw ref reads"),
        line.trim_end().as_bytes(),
        "the raw ref covers the whole completed line"
    );
}

#[test]
fn rewritten_transcript_is_reindexed_from_its_new_generation() {
    let setup = Setup::new();
    setup.import();
    setup.sync();
    let path = setup.main_transcript();
    let mut rewritten = b"{\"type\":\"summary\",\"summary\":\"compacted\"}\n".to_vec();
    rewritten.extend(fs::read(&path).expect("transcript is readable"));
    fs::write(&path, rewritten).expect("transcript is rewritten");

    setup.import();
    setup.sync();

    let messages = setup.messages(MAIN_KEY);
    assert_eq!(
        messages.iter().map(|m| m.position).collect::<Vec<_>>(),
        (0..10).collect::<Vec<_>>(),
        "the session holds each message once, at positions 0 to 9"
    );
    let raw = View::open(&setup.data).expect("raw view opens");
    let (_, lines) = lines_by_uuid(&path);
    for message in &messages {
        assert_eq!(
            message.raw.generation, 1,
            "messages point at the new generation"
        );
        assert_eq!(
            raw.read(&message.raw).expect("raw ref reads"),
            lines[&message.id]
        );
    }
}

#[test]
fn changed_title_file_retitles_the_session() {
    let setup = Setup::new();
    setup.import();
    setup.sync();
    let title_file = setup
        .root
        .join("-home-dev-example-project")
        .join(SESSION_ID)
        .join("custom-title.json");
    fs::write(&title_file, "{\"customTitle\": \"Add CSV export\"}").expect("title is rewritten");

    setup.import();
    setup.sync();

    assert_eq!(
        setup.strings("SELECT title FROM sessions WHERE key = ?1", MAIN_KEY),
        ["Add CSV export"],
        "the latest title wins"
    );
    assert_eq!(
        setup.strings(
            "SELECT key FROM title_search WHERE title_search MATCH ?1",
            "csv"
        ),
        [MAIN_KEY],
        "the new title is searchable"
    );
    assert!(
        setup
            .strings(
                "SELECT key FROM title_search WHERE title_search MATCH ?1",
                "json"
            )
            .is_empty(),
        "the old title is not"
    );
}

#[test]
fn image_becomes_one_media_row_and_no_base64_reaches_search() {
    let setup = Setup::new();
    let transcript = fs::read_to_string(setup.main_transcript()).expect("transcript is readable");
    let image = transcript
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("line is JSON"))
        .flat_map(|line| {
            line["message"]["content"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .find(|block| block["type"] == "image")
        .expect("the transcript has an image");
    let data = image["source"]["data"].as_str().expect("image data");
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(data)
        .expect("image data is base64");

    setup.import();
    setup.sync();

    let db = setup.index();
    let mut query = db
        .prepare("SELECT hash, mime, bytes FROM media")
        .expect("media query prepares");
    let media: Vec<(String, String, i64)> = query
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .expect("media query runs")
        .collect::<rusqlite::Result<_>>()
        .expect("media rows decode");
    assert_eq!(
        media,
        [(
            Sha256::of(&decoded).to_hex(),
            "image/png".to_owned(),
            decoded.len() as i64
        )],
        "the image is one media row keyed by the hash of its decoded bytes"
    );
    for session in [MAIN_KEY, SUBAGENT_KEY] {
        for message in setup.messages(session) {
            assert!(
                !message.text.contains(&data[..16]),
                "message {} has no base64 text",
                message.position
            );
        }
    }
}

#[test]
fn secret_in_a_transcript_is_redacted_from_searchable_text() {
    let setup = Setup::new();
    setup.import();
    setup.sync();
    let token = ["gh", "p_", &"k3Xq".repeat(9)].concat();
    let uuid = "4b1d2a6e-0000-4000-8000-000000000003";
    append(
        &setup.main_transcript(),
        user_line(
            uuid,
            "1ba1c71c-5c5a-4d58-aa20-8f86f83a4023",
            &format!("Use GH_TOKEN={token} and also {token} directly."),
        )
        .as_bytes(),
    );

    setup.import();
    setup.sync();

    let message = setup
        .messages(MAIN_KEY)
        .into_iter()
        .find(|m| m.id == uuid)
        .expect("the appended message is indexed");
    assert!(
        !message.text.contains(&token),
        "the token is absent from searchable text"
    );
    assert_eq!(
        message.text, "Use GH_TOKEN=[redacted] and also [redacted] directly.",
        "each occurrence is replaced by [redacted]"
    );
    assert!(
        setup.search(&format!("\"{}\"", &token[4..])).is_empty(),
        "full-text search cannot find the token"
    );
}
