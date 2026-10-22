use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use prime_radiant::collect::{self, import_grok};
use prime_radiant::index::{Indexer, SyncSummary, Timestamp};
use prime_radiant::query::Query;
use prime_radiant::raw::{Store, WriteMode};
use rusqlite::Connection;
use tempfile::TempDir;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/grok");
const ORIGIN: &str = "example-origin";
const PROJECT_FOLDER: &str = "sessions/%2Fhome%2Fdev%2Fexample-project";
const OTHER_FOLDER: &str = "sessions/%2Fhome%2Fdev%2Fother%20project";
const NEW_ID: &str = "2b6f0d1e-8c3a-4e7b-9a51-6d2c8f4e1a03";
const OLD_ID: &str = "7c1e9a4b-3d2f-4b8e-a6c0-5e9d1f3b7a02";
const BOTH_ID: &str = "4a8d2c6e-1f3b-4c9a-8e7d-0b5f3a9c2e04";
const NEW_KEY: &str = "grok/2b6f0d1e-8c3a-4e7b-9a51-6d2c8f4e1a03";
const OLD_KEY: &str = "grok/7c1e9a4b-3d2f-4b8e-a6c0-5e9d1f3b7a02";
const BOTH_KEY: &str = "grok/4a8d2c6e-1f3b-4c9a-8e7d-0b5f3a9c2e04";
/// 2026-01-15T10:00:00Z, when the old-layout fixture's first update was written.
const T0_MS: i64 = 1_768_471_200_000;
const COUNTED: [&str; 4] = ["sessions", "messages", "aliases", "progress"];

struct Setup {
    _temp: TempDir,
    root: PathBuf,
    data: PathBuf,
}

