use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use prime_radiant::collect::{self, import_codex};
use prime_radiant::index::{self, Cuts, Indexer, SyncSummary, Timestamp};
use prime_radiant::query::{Brief, ParentKind, Query};
use prime_radiant::raw::{RawRef, Sha256, Source, Store, StreamId, View, WriteMode};
use rusqlite::Connection;
use serde_json::Value;
use tempfile::TempDir;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/codex");
const ORIGIN: &str = "example-origin";
const DAY: &str = "sessions/2026/01/15";
const SESSION_ID: &str = "5f0c2a9e-7d41-4b8a-9e36-1a2b3c4d5e6f";
const SUBAGENT_ID: &str = "8e3d1c7b-2a64-4f90-b1d5-6c7d8e9f0a1b";
const BRANCH_ID: &str = "c4a7e2f1-9b38-4d56-8e0f-2b3c4d5e6f70";
const MAIN_KEY: &str = "codex/5f0c2a9e-7d41-4b8a-9e36-1a2b3c4d5e6f";
const SUBAGENT_KEY: &str = "codex/8e3d1c7b-2a64-4f90-b1d5-6c7d8e9f0a1b";
const COUNTED: [&str; 4] = ["sessions", "messages", "aliases", "progress"];

struct Setup {
    _temp: TempDir,
    root: PathBuf,
    data: PathBuf,
}

