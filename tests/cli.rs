use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use prime_radiant::index::Timestamp;
use serde_json::{Value, json};
use tempfile::TempDir;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/claude-code");
const SESSION_ID: &str = "33831ab7-5c2a-48a4-a8d7-0b5125040f4d";
const AGENT_ID: &str = "a75ce3543dd257a2d";
const MAIN_KEY: &str = "claude-code/33831ab7-5c2a-48a4-a8d7-0b5125040f4d";
const SUBAGENT_KEY: &str =
    "claude-code/33831ab7-5c2a-48a4-a8d7-0b5125040f4d/agent-a75ce3543dd257a2d";
const OTHER_ID: &str = "5f0c9e21-7d4b-4c3a-9e8f-2b6a1d0c4e77";
const OTHER_KEY: &str = "claude-code/5f0c9e21-7d4b-4c3a-9e8f-2b6a1d0c4e77";
const FORK_ID: &str = "9e7d4c21-3b5a-4f60-8a1e-6c2f0b9d8e14";
const FORK_KEY: &str = "claude-code/9e7d4c21-3b5a-4f60-8a1e-6c2f0b9d8e14";

struct Setup {
    _temp: TempDir,
    root: PathBuf,
    data: PathBuf,
}

impl Setup {
    /// The fixture plus a second session whose text names the first session's IDs.
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("temp directory is created");
        let root = temp.path().join("projects");
        copy_dir(Path::new(FIXTURES), &root);
        let other = root.join("-home-dev-other-project");
        fs::create_dir_all(&other).expect("project folder is created");
        let lines = [
            line(
                OTHER_ID,
                "user",
                "c0000000-0000-4000-8000-000000000001",
                None,
                json!(format!(
                    "Session {SESSION_ID} added a json flag, and agent {AGENT_ID} read the list code. \
                     Review 33831ab7 and add the same json flag to `todo show`."
                )),
                "2026-02-01T09:00:00.000Z",
            ),
            line(
                OTHER_ID,
                "assistant",
                "c0000000-0000-4000-8000-000000000002",
                Some("c0000000-0000-4000-8000-000000000001"),
                json!([{"type": "text", "text": "I'll add the json flag to `todo show` the same way."}]),
                "2026-02-01T09:00:30.000Z",
            ),
        ];
        fs::write(other.join(format!("{OTHER_ID}.jsonl")), lines.concat())
            .expect("second session is written");
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

    fn import(&self) -> String {
        run(&[
            "import",
            "--data",
            path(&self.data),
            "--origin",
            "example-origin",
            "--claude-code",
            path(&self.root),
        ])
    }

    fn find(&self, query: &str) -> Vec<Value> {
        match run_json(&["find", query, "--data", path(&self.data), "--json"]) {
            Value::Array(hits) => hits,
            other => panic!("find --json prints an array, not {other}"),
        }
    }

    fn find_keys(&self, query: &str) -> Vec<String> {
        self.find(query)
            .iter()
            .map(|hit| hit["key"].as_str().expect("a hit has a key").to_owned())
            .collect()
    }

    fn brief(&self, session: &str) -> Value {
        run_json(&["brief", session, "--data", path(&self.data), "--json"])
    }

    fn stats(&self) -> Value {
        run_json(&["stats", "--data", path(&self.data), "--json"])
    }
}