impl Setup {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("temp directory is created");
        let root = temp.path().join("grok-home");
        copy_dir(Path::new(FIXTURES), &root);
        let data = temp.path().join("data");
        Self {
            _temp: temp,
            root,
            data,
        }
    }

    fn session_file(&self, folder: &str, session: &str, name: &str) -> PathBuf {
        self.root.join(folder).join(session).join(name)
    }

    fn import(&self) -> collect::ImportSummary {
        let mut store = Store::open(&self.data).expect("store opens");
        let summary = import_grok(&self.root, ORIGIN, &mut store).expect("import runs");
        assert!(summary.rejected.is_empty(), "no write is rejected");
        summary
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

    fn messages(&self, session: &str) -> Vec<(String, String)> {
        let db = self.index();
        let mut query = db
            .prepare(
                "SELECT role, searchable_text FROM messages WHERE session_key = ?1 ORDER BY position",
            )
            .expect("query prepares");
        query
            .query_map([session], |row| Ok((row.get(0)?, row.get(1)?)))
            .expect("query runs")
            .collect::<rusqlite::Result<_>>()
            .expect("rows decode")
    }

    fn times(&self, session: &str) -> Vec<Option<i64>> {
        let db = self.index();
        let mut query = db
            .prepare("SELECT at FROM messages WHERE session_key = ?1 ORDER BY position")
            .expect("query prepares");
        query
            .query_map([session], |row| row.get(0))
            .expect("query runs")
            .collect::<rusqlite::Result<_>>()
            .expect("rows decode")
    }

    fn session(&self, key: &str) -> (String, Option<String>, Option<String>, Option<i64>) {
        self.index()
            .query_row(
                "SELECT kind, title, project, started_at FROM sessions WHERE key = ?1",
                [key],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("session row exists")
    }

    fn find(&self, query: &str) -> Vec<String> {
        Query::open(&self.data, Timestamp(T0_MS))
            .expect("query opens")
            .find(query, 5)
            .expect("find runs")
            .into_iter()
            .map(|hit| hit.key)
            .collect()
    }
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

fn pairs(expected: &[(&str, &str)]) -> Vec<(String, String)> {
    expected
        .iter()
        .map(|(role, text)| (role.to_string(), text.to_string()))
        .collect()
}

fn append(path: &Path, line: &serde_json::Value) {
    OpenOptions::new()
        .append(true)
        .open(path)
        .expect("transcript opens for append")
        .write_all(format!("{line}\n").as_bytes())
        .expect("line is appended");
}

#[test]
fn scan_keeps_session_files_only() {
    let setup = Setup::new();
    let found: Vec<(String, WriteMode)> = collect::grok::scan(&setup.root)
        .expect("scan runs")
        .into_iter()
        .map(|file| (file.locator, file.write_mode))
        .collect();
    let expected = [
        (
            PROJECT_FOLDER,
            NEW_ID,
            "chat_history.jsonl",
            WriteMode::Append,
        ),
        (PROJECT_FOLDER, NEW_ID, "events.jsonl", WriteMode::Append),
        (PROJECT_FOLDER, NEW_ID, "summary.json", WriteMode::Replace),
        (PROJECT_FOLDER, OLD_ID, "summary.json", WriteMode::Replace),
        (PROJECT_FOLDER, OLD_ID, "updates.jsonl", WriteMode::Append),
        (PROJECT_FOLDER, OLD_ID, "usage.json", WriteMode::Replace),
        (
            OTHER_FOLDER,
            BOTH_ID,
            "chat_history.jsonl",
            WriteMode::Append,
        ),
        (OTHER_FOLDER, BOTH_ID, "events.jsonl", WriteMode::Append),
        (OTHER_FOLDER, BOTH_ID, "summary.json", WriteMode::Replace),
        (OTHER_FOLDER, BOTH_ID, "updates.jsonl", WriteMode::Append),
    ]
    .map(|(folder, session, name, mode)| (format!("{folder}/{session}/{name}"), mode));
    assert_eq!(
        found, expected,
        "transcripts, summaries, and usage are kept; temporary, lock, prompt, and tool files are not"
    );
}

#[test]
fn new_layout_session_has_its_chat_history_in_order_and_its_title() {
    let setup = Setup::new();
    setup.import();
    setup.sync();

    assert_eq!(
        setup.messages(NEW_KEY),
        pairs(&[
            ("user", "Add a --json flag to `todo list`."),
            (
                "assistant",
                "The user wants JSON output from the list command."
            ),
            ("assistant", "I'll read the list command first."),
            ("tool_call", r#"read_file {"path":"src/list.rs"}"#),
            ("tool_result", "pub fn list(items: &[Item]) {}"),
            (
                "assistant",
                "Added the flag. The test config sets API_TOKEN=[redacted] locally.",
            ),
        ]),
        "typed prompts, reasoning summaries, replies, and tools in order, without injected context"
    );
    assert_eq!(
        setup.session(NEW_KEY),
        (
            "main".to_owned(),
            Some("Add a JSON output flag".to_owned()),
            Some("/home/dev/example-project".to_owned()),
            Some(T0_MS + 123),
        ),
        "the title and start come from summary.json and the project from the folder name"
    );
}

#[test]
fn old_layout_session_has_its_updates_in_order_and_its_first_prompt_as_title() {
    let setup = Setup::new();
    setup.import();
    setup.sync();

    assert_eq!(
        setup.messages(OLD_KEY),
        pairs(&[
            (
                "user",
                "Draft release notes for version 2.0 that cover the new sync engine, the JSON output flag, and the removed legacy importer.",
            ),
            ("assistant", "The user wants release notes for 2.0."),
            ("assistant", "I'll read the changelog first."),
            ("tool_call", r#"read_file {"path":"CHANGELOG.md"}"#),
            ("tool_result", "## 2.0\n- New sync engine"),
            (
                "tool_call",
                r###"search_replace {"new_string":"## 2.0.0","old_string":"## 2.0","path":"CHANGELOG.md"}"###,
            ),
            ("tool_result", "CHANGELOG.md"),
            ("assistant", "Here is a first draft of the release notes."),
        ]),
        "one message per turn item, without hooks, progress updates, or prompts Grok sent itself"
    );
    assert_eq!(
        setup.times(OLD_KEY).first(),
        Some(&Some(T0_MS + 250)),
        "messages carry the update's time"
    );
    assert_eq!(
        setup.session(OLD_KEY),
        (
            "main".to_owned(),
            Some(
                "Draft release notes for version 2.0 that cover the new sync engine, the JSON out"
                    .to_owned()
            ),
            Some("/home/dev/example-project".to_owned()),
            Some(T0_MS),
        ),
        "an untitled summary leaves the first prompt, cut to 80 characters, as the title"
    );
}

#[test]
fn a_folder_with_both_transcripts_indexes_each_turn_once_from_the_updates() {
    let expected = pairs(&[
        ("user", "Rename the config loader."),
        ("assistant", "Renamed it to `load_config`."),
    ]);
    let turn_times = vec![Some(T0_MS + 100_250), Some(T0_MS + 104_250)];

    let fresh = Setup::new();
    fresh.import();
    fresh.sync();
    assert_eq!(fresh.messages(BOTH_KEY), expected, "each turn once");
    assert_eq!(
        fresh.times(BOTH_KEY),
        turn_times,
        "the messages are the updates"
    );

    let racing = Setup::new();
    let updates = racing.session_file(OTHER_FOLDER, BOTH_ID, "updates.jsonl");
    let bytes = fs::read(&updates).expect("updates are readable");
    fs::remove_file(&updates).expect("updates are set aside");
    racing.import();
    racing.sync();
    assert_eq!(
        racing.messages(BOTH_KEY),
        expected,
        "the chat history stands in while the folder has no updates"
    );
    fs::write(&updates, bytes).expect("Grok writes the updates");
    racing.import();
    racing.sync();
    assert_eq!(
        racing.messages(BOTH_KEY),
        expected,
        "the updates replace the chat history's copies"
    );
    assert_eq!(
        racing.times(BOTH_KEY),
        turn_times,
        "the messages are the updates"
    );
    assert_eq!(
        racing.counts(),
        fresh.counts(),
        "the index ends as if the updates had been there from the start"
    );
    assert_eq!(
        racing.session(BOTH_KEY).2.as_deref(),
        Some("/home/dev/other project"),
        "the folder name is URL-decoded"
    );
}

#[test]
fn a_session_id_and_its_prefix_find_the_session() {
    let setup = Setup::new();
    setup.import();
    setup.sync();

    for query in [NEW_ID, &NEW_ID[..8], OLD_ID] {
        let key = format!("grok/{}", if query == OLD_ID { OLD_ID } else { NEW_ID });
        assert_eq!(
            setup.find(query).first(),
            Some(&key),
            "{query} finds its session"
        );
    }
}

#[test]
fn second_import_and_sync_change_nothing() {
    let setup = Setup::new();
    setup.import();
    setup.sync();
    let before = setup.counts();

    let again = setup.import();
    assert_eq!(
        (
            again.writes_planned,
            again.bytes_appended,
            again.versions_stored
        ),
        (0, 0, 0),
        "a second import writes nothing"
    );
    assert_eq!(
        setup.sync(),
        SyncSummary::default(),
        "a second sync indexes nothing"
    );
    assert_eq!(setup.counts(), before, "no count changes");
}

#[test]
fn appended_line_adds_exactly_one_message() {
    let setup = Setup::new();
    setup.import();
    setup.sync();
    let before = setup.messages(OLD_KEY);

    append(
        &setup.session_file(PROJECT_FOLDER, OLD_ID, "updates.jsonl"),
        &serde_json::json!({
            "timestamp": 1_768_471_230,
            "method": "session/update",
            "params": {
                "sessionId": OLD_ID,
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": {"type": "text", "text": "I also noted the removed importer."},
                },
                "_meta": {"eventId": format!("{OLD_ID}-13"), "agentTimestampMs": 1_768_471_230_250_i64},
            },
        }),
    );
    setup.import();
    let summary = setup.sync();

    assert_eq!(
        (summary.streams_synced, summary.messages_indexed),
        (1, 1),
        "one stream syncs one new message"
    );
    let after = setup.messages(OLD_KEY);
    assert_eq!(after[..before.len()], before[..], "earlier messages stay");
    assert_eq!(
        after[before.len()..],
        pairs(&[("assistant", "I also noted the removed importer.")])[..],
        "exactly the appended message is new"
    );
}

#[test]
fn temporary_files_are_ignored() {
    let setup = Setup::new();
    let first = setup.import();
    setup.sync();
    let before = setup.counts();

    for name in [
        "summary.json.7f3e9a1c.tmp",
        "chat_history.jsonl.2c8d4b6e.tmp",
    ] {
        fs::write(
            setup.session_file(PROJECT_FOLDER, NEW_ID, name),
            r#"{"generated_title":"A half-written title"}"#,
        )
        .expect("Grok writes a temporary file");
    }
    let again = setup.import();

    assert_eq!(
        (again.files_scanned, again.writes_planned),
        (first.files_scanned, 0),
        "temporary files are not scanned"
    );
    assert_eq!(setup.sync(), SyncSummary::default(), "nothing is indexed");
    assert_eq!(setup.counts(), before, "no count changes");
    assert_eq!(
        setup.session(NEW_KEY).1.as_deref(),
        Some("Add a JSON output flag"),
        "the title stays the one in summary.json"
    );
}
