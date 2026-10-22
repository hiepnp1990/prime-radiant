use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use prime_radiant::collect::{ImportSummary, import_claude_code, import_t3_code};
use prime_radiant::index::{Indexer, SyncSummary, Timestamp};
use prime_radiant::query::Query;
use prime_radiant::raw::{Progress, Store, View};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use tempfile::TempDir;

const ORIGIN: &str = "example-origin";
const CLAUDE_FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/claude-code");
const CLAUDE_SESSION: &str = "33831ab7-5c2a-48a4-a8d7-0b5125040f4d";
const PROJECT: &str = "7f1c2b9e-0a4d-4e8b-9c61-3d5e8f2a1b70";
const THREAD_A: &str = "0d5c1e4a-6a0b-4c8e-9f3e-2b7a1c9d4e01";
const THREAD_B: &str = "5b8e2f71-9c3d-4a6e-8b10-4f7d2c6a9e02";
const KEY_A: &str = "t3-code/0d5c1e4a-6a0b-4c8e-9f3e-2b7a1c9d4e01";
const KEY_B: &str = "t3-code/5b8e2f71-9c3d-4a6e-8b10-4f7d2c6a9e02";
const NOW: Timestamp = Timestamp(1_800_000_000_000);

/// The columns of the T3 Code tables the collector reads, as T3 Code creates them.
const SCHEMA: &str = "
CREATE TABLE projection_projects (
    project_id TEXT PRIMARY KEY, title TEXT NOT NULL, workspace_root TEXT NOT NULL,
    scripts_json TEXT NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL, deleted_at TEXT,
    default_model_selection_json TEXT, default_thread_env_mode TEXT, favicon_path TEXT,
    auto_pull INTEGER NOT NULL DEFAULT 0, project_icon_json TEXT
);
CREATE TABLE orchestration_v2_projection_threads (
    thread_id TEXT PRIMARY KEY, project_id TEXT NOT NULL, title TEXT NOT NULL,
    default_provider TEXT NOT NULL, runtime_mode TEXT NOT NULL, interaction_mode TEXT NOT NULL,
    active_provider_thread_id TEXT, created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
    archived_at TEXT, deleted_at TEXT, payload_json TEXT NOT NULL, provider_instance_id TEXT
);
CREATE TABLE orchestration_v2_projection_messages (
    message_id TEXT PRIMARY KEY, thread_id TEXT NOT NULL, run_id TEXT, node_id TEXT,
    role TEXT NOT NULL, streaming INTEGER NOT NULL, created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL, payload_json TEXT NOT NULL
);
CREATE TABLE orchestration_v2_projection_provider_threads (
    provider_thread_id TEXT PRIMARY KEY, thread_id TEXT, owner_node_id TEXT, provider TEXT NOT NULL,
    provider_session_id TEXT, status TEXT NOT NULL, first_run_ordinal INTEGER,
    last_run_ordinal INTEGER, updated_at TEXT NOT NULL, payload_json TEXT NOT NULL, driver TEXT,
    provider_instance_id TEXT
);
";

/// A made-up T3 Code database, held open by a writer the way a running T3 Code holds it.
struct T3 {
    temp: TempDir,
    db_path: PathBuf,
    data: PathBuf,
    writer: Connection,
}