fn line(
    session: &str,
    kind: &str,
    uuid: &str,
    parent: Option<&str>,
    content: Value,
    at: &str,
) -> String {
    let line = json!({
        "parentUuid": parent,
        "isSidechain": false,
        "cwd": "/home/dev/other-project",
        "sessionId": session,
        "type": kind,
        "message": {"role": kind, "content": content},
        "uuid": uuid,
        "timestamp": at,
    });
    format!("{line}\n")
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

fn path(path: &Path) -> &str {
    path.to_str().expect("temp path is UTF-8")
}

fn run(args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_prime-radiant"))
        .args(args)
        .output()
        .expect("the built binary runs");
    assert!(
        output.status.success(),
        "{args:?} exits 0: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("stdout is UTF-8")
}

fn run_json(args: &[&str]) -> Value {
    serde_json::from_str(&run(args)).expect("--json prints JSON")
}

fn object_keys(value: &Value) -> Vec<&str> {
    let mut keys: Vec<&str> = value
        .as_object()
        .expect("value is an object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    keys
}

#[test]
fn version_flag_prints_the_crate_version() {
    let stdout = run(&["--version"]);
    assert_eq!(
        stdout.trim(),
        format!("prime-radiant {}", env!("CARGO_PKG_VERSION")),
        "--version prints the binary name and crate version"
    );
}

#[test]
fn find_puts_the_named_session_first_for_an_id_an_alias_and_a_prefix() {
    let setup = Setup::new();
    setup.import();
    assert_eq!(
        setup
            .find_keys("review 33831ab7 json flag")
            .first()
            .map(String::as_str),
        Some(OTHER_KEY),
        "as words, the IDs rank the session that mentions them first"
    );

    for (query, expected) in [
        (SESSION_ID, MAIN_KEY),
        (MAIN_KEY, MAIN_KEY),
        (AGENT_ID, SUBAGENT_KEY),
        ("33831ab7", MAIN_KEY),
        ("5f0c9e21", OTHER_KEY),
    ] {
        assert_eq!(
            setup.find_keys(query).first().map(String::as_str),
            Some(expected),
            "find {query} returns {expected} first"
        );
    }
    assert!(
        setup.find_keys("33831ab").is_empty(),
        "a 7-character prefix is not an ID, and matches no whole word"
    );
}

#[test]
fn find_matches_words_only_in_a_tool_result_preview() {
    let setup = Setup::new();
    setup.import();

    let hits = setup.find("why was the compile so sluggish and unoptimized");

    assert_eq!(
        hits.iter().map(|hit| &hit["key"]).collect::<Vec<_>>(),
        [MAIN_KEY],
        "one shared word is enough, and only the main session has it"
    );
    let hit = &hits[0];
    assert_eq!(
        object_keys(hit),
        [
            "key",
            "last_activity",
            "project",
            "ran_on",
            "score",
            "snippet",
            "state",
            "title"
        ]
    );
    assert_eq!(hit["title"], "Add a JSON output flag");
    assert_eq!(hit["project"], "/home/dev/example-project");
    assert_eq!(hit["ran_on"], "example-origin");
    assert_eq!(hit["last_activity"], "2026-01-15T10:01:03Z");
    assert_eq!(hit["state"], "idle");
    assert!(hit["score"].as_f64().is_some_and(|score| score > 0.0));
    assert!(
        hit["snippet"]
            .as_str()
            .is_some_and(|snippet| snippet.contains("**unoptimized**")),
        "the snippet highlights the match: {}",
        hit["snippet"]
    );
}

#[test]
fn find_counts_a_subagent_hit_toward_its_main_session() {
    let setup = Setup::new();
    setup.import();

    assert_eq!(
        setup.find_keys("collect"),
        [MAIN_KEY],
        "a word only in the subagent transcript finds the main session"
    );
}

#[test]
fn brief_extracts_the_session_and_is_rebuilt_after_a_new_line() {
    let setup = Setup::new();
    setup.import();

    let brief = setup.brief("33831ab7");
    assert_eq!(brief["key"], MAIN_KEY);
    assert_eq!(brief["title"], "Add a JSON output flag");
    assert_eq!(brief["origin"], "example-origin");
    assert_eq!(brief["started"], "2026-01-15T10:00:00Z");
    assert_eq!(
        brief["first_request"],
        "Add a --json flag to `todo list` so scripts can read the output. Keep the plain output as it is."
    );
    assert_eq!(
        brief["last_answer"],
        "The flag is parsed but never read. I'll pass `args.json` into `render` and print a JSON array when it is set."
    );
    assert_eq!(
        brief["changed_files"],
        json!(["/home/dev/example-project/src/cli.rs"])
    );
    assert_eq!(brief["pull_requests"], json!([]));
    assert_eq!(
        brief["subagents"],
        json!([{"key": SUBAGENT_KEY, "title": "Find the list rendering code", "message_count": 6}])
    );
    assert_eq!(
        brief["messages"],
        json!({"user": 2, "assistant": 2, "tool_call": 2, "tool_result": 3, "attachment": 1, "total": 10})
    );
    assert_eq!(brief["freshness"]["built_through"], 9);
    assert_eq!(brief["freshness"]["rebuilt"], true);
    assert_eq!(
        setup.brief(MAIN_KEY)["freshness"]["rebuilt"],
        false,
        "a second brief reuses the stored one"
    );

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after 1970")
        .as_millis() as i64;
    let pull_request = "https://github.com/example/todo-cli/pull/12";
    let mut transcript = OpenOptions::new()
        .append(true)
        .open(setup.main_transcript())
        .expect("transcript opens for append");
    transcript
        .write_all(
            line(
                SESSION_ID,
                "assistant",
                "c0000000-0000-4000-8000-000000000003",
                Some("1ba1c71c-5c5a-4d58-aa20-8f86f83a4023"),
                json!([{"type": "text", "text": format!("Opened {pull_request} with the flag.")}]),
                &Timestamp(now).to_string(),
            )
            .as_bytes(),
        )
        .expect("line is appended");
    setup.import();

    let rebuilt = setup.brief(SESSION_ID);
    assert_eq!(rebuilt["freshness"]["built_through"], 10);
    assert_eq!(rebuilt["freshness"]["rebuilt"], true);
    assert_eq!(
        rebuilt["last_answer"],
        format!("Opened {pull_request} with the flag.")
    );
    assert_eq!(rebuilt["pull_requests"], json!([pull_request]));
    assert_eq!(rebuilt["state"], "live", "the session ran a moment ago");
}

#[test]
fn brief_names_a_subagent_or_a_fork_and_the_session_it_belongs_to() {
    let setup = Setup::new();
    let original = fs::read_to_string(setup.main_transcript()).expect("transcript is readable");
    let own = line(
        FORK_ID,
        "user",
        "c0000000-0000-4000-8000-000000000003",
        None,
        json!("Now add a quiet flag that hides the banner."),
        "2026-01-16T08:00:05.000Z",
    );
    fs::write(
        setup
            .main_transcript()
            .with_file_name(format!("{FORK_ID}.jsonl")),
        original + &own,
    )
    .expect("fork transcript is written");
    setup.import();

    assert_eq!(setup.brief(MAIN_KEY)["parent"], Value::Null);
    for (session, kind, label) in [
        (SUBAGENT_KEY, "subagent", "subagent of"),
        (FORK_KEY, "fork", "fork of"),
    ] {
        assert_eq!(
            setup.brief(session)["parent"],
            json!({"key": MAIN_KEY, "kind": kind})
        );
        let text = run(&["brief", session, "--data", path(&setup.data)]);
        assert!(
            text.contains(&format!("{label}: {MAIN_KEY}")),
            "the {kind} brief says {label:?}:\n{text}"
        );
    }
}

#[test]
fn a_session_prefix_may_keep_the_source_it_was_copied_with() {
    let setup = Setup::new();
    setup.import();

    assert_eq!(setup.brief("claude-code/33831ab7")["key"], MAIN_KEY);
    assert_eq!(setup.find_keys("claude-code/33831ab7"), [MAIN_KEY]);
    let other_source = Command::new(env!("CARGO_BIN_EXE_prime-radiant"))
        .args(["brief", "codex/33831ab7", "--data", path(&setup.data)])
        .output()
        .expect("prime-radiant runs");
    assert!(
        !other_source.status.success()
            && String::from_utf8_lossy(&other_source.stderr).contains("no session matches"),
        "the source narrows the prefix to that harness"
    );
}

#[test]
fn brief_text_fits_in_100_columns() {
    let setup = Setup::new();
    setup.import();

    let text = run(&["brief", SESSION_ID, "--data", path(&setup.data)]);
    assert!(text.contains("Add a --json flag to `todo list`"));
    for line in text.lines() {
        assert!(line.chars().count() <= 100, "{line:?} fits in 100 columns");
    }
}

#[test]
fn stats_has_the_documented_shape_and_a_second_import_keeps_it() {
    let setup = Setup::new();
    setup.import();
    let stats = setup.stats();

    assert_eq!(object_keys(&stats), ["counts", "sessions", "streams"]);
    assert_eq!(
        stats["counts"],
        json!({
            "streams": 6, "segments": 3, "versions": 3,
            "sessions": 3, "messages": 18, "aliases": 4, "media": 1, "artifacts": 1,
        })
    );
    let streams = stats["streams"].as_array().expect("streams is an array");
    let locators: Vec<&str> = streams
        .iter()
        .map(|s| s["locator"].as_str().expect("locator"))
        .collect();
    let mut sorted = locators.clone();
    sorted.sort_unstable();
    assert_eq!(locators, sorted, "streams are sorted by locator");
    for stream in streams {
        assert_eq!(
            object_keys(stream),
            ["checksum", "generation", "locator", "offset"]
        );
        assert_eq!(stream["generation"], 0);
        let append = stream["locator"]
            .as_str()
            .is_some_and(|l| l.ends_with(".jsonl"));
        assert_eq!(stream["offset"].is_u64(), append, "{stream}");
        assert_eq!(stream["checksum"].is_string(), !append, "{stream}");
    }
    assert_eq!(
        stats["sessions"],
        json!([
            {"key": MAIN_KEY, "message_count": 10},
            {"key": SUBAGENT_KEY, "message_count": 6},
            {"key": OTHER_KEY, "message_count": 2},
        ])
    );

    setup.import();
    assert_eq!(setup.stats(), stats, "a second import changes nothing");
}

#[test]
fn import_reads_codex_and_grok_beside_claude_code_and_both_commands_need_a_source() {
    const CODEX: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/codex");
    const CODEX_ID: &str = "5f0c2a9e-7d41-4b8a-9e36-1a2b3c4d5e6f";
    const GROK: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/grok");
    const GROK_ID: &str = "7c1e9a4b-3d2f-4b8e-a6c0-5e9d1f3b7a02";
    let setup = Setup::new();
    let data = path(&setup.data);
    run(&[
        "import",
        "--data",
        data,
        "--origin",
        "example-origin",
        "--claude-code",
        path(&setup.root),
        "--codex",
        CODEX,
        "--grok",
        GROK,
    ]);
    for (query, expected) in [
        (CODEX_ID.to_owned(), format!("codex/{CODEX_ID}")),
        (GROK_ID.to_owned(), format!("grok/{GROK_ID}")),
        (SESSION_ID.to_owned(), MAIN_KEY.to_owned()),
    ] {
        assert_eq!(
            setup.find_keys(&query).first(),
            Some(&expected),
            "{query} finds its session"
        );
    }

    for command in [
        ["import", "--data", data],
        ["collect", "--hub-socket", "hub.sock"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_prime-radiant"))
            .args(command)
            .args(["--origin", "example-origin"])
            .output()
            .expect("the built binary runs");
        assert!(
            !output.status.success(),
            "{} without a source fails",
            command[0]
        );
    }
}