impl Setup {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("temp directory is created");
        let root = temp.path().join("codex-home");
        copy_dir(Path::new(FIXTURES), &root);
        let data = temp.path().join("data");
        Self {
            _temp: temp,
            root,
            data,
        }
    }

    fn rollout(&self, name: &str) -> PathBuf {
        self.root.join(DAY).join(name)
    }

    fn live(&self) -> PathBuf {
        self.rollout(&format!("rollout-2026-01-15T10-00-00-{SESSION_ID}.jsonl"))
    }

    fn finished(&self) -> PathBuf {
        self.rollout(&format!(
            "rollout-2026-01-15T10-00-00-{SESSION_ID}.jsonl.zst"
        ))
    }

    fn subagent(&self) -> PathBuf {
        self.rollout(&format!("rollout-2026-01-15T10-01-00-{SUBAGENT_ID}.jsonl"))
    }

    fn branch(&self) -> PathBuf {
        self.rollout(&format!(
            "rollout-2026-01-15T10-05-00-{SESSION_ID}_{BRANCH_ID}.jsonl"
        ))
    }

    fn import(&self) -> collect::ImportSummary {
        let mut store = Store::open(&self.data).expect("store opens");
        let summary = import_codex(&self.root, ORIGIN, &mut store).expect("import runs");
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

    fn strings(&self, sql: &str, arg: &str) -> Vec<String> {
        let db = self.index();
        let mut query = db.prepare(sql).expect("query prepares");
        query
            .query_map([arg], |row| row.get(0))
            .expect("query runs")
            .collect::<rusqlite::Result<_>>()
            .expect("rows decode")
    }

    fn message_ids(&self, session: &str) -> Vec<String> {
        self.strings(
            "SELECT message_id FROM messages WHERE session_key = ?1 ORDER BY position",
            session,
        )
    }

    fn search(&self, words: &str) -> Vec<String> {
        self.strings(
            "SELECT DISTINCT session_key FROM message_search WHERE message_search MATCH ?1 ORDER BY session_key",
            words,
        )
    }

    /// Appends a line to the session index, as Codex does each time it names a thread.
    fn name(&self, thread: &str, name: &str, at: &str) {
        let line = serde_json::json!({"id": thread, "thread_name": name, "updated_at": at});
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.root.join("session_index.jsonl"))
            .expect("session index opens for append")
            .write_all(format!("{line}\n").as_bytes())
            .expect("name is appended");
    }

    fn query(&self) -> Query {
        Query::open(&self.data, Timestamp(0)).expect("query opens")
    }

    fn brief(&self, key: &str) -> Brief {
        self.query().brief(key).expect("brief runs")
    }

    fn find(&self, words: &str) -> Vec<String> {
        self.query()
            .find(words, 10)
            .expect("find runs")
            .into_iter()
            .map(|hit| hit.key)
            .collect()
    }

    fn session(&self, key: &str) -> (String, Option<String>, Option<String>, Option<String>) {
        self.index()
            .query_row(
                "SELECT kind, parent_key, title, project FROM sessions WHERE key = ?1",
                [key],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("session row exists")
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

fn decompress(bytes: &[u8]) -> Vec<u8> {
    zstd::stream::decode_all(bytes).expect("version decompresses")
}

fn expected_ids(jsonl: &[u8], thread: &str) -> Vec<String> {
    let mut ids = Vec::new();
    for line in jsonl.split(|b| *b == b'\n').filter(|line| !line.is_empty()) {
        let line: Value = serde_json::from_slice(line).expect("line is JSON");
        let payload = &line["payload"];
        let indexed = match (line["type"].as_str(), payload["type"].as_str()) {
            (Some("response_item"), Some("message")) => payload["role"] != "developer",
            (Some("response_item"), Some("reasoning")) => {
                payload["summary"].as_array().is_some_and(|s| !s.is_empty())
            }
            (Some("response_item"), _) => true,
            (Some("compacted"), _) => true,
            _ => false,
        };
        if indexed {
            ids.push(match payload["id"].as_str() {
                Some(id) => id.to_owned(),
                None => format!("{thread}:ordinal:{}", line["ordinal"]),
            });
        }
    }
    ids
}

#[test]
fn fixture_zst_is_the_live_rollout_plus_its_last_lines() {
    let setup = Setup::new();
    let live = fs::read(setup.live()).expect("live rollout is readable");
    let finished = decompress(&fs::read(setup.finished()).expect("finished rollout is readable"));
    assert!(
        finished.starts_with(&live) && finished.len() > live.len(),
        "the finished rollout continues the live one"
    );
}

#[test]
fn scan_keeps_rollouts_and_the_session_index() {
    let setup = Setup::new();
    setup.name(SESSION_ID, "Listing output", "2026-01-15T10:30:00.000000Z");
    fs::write(setup.root.join("history.jsonl"), "{}\n").expect("history is written");
    fs::write(setup.root.join(DAY).join(".DS_Store"), "").expect("junk is written");
    fs::write(
        setup.rollout(&format!(
            "rollout-2026-01-15T10-00-00-{SESSION_ID}.jsonl.before-reattach"
        )),
        "{}\n",
    )
    .expect("backup is written");
    let archived = setup.root.join("archived_sessions");
    fs::create_dir_all(&archived).expect("archive directory is created");
    fs::copy(
        setup.finished(),
        archived.join(format!(
            "rollout-2026-01-15T10-00-00-{SESSION_ID}.jsonl.zst"
        )),
    )
    .expect("finished rollout is archived");

    let scanned: Vec<(String, WriteMode)> = collect::codex::LAYOUT
        .scan(&setup.root)
        .expect("scan runs")
        .into_iter()
        .map(|file| (file.locator, file.write_mode))
        .collect();
    assert_eq!(
        scanned,
        [
            (
                format!("archived_sessions/rollout-2026-01-15T10-00-00-{SESSION_ID}.jsonl.zst"),
                WriteMode::Replace
            ),
            ("session_index.jsonl".to_owned(), WriteMode::Append),
            (
                format!("{DAY}/rollout-2026-01-15T10-00-00-{SESSION_ID}.jsonl"),
                WriteMode::Append
            ),
            (
                format!("{DAY}/rollout-2026-01-15T10-00-00-{SESSION_ID}.jsonl.zst"),
                WriteMode::Replace
            ),
            (
                format!("{DAY}/rollout-2026-01-15T10-01-00-{SUBAGENT_ID}.jsonl"),
                WriteMode::Append
            ),
            (
                format!("{DAY}/rollout-2026-01-15T10-05-00-{SESSION_ID}_{BRANCH_ID}.jsonl"),
                WriteMode::Append
            ),
        ],
        "live rollouts and the session index append, finished rollouts replace, and nothing else is kept"
    );
}

#[test]
fn a_thread_name_titles_its_session_and_a_rename_replaces_it() {
    let setup = Setup::new();
    setup.import();
    setup.sync();

    setup.name(
        SESSION_ID,
        "Listing output for automation",
        "2026-01-15T10:30:00.000000Z",
    );
    setup.import();
    setup.sync();
    assert_eq!(
        setup.brief(MAIN_KEY).title.as_deref(),
        Some("Listing output for automation"),
        "a name given after the thread started replaces the title from its first request"
    );
    assert_eq!(
        setup.find("automation"),
        [MAIN_KEY],
        "a word only in the name finds the thread"
    );

    setup.name(
        SESSION_ID,
        "Readable listing",
        "2026-01-15T11:00:00.000000Z",
    );
    setup.import();
    setup.sync();
    assert_eq!(
        setup.brief(MAIN_KEY).title.as_deref(),
        Some("Readable listing"),
        "the latest name wins"
    );
    assert_eq!(
        setup.find("readable"),
        [MAIN_KEY],
        "the new name finds the thread"
    );
    assert!(
        setup.find("automation").is_empty(),
        "the old name no longer does"
    );
    assert_eq!(
        setup.brief(SUBAGENT_KEY).title.as_deref(),
        Some("Review the --json change in src/cli.rs for missing escapes"),
        "another thread keeps its title"
    );
}

#[test]
fn a_name_that_arrives_before_its_thread_titles_it_and_keeps_its_parent() {
    let setup = Setup::new();
    let rollout = fs::read(setup.subagent()).expect("subagent rollout is readable");
    fs::remove_file(setup.subagent()).expect("subagent rollout is removed");
    setup.name(
        SUBAGENT_ID,
        "Escaping review",
        "2026-01-15T10:01:00.500000Z",
    );
    setup.import();
    setup.sync();

    fs::write(setup.subagent(), rollout).expect("subagent rollout is written");
    setup.import();
    setup.sync();
    let brief = setup.brief(SUBAGENT_KEY);
    assert_eq!(
        brief.title.as_deref(),
        Some("Escaping review"),
        "the name outlasts the rollout's first request"
    );
    assert_eq!(
        brief.parent.map(|parent| (parent.key, parent.kind)),
        Some((MAIN_KEY.to_owned(), ParentKind::Subagent)),
        "the rollout still makes the thread a subagent of its session"
    );
}

#[test]
fn live_rollout_and_its_finished_zst_index_each_message_once() {
    let setup = Setup::new();
    let finished = fs::read(setup.finished()).expect("finished rollout is readable");
    fs::remove_file(setup.finished()).expect("finished rollout is set aside");
    setup.import();
    setup.sync();
    let live_ids = expected_ids(
        &fs::read(setup.live()).expect("live rollout is readable"),
        SESSION_ID,
    );

    fs::write(setup.finished(), &finished).expect("Codex writes the finished rollout");
    fs::remove_file(setup.live()).expect("Codex removes the live rollout");
    setup.import();
    setup.sync();

    let finished_ids = expected_ids(&decompress(&finished), SESSION_ID);
    let branch_ids = expected_ids(
        &fs::read(setup.branch()).expect("branch is readable"),
        SESSION_ID,
    );
    assert!(
        finished_ids.starts_with(&live_ids) && finished_ids.len() > live_ids.len(),
        "the finished rollout holds the live messages and more"
    );
    let mut indexed = setup.message_ids(MAIN_KEY);
    indexed.sort();
    let mut expected = [finished_ids, branch_ids].concat();
    expected.sort();
    assert_eq!(
        indexed, expected,
        "one session holds every message of the thread, each once"
    );
    let raw = View::open(&setup.data).expect("raw view opens");
    let main_streams = raw
        .streams()
        .expect("streams list")
        .into_iter()
        .filter(|stream| stream.key.locator.contains("10-00-00"))
        .count();
    assert_eq!(
        main_streams, 2,
        "the raw layer keeps the live and the finished rollout"
    );
}

#[test]
fn raw_refs_read_back_their_source_lines() {
    let setup = Setup::new();
    setup.import();
    setup.sync();
    let raw = View::open(&setup.data).expect("raw view opens");
    let db = setup.index();
    let mut query = db
        .prepare(
            "SELECT session_key, message_id, raw_stream, raw_generation, raw_version, raw_start, raw_end
             FROM messages",
        )
        .expect("message query prepares");
    type RefRow = (String, String, i64, Option<i64>, Option<String>, i64, i64);
    let rows: Vec<RefRow> = query
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
            ))
        })
        .expect("message query runs")
        .collect::<rusqlite::Result<_>>()
        .expect("message rows decode");
    assert!(
        rows.iter().any(|row| row.4.is_some()),
        "the message only in the finished rollout points into its version"
    );
    for (session, id, stream, generation, version, start, end) in rows {
        let bytes = match (generation, version) {
            (Some(generation), None) => raw
                .read(&RawRef {
                    stream: StreamId(stream),
                    generation: generation as u64,
                    start: start as u64,
                    end: end as u64,
                })
                .expect("log ref reads"),
            (None, Some(hex)) => {
                let sha256 = Sha256::from_hex(&hex).expect("version checksum is hex");
                decompress(&raw.version(sha256).expect("version reads"))
                    [start as usize..end as usize]
                    .to_vec()
            }
            other => panic!("message {id} has an invalid raw ref {other:?}"),
        };
        let line: Value = serde_json::from_slice(&bytes).expect("raw ref is one JSON line");
        let found = line["payload"]["id"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| {
                let thread = session.strip_prefix("codex/").expect("a Codex session key");
                format!("{thread}:ordinal:{}", line["ordinal"])
            });
        assert_eq!(found, id, "message {id} reads back its own line");
    }
}