impl T3 {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("temp directory is created");
        let db_path = temp.path().join("statev2.sqlite");
        let writer = Connection::open(&db_path).expect("example database is created");
        writer
            .pragma_update(None, "journal_mode", "WAL")
            .expect("example database uses a write-ahead log");
        writer.execute_batch(SCHEMA).expect("schema is created");
        writer
            .execute(
                "INSERT INTO projection_projects (project_id, title, workspace_root, scripts_json, created_at, updated_at)
                 VALUES (?1, 'example-project', '/home/dev/example-project', '[]', ?2, ?2)",
                params![PROJECT, "2026-01-15T09:00:00.000Z"],
            )
            .expect("project is inserted");
        let setup = Self {
            data: temp.path().join("data"),
            temp,
            db_path,
            writer,
        };
        setup.thread(THREAD_A, "Add a JSON output flag", "claudeAgent", None);
        setup.thread(
            THREAD_B,
            "Draft the release notes",
            "codex",
            Some("2026-01-16T12:00:00.000Z"),
        );
        // Inserted out of order: the snapshot orders messages by creation time.
        setup.message(
            THREAD_A,
            "a-2",
            "assistant",
            "2026-01-15T10:00:30.000Z",
            "I'll add the flag to `todo list`.",
        );
        setup.message(
            THREAD_A,
            "a-1",
            "user",
            "2026-01-15T10:00:05.000Z",
            "Add a --json flag to `todo list`.",
        );
        setup.message(
            THREAD_A,
            "a-3",
            "user",
            "2026-01-15T10:01:00.000Z",
            "The test database uses DB_PASSWORD=hunter2 locally.",
        );
        setup.message(
            THREAD_B,
            "b-1",
            "user",
            "2026-01-16T11:00:00.000Z",
            "Draft release notes for the changelog.",
        );
        setup.message(
            THREAD_B,
            "b-2",
            "assistant",
            "2026-01-16T11:00:20.000Z",
            "Here is a first draft of the release notes.",
        );
        setup.provider_thread(
            "provider-thread:example-a",
            Some(THREAD_A),
            "claudeAgent",
            CLAUDE_SESSION,
        );
        setup.provider_thread(
            "provider-thread:example-orphan",
            None,
            "claudeAgent",
            "9e4a1f0c-2b7d-4c8e-a6f3-1d5b9c7e2a03",
        );
        setup
    }

    fn thread(&self, id: &str, title: &str, provider: &str, closed_at: Option<&str>) {
        let created = "2026-01-15T10:00:00.000Z";
        let payload = json!({
            "id": id, "projectId": PROJECT, "title": title, "createdAt": created,
            "archivedAt": closed_at, "deletedAt": closed_at, "lineage": {"rootThreadId": id},
        });
        self.writer
            .execute(
                "INSERT INTO orchestration_v2_projection_threads (thread_id, project_id, title, default_provider,
                    runtime_mode, interaction_mode, created_at, updated_at, archived_at, deleted_at, payload_json)
                 VALUES (?1, ?2, ?3, ?4, 'full-access', 'default', ?5, ?5, ?6, ?6, ?7)",
                params![id, PROJECT, title, provider, created, closed_at, payload.to_string()],
            )
            .expect("thread is inserted");
    }

    fn message(&self, thread: &str, id: &str, role: &str, at: &str, text: &str) {
        let payload = json!({
            "id": id, "threadId": thread, "role": role, "text": text, "attachments": [],
            "streaming": false, "createdAt": at, "updatedAt": at,
        });
        self.writer
            .execute(
                "INSERT INTO orchestration_v2_projection_messages (message_id, thread_id, role, streaming,
                    created_at, updated_at, payload_json)
                 VALUES (?1, ?2, ?3, 0, ?4, ?4, ?5)",
                params![id, thread, role, at, payload.to_string()],
            )
            .expect("message is inserted");
    }

    fn provider_thread(&self, id: &str, thread: Option<&str>, driver: &str, native_id: &str) {
        let payload = json!({
            "id": id, "appThreadId": thread, "driver": driver,
            "nativeThreadRef": {"driver": driver, "nativeId": native_id, "strength": "strong"},
        });
        self.writer
            .execute(
                "INSERT INTO orchestration_v2_projection_provider_threads (provider_thread_id, thread_id, provider,
                    status, updated_at, payload_json, driver)
                 VALUES (?1, ?2, ?3, 'idle', '2026-01-15T10:00:00.000Z', ?4, ?3)",
                params![id, thread, driver, payload.to_string()],
            )
            .expect("provider thread is inserted");
    }

    fn import(&self) -> ImportSummary {
        let mut store = Store::open(&self.data).expect("store opens");
        let summary = import_t3_code(&self.db_path, ORIGIN, &mut store).expect("import runs");
        assert!(summary.rejected.is_empty(), "no write is rejected");
        summary
    }

    fn import_claude_code(&self) {
        let mut store = Store::open(&self.data).expect("store opens");
        import_claude_code(Path::new(CLAUDE_FIXTURES), ORIGIN, &mut store).expect("import runs");
    }

    fn sync(&self) -> SyncSummary {
        Indexer::open(&self.data)
            .expect("indexer opens")
            .sync()
            .expect("sync runs")
    }

    fn brief(&self, session: &str) -> Value {
        let brief = Query::open(&self.data, NOW)
            .expect("query opens")
            .brief(session)
            .expect("brief is built");
        serde_json::to_value(brief).expect("brief serializes")
    }

    fn index(&self) -> Connection {
        Connection::open(self.data.join("index.db")).expect("index.db opens")
    }

    /// `(message_id, role, searchable_text)` of a session's messages, in order.
    fn messages(&self, session: &str) -> Vec<(String, String, String)> {
        let db = self.index();
        let mut query = db
            .prepare("SELECT message_id, role, searchable_text FROM messages WHERE session_key = ?1 ORDER BY position")
            .expect("message query prepares");
        query
            .query_map([session], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .expect("message query runs")
            .collect::<rusqlite::Result<_>>()
            .expect("message rows decode")
    }

    /// Bytes and modification times of the database and its write-ahead log.
    fn files(&self) -> Vec<(Vec<u8>, SystemTime)> {
        ["statev2.sqlite", "statev2.sqlite-wal"]
            .iter()
            .map(|name| {
                let path = self.temp.path().join(name);
                let modified = fs::metadata(&path)
                    .and_then(|meta| meta.modified())
                    .expect("database file has a modification time");
                (
                    fs::read(&path).expect("database file is readable"),
                    modified,
                )
            })
            .collect()
    }
}

