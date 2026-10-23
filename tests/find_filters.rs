use std::fs;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_prime-radiant");
const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/claude-code");

/// Origin A, project Garden-Planner, last active at the first millisecond of 2026-03-01.
const A1: &str = "claude-code/a1000000-0000-4000-8000-000000000001";
/// Origin A, project bird-feeder, last active at the last millisecond of 2026-03-01.
const A2: &str = "claude-code/a2000000-0000-4000-8000-000000000002";
/// Origin B, project garden-planner, last active at the first millisecond of 2026-03-02.
const B1: &str = "claude-code/b1000000-0000-4000-8000-000000000001";
/// Origin B, project bird-feeder, last active at the last millisecond of 2026-02-28. Its text names A1's ID.
const B2: &str = "claude-code/b2000000-0000-4000-8000-000000000002";
/// A Codex session from origin A, project garden-planner, last active at noon on 2026-03-01.
const C1: &str = "codex/c1000000-0000-4000-8000-000000000001";
/// Has the phrase "drop off" as "drop-off".
const P1: &str = "claude-code/d1000000-0000-4000-8000-000000000001";
/// Has "drop" and "off", not adjacent.
const P2: &str = "claude-code/d2000000-0000-4000-8000-000000000002";
/// Has "off" right before "drop".
const P3: &str = "claude-code/d3000000-0000-4000-8000-000000000003";
/// Ends one message with "drop" and starts the next with "off".
const P4: &str = "claude-code/d4000000-0000-4000-8000-000000000004";
/// Has the phrase only in its title.
const P5: &str = "claude-code/d5000000-0000-4000-8000-000000000005";

const MAIN_KEY: &str = "claude-code/33831ab7-5c2a-48a4-a8d7-0b5125040f4d";

fn id(key: &str) -> &str {
    key.split_once('/').expect("a key is <source>/<id>").1
}

/// Made-up sessions from two origins, in a data folder.
struct World {
    temp: TempDir,
    data: PathBuf,
}

impl World {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("temp directory is created");
        let (a, b) = (temp.path().join("origin-a"), temp.path().join("origin-b"));
        let codex = temp.path().join("codex");
        claude_code(
            &a,
            A1,
            "/home/dev/Garden-Planner",
            None,
            &[("2026-03-01T00:00:00.000Z", "Wire the lantern to the shed.")],
        );
        claude_code(
            &a,
            A2,
            "/home/dev/bird-feeder",
            None,
            &[
                (
                    "2026-02-28T23:00:00.000Z",
                    "Put a lantern timer on the feeder.",
                ),
                ("2026-03-01T23:59:59.999Z", "The timer works."),
            ],
        );
        claude_code(
            &b,
            B1,
            "/home/dev/garden-planner",
            None,
            &[("2026-03-02T00:00:00.000Z", "Paint the lantern green.")],
        );
        claude_code(
            &b,
            B2,
            "/home/dev/bird-feeder",
            None,
            &[(
                "2026-02-28T23:59:59.999Z",
                &format!("A lantern for the bird bath, as in {}.", id(A1)),
            )],
        );
        codex_session(
            &codex,
            C1,
            "/home/dev/garden-planner",
            "2026-03-01T12:00:00.000Z",
            "Hang the lantern by the gate.",
        );
        for (key, title, messages) in [
            (
                P1,
                None,
                &["The drop-off point moved to the north gate."][..],
            ),
            (P2, None, &["Drop the cable, then switch the pump off."]),
            (P3, None, &["Turn it off. Drop the crate here."]),
            (P4, None, &["Where do we drop", "off the spare keys?"]),
            (P5, Some("Drop off schedule"), &["Plan the week."]),
        ] {
            let messages: Vec<(&str, &str)> = messages
                .iter()
                .map(|text| ("2026-02-10T09:00:00.000Z", *text))
                .collect();
            claude_code(&a, key, "/home/dev/depot", title, &messages);
        }