#[test]
fn subagent_thread_has_its_session_as_parent_and_a_branch_joins_its_session() {
    let setup = Setup::new();
    setup.import();
    setup.sync();

    assert_eq!(
        setup.strings(
            "SELECT key FROM sessions WHERE source = ?1 ORDER BY key",
            "codex"
        ),
        [MAIN_KEY, SUBAGENT_KEY],
        "the branch rollout adds no session"
    );
    assert_eq!(
        setup.session(MAIN_KEY),
        (
            "main".to_owned(),
            None,
            Some("Add a --json flag to the list command so scripts can read its output".to_owned()),
            Some("/home/dev/example-project".to_owned()),
        ),
        "the main session is titled by the first request the user typed"
    );
    assert_eq!(
        setup.session(SUBAGENT_KEY),
        (
            "subagent".to_owned(),
            Some(MAIN_KEY.to_owned()),
            Some("Review the --json change in src/cli.rs for missing escapes".to_owned()),
            Some("/home/dev/example-project".to_owned()),
        ),
        "the subagent's parent is its session, despite the parent's meta copied into its rollout"
    );
    for (id, key) in [(SESSION_ID, MAIN_KEY), (SUBAGENT_ID, SUBAGENT_KEY)] {
        assert_eq!(
            setup.strings("SELECT session_key FROM aliases WHERE value = ?1", id),
            [key],
            "{id} is an alias of {key}"
        );
    }
    assert_eq!(
        setup.message_ids(SUBAGENT_KEY),
        ["msg_0201", "am_0201", "am_0202", "msg_0202"],
        "the subagent holds its own messages"
    );
    let started = |key: &str| -> i64 {
        setup
            .index()
            .query_row(
                "SELECT started_at FROM sessions WHERE key = ?1",
                [key],
                |row| row.get(0),
            )
            .expect("session has a start")
    };
    let at = |text: &str| Timestamp::parse_utc(text).expect("timestamp parses").0;
    assert_eq!(
        (started(MAIN_KEY), started(SUBAGENT_KEY)),
        (
            at("2026-01-15T10:00:00.000Z"),
            at("2026-01-15T10:01:00.220Z")
        ),
        "each thread starts at its own session_meta, not its branch's or its parent's"
    );
}