fn ids(messages: &[(String, String, String)]) -> Vec<&str> {
    messages.iter().map(|(id, _, _)| id.as_str()).collect()
}

#[test]
fn two_threads_become_two_sessions_with_their_messages_in_order() {
    let t3 = T3::new();
    let summary = t3.import();
    assert_eq!(summary.files_scanned, 2, "each thread is one stream");
    assert_eq!(summary.versions_stored, 2);
    t3.sync();

    let a = t3.messages(KEY_A);
    assert_eq!(ids(&a), ["a-1", "a-2", "a-3"]);
    assert_eq!(a[0].1, "user");
    assert_eq!(a[1].1, "assistant");
    assert_eq!(a[0].2, "Add a --json flag to `todo list`.");
    assert_eq!(
        a[2].2, "The test database uses DB_PASSWORD=[redacted] locally.",
        "message text has secrets removed"
    );
    assert_eq!(ids(&t3.messages(KEY_B)), ["b-1", "b-2"]);

    let brief = t3.brief(KEY_A);
    assert_eq!(brief["title"], "Add a JSON output flag");
    assert_eq!(brief["project"], "/home/dev/example-project");
    assert_eq!(brief["started"], "2026-01-15T10:00:00Z");
    assert_eq!(brief["last_activity"], "2026-01-15T10:01:00Z");
    assert_eq!(brief["first_request"], "Add a --json flag to `todo list`.");
    assert_eq!(brief["messages"]["total"], 3);
    assert_eq!(brief["archived_at"], Value::Null);
    assert_eq!(brief["deleted_at"], Value::Null);
    assert_eq!(t3.brief(KEY_B)["title"], "Draft the release notes");
}