        let data = temp.path().join("data");
        let data_flag = path(&data);
        run(&[
            "import",
            "--data",
            data_flag,
            "--origin",
            "origin-a",
            "--claude-code",
            path(&a),
            "--codex",
            path(&codex),
        ]);
        run(&[
            "import",
            "--data",
            data_flag,
            "--origin",
            "origin-b",
            "--claude-code",
            path(&b),
        ]);
        Self { temp, data }
    }

    /// The hits of `find <args>` against the data folder, best first.
    fn hits(&self, args: &[&str]) -> Vec<Value> {
        let args = [args, &["--data", path(&self.data), "--json"]].concat();
        match serde_json::from_str(&run(&args)).expect("--json prints JSON") {
            Value::Array(hits) => hits,
            other => panic!("find --json prints an array, not {other}"),
        }
    }

    /// The keys that `find <args>` returns, best first.
    fn ranked(&self, args: &[&str]) -> Vec<String> {
        self.hits(args)
            .iter()
            .map(|hit| hit["key"].as_str().expect("a hit has a key").to_owned())
            .collect()
    }

    /// The keys that `find <args>` returns, sorted.
    fn keys(&self, args: &[&str]) -> Vec<String> {
        let mut keys = self.ranked(args);
        keys.sort();
        keys
    }

    fn hub(&self) -> Hub {
        let child = Command::new(BIN)
            .args(["hub", "serve", "--data", path(&self.data)])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("the hub starts");
        let deadline = Instant::now() + Duration::from_secs(30);
        while UnixStream::connect(self.data.join("hub.sock")).is_err() {
            assert!(Instant::now() < deadline, "the hub listens within 30s");
            thread::sleep(Duration::from_millis(20));
        }
        Hub(child)
    }
}

struct Hub(Child);