#[test]
fn encrypted_reasoning_is_never_searchable_and_a_summary_is() {
    let setup = Setup::new();
    setup.import();
    setup.sync();
    assert!(
        fs::read_to_string(setup.live())
            .expect("live rollout is readable")
            .contains("ciphertextsentinel"),
        "the fixture holds the encrypted word"
    );
    assert_eq!(
        setup.search("ciphertextsentinel"),
        Vec::<String>::new(),
        "encrypted reasoning reaches no searchable text"
    );
    assert_eq!(
        setup.search("\"argument parser\""),
        [MAIN_KEY],
        "the reasoning summary is searchable"
    );
    assert_eq!(
        setup.search("escaped"),
        [SUBAGENT_KEY],
        "an agent message is searchable"
    );
    assert_eq!(
        setup.search("\"json argument\""),
        [MAIN_KEY],
        "a compaction summary is searchable"
    );
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
    let before = setup.message_ids(SUBAGENT_KEY);

    let line = serde_json::json!({
        "type": "response_item",
        "timestamp": "2026-01-15T10:01:10.000Z",
        "ordinal": 8,
        "payload": {
            "type": "message",
            "id": "msg_0203",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "Reviewed one more file."}],
        },
    });
    OpenOptions::new()
        .append(true)
        .open(setup.subagent())
        .expect("subagent rollout opens for append")
        .write_all(format!("{line}\n").as_bytes())
        .expect("line is appended");
    setup.import();
    let summary = setup.sync();

    assert_eq!(
        (summary.streams_synced, summary.messages_indexed),
        (1, 1),
        "one stream syncs one new message"
    );
    let after: HashSet<String> = setup.message_ids(SUBAGENT_KEY).into_iter().collect();
    let new: Vec<&String> = after.iter().filter(|id| !before.contains(id)).collect();
    assert_eq!(new, ["msg_0203"], "exactly the appended message is new");
}