#[test]
fn archived_and_deleted_threads_are_kept_and_marked() {
    let t3 = T3::new();
    t3.import();
    t3.sync();
    let brief = t3.brief(KEY_B);
    assert_eq!(brief["messages"]["total"], 2);
    assert_eq!(brief["archived_at"], "2026-01-16T12:00:00Z");
    assert_eq!(brief["deleted_at"], "2026-01-16T12:00:00Z");
}

#[test]
fn a_thread_id_resolves_to_its_session() {
    let t3 = T3::new();
    t3.import();
    t3.sync();
    let query = Query::open(&t3.data, NOW).expect("query opens");
    for (thread, key) in [(THREAD_A, KEY_A), (THREAD_B, KEY_B)] {
        let hits = query.find(thread, 10).expect("find runs");
        let keys: Vec<&str> = hits.iter().map(|hit| hit.key.as_str()).collect();
        assert_eq!(keys, [key], "{thread} resolves through its alias");
    }
}

#[test]
fn an_unchanged_database_stores_nothing_and_a_new_message_one_version() {
    let t3 = T3::new();
    t3.import();
    t3.sync();

    let again = t3.import();
    assert_eq!(
        again.writes_planned, 0,
        "an unchanged thread plans no write"
    );
    assert_eq!(again.versions_stored, 0);
    assert_eq!(
        t3.sync(),
        SyncSummary::default(),
        "the index has nothing new"
    );

    t3.message(
        THREAD_A,
        "a-4",
        "assistant",
        "2026-01-15T10:02:00.000Z",
        "Added the flag and a test.",
    );
    let changed = t3.import();
    assert_eq!(
        changed.versions_stored, 1,
        "only the changed thread is stored"
    );
    t3.sync();
    assert_eq!(ids(&t3.messages(KEY_A)), ["a-1", "a-2", "a-3", "a-4"]);
    assert_eq!(ids(&t3.messages(KEY_B)), ["b-1", "b-2"]);
    assert_eq!(t3.brief(KEY_A)["last_answer"], "Added the flag and a test.");
}

#[test]
fn each_message_points_at_its_row_in_the_latest_version() {
    let t3 = T3::new();
    t3.import();
    t3.sync();
    let raw = View::open(&t3.data).expect("raw layer opens");
    let stream = raw
        .streams()
        .expect("streams are listed")
        .into_iter()
        .find(|stream| stream.key.locator == format!("threads/{THREAD_A}.json"))
        .expect("thread A has a stream");
    let Progress::Replace { latest_checksum } = stream.state.progress else {
        panic!("a thread is a replace stream");
    };
    let version = raw.version(latest_checksum).expect("version is readable");

    let db = t3.index();
    let mut query = db
        .prepare(
            "SELECT message_id, raw_generation, raw_version, raw_start, raw_end FROM messages
             WHERE raw_stream = ?1 ORDER BY position",
        )
        .expect("query prepares");
    type RefRow = (String, Option<i64>, Option<String>, i64, i64);
    let refs: Vec<RefRow> = query
        .query_map([stream.id.0], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })
        .expect("query runs")
        .collect::<rusqlite::Result<_>>()
        .expect("rows decode");
    assert_eq!(refs.len(), 3);
    for (id, generation, named, start, end) in refs {
        assert_eq!(generation, None, "{id} points into a version, not a log");
        assert_eq!(
            named,
            Some(latest_checksum.to_hex()),
            "{id} names the latest version"
        );
        let row: Value = serde_json::from_slice(&version[start as usize..end as usize])
            .expect("range is one JSON row");
        assert_eq!(row["message_id"], id.as_str());
    }
}