impl Drop for Hub {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Writes one made-up Claude Code session, with user messages at the given times.
fn claude_code(root: &Path, key: &str, cwd: &str, title: Option<&str>, messages: &[(&str, &str)]) {
    let session = id(key);
    let dir = root.join(cwd.replace('/', "-"));
    fs::create_dir_all(&dir).expect("project folder is created");
    let mut lines = String::new();
    if let Some(title) = title {
        lines += &format!(
            "{}\n",
            json!({"type": "custom-title", "customTitle": title, "sessionId": session})
        );
    }
    let mut parent: Option<String> = None;
    for (i, (at, text)) in messages.iter().enumerate() {
        let uuid = format!("{}-0000-4000-8000-{i:012}", &session[..8]);
        lines += &format!(
            "{}\n",
            json!({
                "parentUuid": parent,
                "isSidechain": false,
                "cwd": cwd,
                "sessionId": session,
                "type": "user",
                "message": {"role": "user", "content": text},
                "uuid": uuid,
                "timestamp": at,
            })
        );
        parent = Some(uuid);
    }
    fs::write(dir.join(format!("{session}.jsonl")), lines).expect("transcript is written");
}

/// Writes one made-up Codex rollout with one user message.
fn codex_session(home: &Path, key: &str, cwd: &str, at: &str, text: &str) {
    let thread = id(key);
    let dir = home.join("sessions/2026/03/01");
    fs::create_dir_all(&dir).expect("rollout folder is created");
    let lines = [
        json!({"type": "session_meta", "timestamp": at, "ordinal": 0,
               "payload": {"id": thread, "cwd": cwd, "source": "cli"}}),
        json!({"type": "response_item", "timestamp": at, "ordinal": 1,
               "payload": {"type": "message", "id": "msg_0001", "role": "user",
                           "content": [{"type": "input_text", "text": text}]}}),
    ];
    let lines: String = lines.iter().map(|line| format!("{line}\n")).collect();
    fs::write(
        dir.join(format!("rollout-2026-03-01T12-00-00-{thread}.jsonl")),
        lines,
    )
    .expect("rollout is written");
}

fn command(args: &[&str]) -> Command {
    let mut command = Command::new(BIN);
    command.args(args).env_remove("PRIME_RADIANT_HUB");
    command
}

fn checked(output: Output, args: &[&str]) -> String {
    assert!(
        output.status.success(),
        "{args:?} exits 0: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("stdout is UTF-8")
}

fn run(args: &[&str]) -> String {
    checked(command(args).output().expect("the binary runs"), args)
}

/// The stderr of a command that must fail.
fn refused(args: &[&str]) -> String {
    let output = command(args).output().expect("the binary runs");
    assert!(!output.status.success(), "{args:?} fails");
    String::from_utf8(output.stderr).expect("stderr is UTF-8")
}

fn path(path: &Path) -> &str {
    path.to_str().expect("temp path is UTF-8")
}

fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

fn sorted(keys: &[&str]) -> Vec<String> {
    let mut keys: Vec<String> = keys.iter().map(|key| (*key).to_owned()).collect();
    keys.sort();
    keys
}

#[test]
fn each_filter_narrows_the_candidates() {
    let world = World::new();
    assert_eq!(
        world.keys(&["find", "lantern"]),
        sorted(&[A1, A2, B1, B2, C1])
    );

    for (filter, expected) in [
        (&["--source", "codex"][..], &[C1][..]),
        (&["--source", "claude-code"], &[A1, A2, B1, B2]),
        (&["--origin", "origin-b"], &[B1, B2]),
        (&["--project", "garden"], &[A1, B1, C1]),
        (&["--project", "GARDEN-planner"], &[A1, B1, C1]),
        (&["--since", "2026-03-02"], &[B1]),
        (&["--until", "2026-02-28"], &[B2]),
    ] {
        assert_eq!(
            world.keys(&[&["find", "lantern"], filter].concat()),
            sorted(expected),
            "find lantern {filter:?}"
        );
    }
}

#[test]
fn a_repeated_flag_allows_any_of_its_values() {
    let world = World::new();
    for (filter, expected) in [
        (
            &["--source", "codex", "--source", "claude-code"][..],
            &[A1, A2, B1, B2, C1][..],
        ),
        (
            &["--origin", "origin-a", "--origin", "origin-b"],
            &[A1, A2, B1, B2, C1],
        ),
        (&["--origin", "origin-b", "--origin", "nowhere"], &[B1, B2]),
    ] {
        assert_eq!(
            world.keys(&[&["find", "lantern"], filter].concat()),
            sorted(expected),
            "find lantern {filter:?}"
        );
    }
}

#[test]
fn filters_compose_and_keep_the_unfiltered_order() {
    let world = World::new();
    for (filter, expected) in [
        (
            &["--origin", "origin-a", "--project", "garden"][..],
            &[A1, C1][..],
        ),
        (
            &[
                "--origin",
                "origin-a",
                "--project",
                "garden",
                "--since",
                "2026-03-01T06:00:00Z",
            ],
            &[C1],
        ),
        (
            &[
                "--source",
                "claude-code",
                "--project",
                "bird",
                "--until",
                "2026-03-01",
            ],
            &[A2, B2],
        ),
        (&["--source", "codex", "--origin", "origin-b"], &[]),
    ] {
        assert_eq!(
            world.keys(&[&["find", "lantern"], filter].concat()),
            sorted(expected),
            "find lantern {filter:?}"
        );
    }

    let origin_a = [A1, A2, C1];
    let unfiltered: Vec<String> = world
        .ranked(&["find", "lantern"])
        .into_iter()
        .filter(|key| origin_a.contains(&key.as_str()))
        .collect();
    assert_eq!(
        world.ranked(&["find", "lantern", "--origin", "origin-a"]),
        unfiltered,
        "a filter drops sessions and leaves the rest in their order"
    );
}

#[test]
fn a_day_covers_its_first_and_last_utc_millisecond() {
    let world = World::new();
    for (filter, expected) in [
        (&["--since", "2026-03-01"][..], &[A1, A2, B1, C1][..]),
        (&["--until", "2026-03-01"], &[A1, A2, B2, C1]),
        (
            &["--since", "2026-03-01", "--until", "2026-03-01"],
            &[A1, A2, C1],
        ),
        (&["--since", "2026-03-01T12:00:00Z"], &[A2, B1, C1]),
        (&["--since", "2026-03-01T13:00:00+01:00"], &[A2, B1, C1]),
        (&["--until", "2026-03-01T07:00:00-05:00"], &[A1, B2, C1]),
        (&["--since", "2026-03-01t23:59:59.999z"], &[A2, B1]),
        (&["--until", "2026-03-01T23:59:59.998Z"], &[A1, B2, C1]),
    ] {
        assert_eq!(
            world.keys(&[&["find", "lantern"], filter].concat()),
            sorted(expected),
            "find lantern {filter:?}"
        );
    }

    let data = path(&world.data);
    for (flag, value) in [
        ("--since", "2026-3-1"),
        ("--since", "2026-02-30"),
        ("--until", "yesterday"),
        ("--until", "2026-03-01T12:00:00"),
        ("--since", "2026-03-01T24:00:00Z"),
        ("--since", "2026-03-01T12:00:00+1:00"),
    ] {
        let stderr = refused(&["find", "lantern", flag, value, "--data", data]);
        assert!(
            stderr.contains(flag) && stderr.contains(value),
            "{flag} {value} is refused with a message that names both: {stderr}"
        );
    }
}

#[test]
fn a_phrase_needs_its_words_adjacent_and_in_order_in_one_message_or_the_title() {
    let world = World::new();
    let hits = world.hits(&["find", "--phrase", "drop off"]);
    let mut keys: Vec<&str> = hits
        .iter()
        .map(|hit| hit["key"].as_str().expect("a hit has a key"))
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        sorted(&[P1, P5]),
        "a phrase alone is a query, and only adjacent words in order in one message or the title match it"
    );
    assert!(
        hits.iter()
            .all(|hit| hit["score"].as_f64().is_some_and(|score| score > 0.0)),
        "the phrase adds to the score: {hits:?}"
    );

    for (args, expected) in [
        (&["find", "--phrase", "Drop-Off"][..], &[P1, P5][..]),
        (&["find", "drop", "--phrase", "drop off"], &[P1, P5]),
        (&["find", "gate", "--phrase", "drop off"], &[P1, P5]),
        (
            &["find", "--phrase", "drop off", "--phrase", "north gate"],
            &[P1],
        ),
        (&["find", "--phrase", "gate north"], &[]),
        (&["find", "--phrase", "drop off", "--source", "codex"], &[]),
        (&["find", "drop"], &[P1, P2, P3, P4, P5]),
    ] {
        assert_eq!(world.keys(args), sorted(expected), "{args:?}");
    }
}

#[test]
fn find_without_words_or_a_phrase_is_a_usage_error() {
    let world = World::new();
    let stderr = refused(&["find", "--source", "codex", "--data", path(&world.data)]);
    assert!(
        stderr.contains("--phrase"),
        "the error says a phrase would do: {stderr}"
    );
}

#[test]
fn an_id_that_names_a_session_outside_the_filters_finds_nothing() {
    let world = World::new();
    for query in [A1, id(A1), "a1000000"] {
        assert_eq!(
            world.keys(&["find", query, "--origin", "origin-a"]),
            [A1],
            "{query} inside the filters finds its session"
        );
        assert_eq!(
            world.keys(&["find", query, "--origin", "origin-b"]),
            Vec::<String>::new(),
            "{query} outside the filters finds nothing, not B2, which names the ID in its text"
        );
        assert_eq!(
            world.keys(&["find", query, "--phrase", "drop off"]),
            Vec::<String>::new(),
            "{query} names a session without the phrase"
        );
    }
}

#[test]
fn a_subagent_hit_is_judged_by_its_main_sessions_metadata() {
    let world = World::new();
    let root = world.temp.path().join("fixture");
    copy_dir(Path::new(FIXTURES), &root);
    let data = world.temp.path().join("fixture-data");
    run(&[
        "import",
        "--data",
        path(&data),
        "--origin",
        "origin-a",
        "--claude-code",
        path(&root),
    ]);
    let find = |query: &[&str]| -> Vec<Value> {
        let args = [&["find"][..], query, &["--data", path(&data), "--json"]].concat();
        match serde_json::from_str(&run(&args)).expect("--json prints JSON") {
            Value::Array(hits) => hits,
            other => panic!("find --json prints an array, not {other}"),
        }
    };
    let keys =
        |hits: Vec<Value>| -> Vec<Value> { hits.iter().map(|hit| hit["key"].clone()).collect() };

    // The word and the phrase are only in the subagent, last active at 10:01:45; the main session at 10:01:03.
    assert_eq!(keys(find(&["collect"])), [json!(MAIN_KEY)]);
    assert_eq!(
        keys(find(&["--phrase", "items iter"])),
        [json!(MAIN_KEY)],
        "a phrase in a subagent's message counts for its main session"
    );
    assert_eq!(
        keys(find(&["collect", "--until", "2026-01-15T10:01:10Z"])),
        [json!(MAIN_KEY)],
        "the main session's last activity passes, though the subagent's does not"
    );
    assert_eq!(
        keys(find(&["collect", "--since", "2026-01-15T10:01:30Z"])),
        Vec::<Value>::new(),
        "the subagent's last activity passes, but the main session's does not"
    );
}

#[test]
fn filters_and_phrases_through_the_hub_answer_as_locally() {
    let world = World::new();
    let questions: [&[&str]; 6] = [
        &[
            "find",
            "lantern",
            "--source",
            "claude-code",
            "--origin",
            "origin-b",
        ],
        &[
            "find",
            "lantern",
            "--project",
            "GARDEN",
            "--until",
            "2026-03-01",
        ],
        &["find", "lantern", "--since", "2026-03-01T23:59:59.999Z"],
        &["find", "--phrase", "drop off"],
        &[
            "find", "gate", "--phrase", "drop-off", "--origin", "origin-a",
        ],
        &["find", id(A1), "--origin", "origin-b"],
    ];
    let expected: [&[&str]; 6] = [&[B1, B2], &[A1, C1], &[A2, B1], &[P1, P5], &[P1, P5], &[]];
    let local: Vec<Value> = questions
        .iter()
        .map(|args| Value::Array(world.hits(args)))
        .collect();
    for ((args, expected), local) in questions.iter().zip(expected).zip(&local) {
        let mut keys: Vec<&str> = local
            .as_array()
            .expect("hits are an array")
            .iter()
            .map(|hit| hit["key"].as_str().expect("a hit has a key"))
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, sorted(expected), "{args:?} locally");
    }

    let _hub = world.hub();
    let socket = world.data.join("hub.sock");
    let bridge = format!(
        "{} hub connect --data {}",
        quote(BIN),
        quote(path(&world.data))
    );
    let targets: [(&str, Vec<&str>, Option<&str>); 3] = [
        ("the hub socket", vec!["--hub-socket", path(&socket)], None),
        ("a hub command", vec!["--hub-command", &bridge], None),
        ("PRIME_RADIANT_HUB", vec![], Some(&bridge)),
    ];
    for (target, flags, env) in &targets {
        for (args, local) in questions.iter().zip(&local) {
            let args: Vec<&str> = [*args, &flags[..], &["--json"]].concat();
            let mut remote = command(&args);
            if let Some(env) = env {
                remote.env("PRIME_RADIANT_HUB", env);
            }
            let printed = checked(remote.output().expect("the binary runs"), &args);
            let remote: Value = serde_json::from_str(&printed).expect("--json prints JSON");
            assert_eq!(
                &remote, local,
                "{args:?} through {target} answers as the local folder does"
            );
        }
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