#[test]
fn fallback_title_is_the_typed_request_and_skips_injected_context() {
    let title = |item: Value| {
        let line = serde_json::json!({
            "type": "response_item",
            "timestamp": "2026-01-15T10:00:00.000Z",
            "ordinal": 1,
            "payload": item,
        });
        let parser = index::parser(Source::Codex);
        let session = parser
            .session(
                &format!("rollout-2026-01-15T10-00-00-{SESSION_ID}.jsonl"),
                None,
            )
            .expect("a rollout names its thread");
        parser
            .line(&session, line.to_string().as_bytes(), Cuts::INDEX)
            .facts
            .fallback_title
    };
    let user = |texts: &[&str]| {
        let content: Vec<Value> = texts
            .iter()
            .map(|text| serde_json::json!({"type": "input_text", "text": text}))
            .collect();
        serde_json::json!({"type": "message", "id": "msg", "role": "user", "content": content})
    };

    assert_eq!(
        title(user(&[
            "# AGENTS.md instructions for /home/dev\n\nRun the tests.",
            "<environment_context>\n</environment_context>",
            "Rename   the\nconfig file",
        ])),
        Some("Rename the config file".to_owned()),
        "injected parts are skipped and whitespace collapses"
    );
    assert_eq!(
        title(user(&[
            "# Files mentioned by the user:\n\n## a.rs: /home/dev/a.rs\n\n## My request for Codex:\nFix the parser"
        ])),
        Some("Fix the parser".to_owned()),
        "an IDE request is titled by the request, not the file list"
    );
    assert_eq!(
        title(user(&[
            "<codex_delegation>\n  <input>Port the scanner</input>\n</codex_delegation>"
        ])),
        Some("Port the scanner".to_owned()),
        "a delegated thread is titled by its input"
    );
    assert_eq!(
        title(user(&[&"word ".repeat(40)])).map(|title| title.chars().count()),
        Some(80),
        "a long request is cut to 80 characters"
    );
    let agent = |text: &str| {
        serde_json::json!({
            "type": "agent_message",
            "id": "am",
            "author": "/root",
            "recipient": "/root/worker",
            "content": [{"type": "input_text", "text": text}],
        })
    };
    assert_eq!(
        title(agent(
            "Message Type: NEW_TASK\nTask name: /root/worker\nSender: /root\nPayload:\nCount the lines"
        )),
        Some("Count the lines".to_owned()),
        "a new task titles its subagent"
    );
    assert_eq!(
        title(agent(
            "Message Type: NEW_TASK\nTask name: /root/worker\nSender: /root\nPayload:\n"
        )),
        Some("/root/worker".to_owned()),
        "a new task whose payload is encrypted titles its subagent by the task name"
    );
    assert_eq!(
        title(agent(
            "Message Type: FINAL_ANSWER\nTask name: /root\nSender: /root/worker\nPayload:\n42"
        )),
        None,
        "a subagent's answer is no title"
    );
}