#[test]
fn the_provider_session_is_linked_both_ways() {
    let t3 = T3::new();
    t3.import();
    t3.import_claude_code();
    t3.sync();
    let claude_key = format!("claude-code/{CLAUDE_SESSION}");
    assert_eq!(
        t3.brief(KEY_A)["underlying_sessions"],
        json!([claude_key]),
        "the T3 Code brief names the Claude Code session"
    );
    assert_eq!(
        t3.brief(&claude_key)["front_end_sessions"],
        json!([KEY_A]),
        "the Claude Code brief names the T3 Code thread"
    );
    assert_eq!(t3.brief(KEY_B)["underlying_sessions"], json!([]));
    let kinds: Vec<String> = {
        let db = t3.index();
        let mut query = db
            .prepare("SELECT kind FROM artifacts WHERE session_key = ?1")
            .expect("query prepares");
        query
            .query_map([KEY_A], |row| row.get(0))
            .expect("query runs")
            .collect::<rusqlite::Result<_>>()
            .expect("rows decode")
    };
    assert_eq!(kinds, ["underlying_session"]);
}

#[test]
fn importing_never_modifies_the_database() {
    let t3 = T3::new();
    let before = t3.files();
    assert!(
        !before[1].0.is_empty(),
        "the rows are still in the write-ahead log"
    );
    t3.import();
    t3.import();
    t3.sync();
    assert!(
        t3.files() == before,
        "the database and its write-ahead log keep their bytes and modification times"
    );
}

#[test]
fn an_index_built_for_an_older_schema_is_rebuilt() {
    let t3 = T3::new();
    t3.import_claude_code();
    t3.sync();
    t3.index()
        .execute_batch(
            "DROP TABLE artifacts;
             CREATE TABLE artifacts (
                 session_key TEXT NOT NULL REFERENCES sessions (key),
                 kind TEXT NOT NULL CHECK (kind IN ('pull_request', 'changed_file')),
                 value TEXT NOT NULL,
                 message INTEGER NOT NULL REFERENCES messages (id) ON DELETE CASCADE,
                 PRIMARY KEY (session_key, kind, value)
             );
             PRAGMA user_version = 0;",
        )
        .expect("index is set back to the older schema");

    t3.import();
    t3.sync();
    assert_eq!(
        t3.brief(KEY_A)["underlying_sessions"],
        json!([format!("claude-code/{CLAUDE_SESSION}")])
    );
    assert_eq!(
        t3.brief(CLAUDE_SESSION)["messages"]["total"],
        10,
        "the Claude Code session is indexed again"
    );
}

#[test]
fn import_takes_t3_code_beside_claude_code_and_the_brief_shows_its_links() {
    let t3 = T3::new();
    let run = |args: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_prime-radiant"))
            .args(args)
            .output()
            .expect("the built binary runs");
        assert!(output.status.success(), "{args:?} exits 0");
        String::from_utf8(output.stdout).expect("stdout is UTF-8")
    };
    let data = t3.data.to_str().expect("data path is UTF-8");
    let imported = run(&[
        "import",
        "--data",
        data,
        "--origin",
        ORIGIN,
        "--t3-code",
        t3.db_path.to_str().expect("database path is UTF-8"),
        "--claude-code",
        CLAUDE_FIXTURES,
    ]);
    assert!(
        imported.contains("t3-code: scanned 2 threads"),
        "{imported}"
    );

    let thread = run(&["brief", THREAD_A, "--data", data]);
    let claude = format!("claude-code/{CLAUDE_SESSION}");
    assert!(
        thread.contains(&format!("\nunderlying sessions:\n  {claude}\n")),
        "{thread}"
    );
    assert!(
        !thread.contains("archived"),
        "an open thread is not archived"
    );
    let underlying = run(&["brief", &claude, "--data", data]);
    assert!(
        underlying.contains(&format!("\nfront-end sessions:\n  {KEY_A}\n")),
        "{underlying}"
    );
    let closed = run(&["brief", THREAD_B, "--data", data]);
    assert!(
        closed.contains("\narchived 2026-01-16T12:00:00Z\n"),
        "{closed}"
    );
    assert!(
        closed.contains("\ndeleted 2026-01-16T12:00:00Z\n"),
        "{closed}"
    );
}