#[test]
fn truncated_zst_still_yields_its_complete_leading_lines() {
    let setup = Setup::new();
    let live = fs::read(setup.live()).expect("live rollout is readable");
    let lines: Vec<&[u8]> = live.split_inclusive(|b| *b == b'\n').collect();
    let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), 0).expect("encoder starts");
    let mut ends = Vec::new();
    for line in &lines {
        encoder.write_all(line).expect("line compresses");
        encoder.flush().expect("block ends");
        ends.push(encoder.get_ref().len());
    }
    let whole = encoder.finish().expect("frame ends");
    let kept = lines.len() / 2;
    let cut = (ends[kept - 1] + ends[kept]) / 2;
    fs::remove_dir_all(setup.root.join(DAY)).expect("fixture rollouts are removed");
    fs::create_dir_all(setup.root.join(DAY)).expect("day folder is created");
    fs::write(setup.finished(), &whole[..cut]).expect("truncated rollout is written");
    setup.import();
    setup.sync();

    let leading = expected_ids(&lines[..kept].concat(), SESSION_ID);
    assert!(
        !leading.is_empty() && leading.len() < expected_ids(&live, SESSION_ID).len(),
        "the cut drops some messages and keeps others"
    );
    assert_eq!(
        setup.message_ids(MAIN_KEY),
        leading,
        "every line before the damaged block is indexed"
    );
}

#[test]
fn a_new_zst_version_replaces_the_messages_of_the_one_before() {
    let setup = Setup::new();
    let whole = fs::read(setup.finished()).expect("finished rollout is readable");
    let decoded = decompress(&whole);
    let lines: Vec<&[u8]> = decoded.split_inclusive(|b| *b == b'\n').collect();
    let kept = lines.len() / 2;
    let shorter = zstd::stream::encode_all(&lines[..kept].concat()[..], 0).expect("lines compress");
    fs::remove_dir_all(setup.root.join(DAY)).expect("fixture rollouts are removed");
    fs::create_dir_all(setup.root.join(DAY)).expect("day folder is created");
    fs::write(setup.finished(), &whole).expect("finished rollout is written");
    setup.import();
    setup.sync();
    assert_eq!(
        setup.message_ids(MAIN_KEY),
        expected_ids(&decoded, SESSION_ID)
    );

    fs::write(setup.finished(), &shorter).expect("shorter rollout replaces it");
    assert_eq!(setup.import().versions_stored, 1);
    setup.sync();
    let leading = expected_ids(&lines[..kept].concat(), SESSION_ID);
    assert!(!leading.is_empty() && leading.len() < expected_ids(&decoded, SESSION_ID).len());
    assert_eq!(
        setup.message_ids(MAIN_KEY),
        leading,
        "the session holds only the new version's messages"
    );
    assert_eq!(
        setup.strings(
            "SELECT DISTINCT raw_version FROM messages WHERE session_key = ?1",
            MAIN_KEY
        ),
        [Sha256::of(&shorter).to_hex()],
        "every message points into the new version"
    );
    let before = setup.counts();
    setup.import();
    assert_eq!(setup.sync(), SyncSummary::default());
    assert_eq!(setup.counts(), before, "a second sync changes nothing");
}

#[test]
fn only_a_fork_marks_messages_as_copies_of_another_thread() {
    const FORK_ID: &str = "0b1c2d3e-4f50-4a61-8b72-c3d4e5f60718";
    const OTHER_ID: &str = "1a2b3c4d-5e6f-4a70-8b81-92a3b4c5d6e7";
    let setup = Setup::new();
    let meta = |id: &str, forked_from: Option<&str>| {
        serde_json::json!({
            "type": "session_meta",
            "timestamp": "2026-01-15T11:00:00.000Z",
            "ordinal": 0,
            "payload": {
                "session_id": id,
                "id": id,
                "forked_from_id": forked_from,
                "timestamp": "2026-01-15T11:00:00.000Z",
                "cwd": "/home/dev/example-project",
            },
        })
    };
    let live = fs::read_to_string(setup.live()).expect("live rollout is readable");
    let summary_line = live
        .lines()
        .find(|line| line.contains("\"rs_0002\""))
        .expect("the fixture has a reasoning summary");
    let mut copied: Value = serde_json::from_str(summary_line).expect("line is JSON");
    copied["timestamp"] = "2026-01-15T11:00:01.000Z".into();
    copied["ordinal"] = 1.into();
    let compacted = serde_json::json!({
        "type": "compacted",
        "timestamp": "2026-01-15T11:00:02.000Z",
        "ordinal": 16,
        "payload": {"message": "An unrelated thread compacted its yaml work."},
    });
    let rollout = |id: &str, lines: &[Value]| {
        let text: String = lines.iter().map(|line| format!("{line}\n")).collect();
        fs::write(
            setup.rollout(&format!("rollout-2026-01-15T11-00-00-{id}.jsonl")),
            text,
        )
        .expect("rollout is written");
    };
    rollout(FORK_ID, &[meta(FORK_ID, Some(SESSION_ID)), copied]);
    rollout(OTHER_ID, &[meta(OTHER_ID, None), compacted]);
    setup.import();
    setup.sync();

    let fork_key = format!("codex/{FORK_ID}");
    let other_key = format!("codex/{OTHER_ID}");
    assert_eq!(
        setup.strings(
            "SELECT DISTINCT copied_from FROM messages WHERE session_key = ?1",
            &fork_key
        ),
        [MAIN_KEY],
        "the fork's copied message belongs to the thread it forked"
    );
    let (kind, parent, _, _) = setup.session(&fork_key);
    assert_eq!(
        (kind.as_str(), parent.as_deref()),
        ("fork", Some(MAIN_KEY)),
        "a thread that copies another thread's messages is its fork"
    );
    assert_eq!(
        setup.search("\"argument parser\""),
        [MAIN_KEY],
        "the copy leaves search to its original"
    );
    assert_eq!(
        setup.strings(
            "SELECT DISTINCT session_key FROM messages WHERE copied_from IS NOT NULL AND session_key LIKE ?1",
            "codex/%"
        ),
        [fork_key],
        "an ordinal that two threads share marks nothing as a copy"
    );
    assert_eq!(
        setup.search("unrelated"),
        [other_key.as_str()],
        "the other thread's compaction stays searchable"
    );
    let (kind, parent, _, _) = setup.session(&other_key);
    assert_eq!(
        (kind.as_str(), parent),
        ("main", None),
        "the other thread stays a main session"
    );
}
