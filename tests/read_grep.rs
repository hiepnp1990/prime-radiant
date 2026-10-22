use std::fs::{self, Permissions};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use prime_radiant::index::Timestamp;
use serde_json::{Value, json};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_prime-radiant");
const PAGE_CHARS: usize = 12_000;
const SESSION_ID: &str = "9a1f3c5e-2b4d-4e6f-8a0b-1c2d3e4f5a6b";
const KEY: &str = "claude-code/9a1f3c5e-2b4d-4e6f-8a0b-1c2d3e4f5a6b";
const FORK_ID: &str = "4c6e8a0b-1d3f-4a5c-9e7b-2f4a6c8e0b1d";
const PROJECT: &str = "/home/dev/garden-planner";
/// 2026-03-01T09:00:00Z.
const START_MS: i64 = 1_772_355_600_000;
const PHRASE: &str = "the Quokka-Seventeen marker";
const STEPS: usize = 30;
/// The step whose tool result holds `PHRASE`.
const PHRASE_STEP: usize = 21;
const LONG_REQUEST_CHARS: usize = 30_000;

/// A made-up Claude Code transcript, built one line at a time.
struct Transcript {
    session: String,
    lines: Vec<String>,
    last: Option<String>,
}

impl Transcript {
    fn new(session: &str) -> Self {
        Self {
            session: session.to_owned(),
            lines: Vec::new(),
            last: None,
        }
    }

    /// Appends a line and returns its position in the session.
    fn push(&mut self, kind: &str, content: Value) -> usize {
        let n = self.lines.len();
        let uuid = format!("{}-0000-4000-8000-{n:012}", &self.session[..8]);
        let line = json!({
            "parentUuid": self.last,
            "isSidechain": false,
            "cwd": PROJECT,
            "sessionId": self.session,
            "type": kind,
            "message": {"role": kind, "content": content},
            "uuid": uuid,
            "timestamp": Timestamp(START_MS + n as i64 * 1000).to_string(),
        });
        self.lines.push(format!("{line}\n"));
        self.last = Some(uuid);
        n
    }

    fn text(&mut self, kind: &str, text: &str) -> usize {
        self.push(kind, json!([{"type": "text", "text": text}]))
    }

    fn tool_call(&mut self, input: Value) -> usize {
        self.push(
            "assistant",
            json!([{"type": "tool_use", "id": "toolu_example", "name": "Bash", "input": input}]),
        )
    }

    fn tool_result(&mut self, text: &str) -> usize {
        self.push(
            "user",
            json!([{"type": "tool_result", "tool_use_id": "toolu_example", "content": text}]),
        )
    }

    fn write(&self, root: &Path) {
        let dir = root.join("-home-dev-garden-planner");
        fs::create_dir_all(&dir).expect("project folder is created");
        fs::write(
            dir.join(format!("{}.jsonl", self.session)),
            self.lines.concat(),
        )
        .expect("transcript is written");
    }
}

/// Filler text of `chars` characters made of numbered words.
fn filler(label: &str, chars: usize) -> String {
    let mut text = String::new();
    let mut n = 0;
    while text.len() < chars {
        text += &format!("{label}{n} ");
        n += 1;
    }
    text.truncate(chars);
    text
}

/// What the long session holds, to check `read` against.
struct Long {
    count: usize,
    long_request: String,
    long_request_at: usize,
    assistant_texts: Vec<(usize, String)>,
    tool_calls: Vec<usize>,
    tool_results: Vec<usize>,
    phrase_at: usize,
}

/// A session of many steps, each an answer, a tool call longer than 2,000 characters, and a tool result longer than
/// 4,000, with a request longer than a page in the middle.
fn long_session(root: &Path) -> Long {
    let mut transcript = Transcript::new(SESSION_ID);
    let mut long = Long {
        count: 0,
        long_request: filler("bed", LONG_REQUEST_CHARS),
        long_request_at: 0,
        assistant_texts: Vec::new(),
        tool_calls: Vec::new(),
        tool_results: Vec::new(),
        phrase_at: 0,
    };
    transcript.text("user", "Plan the raised beds for the spring garden.");
    for step in 0..STEPS {
        if step == STEPS / 2 {
            long.long_request_at = transcript.text("user", &long.long_request);
        }
        let answer = format!("Step {step}. {}", filler("seed", 900));
        long.assistant_texts
            .push((transcript.text("assistant", &answer), answer));
        long.tool_calls
            .push(transcript.tool_call(json!({"command": filler("echo", 3000)})));
        let mut output = filler("row", 5000);
        if step == PHRASE_STEP {
            output.replace_range(1500..1500 + PHRASE.len(), PHRASE);
            long.phrase_at = transcript.lines.len();
        }
        long.tool_results.push(transcript.tool_result(&output));
    }
    long.count = transcript.lines.len();
    transcript.write(root);
    long
}

struct Setup {
    _temp: TempDir,
    root: PathBuf,
    data: PathBuf,
}

impl Setup {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("temp directory is created");
        let root = temp.path().join("projects");
        let data = temp.path().join("data");
        Self {
            _temp: temp,
            root,
            data,
        }
    }

    fn import(&self) {
        run(&[
            "import",
            "--data",
            path(&self.data),
            "--origin",
            "example-origin",
            "--claude-code",
            path(&self.root),
        ]);
    }

    fn local(&self, args: &[&str]) -> String {
        let mut args = args.to_vec();
        args.extend(["--data", path(&self.data)]);
        run(&args)
    }

    fn local_json(&self, args: &[&str]) -> Value {
        json_of(&self.local(&[args, &["--json"]].concat()))
    }

    fn pages(&self) -> usize {
        self.local_json(&["read", KEY])["pages"]
            .as_u64()
            .expect("a page counts the pages") as usize
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

    fn bridge_command(&self) -> String {
        format!(
            "{} hub connect --data {}",
            quote(BIN),
            quote(path(&self.data))
        )
    }
}

struct Hub(Child);

impl Drop for Hub {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn command(args: &[&str]) -> Command {
    let mut command = Command::new(BIN);
    command.args(args).env_remove("PRIME_RADIANT_HUB");
    command
}

fn checked(output: Output, args: &[&str]) -> String {
    assert!(
        output.status.success(),
        "prime-radiant {args:?} fails: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("output is UTF-8")
}

fn run(args: &[&str]) -> String {
    checked(command(args).output().expect("the binary runs"), args)
}

fn json_of(text: &str) -> Value {
    serde_json::from_str(text).expect("--json prints JSON")
}

fn path(path: &Path) -> &str {
    path.to_str().expect("temp path is UTF-8")
}

fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// Splits printed page text into the messages and the closing line that names the next page.
fn body_and_footer(text: &str) -> (&str, &str) {
    let at = text.rfind("\npage ").expect("a page ends with its number");
    (&text[..at], &text[at + 1..])
}

#[test]
fn read_pages_stay_within_the_size_and_cover_every_message_once_in_order() {
    let setup = Setup::new();
    let long = long_session(&setup.root);
    setup.import();
    let pages = setup.pages();
    assert!(
        pages > 3,
        "the long session takes several pages, not {pages}"
    );

    let mut seen: Vec<(i64, Option<u64>)> = Vec::new();
    let mut long_request = String::new();
    for page in 1..=pages {
        let number = page.to_string();
        let text = setup.local(&["read", KEY, "--page", &number]);
        let (body, footer) = body_and_footer(&text);
        assert!(
            body.chars().count() <= PAGE_CHARS,
            "page {page} holds {} characters",
            body.chars().count()
        );
        if page < pages {
            assert_eq!(
                footer,
                format!(
                    "page {page} of {pages}. Next page: prime-radiant read {KEY} --page {} --data {}\n",
                    page + 1,
                    path(&setup.data)
                )
            );
        } else {
            assert_eq!(footer, format!("page {pages} of {pages}, the last page.\n"));
        }

        let json = setup.local_json(&["read", KEY, "--page", &number]);
        assert_eq!(json["page"], page);
        let messages = json["messages"].as_array().expect("a page lists messages");
        for message in messages {
            let position = message["position"]
                .as_i64()
                .expect("a message has a position");
            let part = message["part"]["number"].as_u64();
            let text = message["text"].as_str().expect("a message has text");
            if part.is_some() {
                assert_eq!(
                    messages.len(),
                    1,
                    "a part of a long message has its page to itself"
                );
                long_request += text;
            }
            assert!(
                text.chars().count() <= PAGE_CHARS,
                "message {position} fits on its page"
            );
            seen.push((position, part));
        }
    }

    let positions: Vec<i64> = seen.iter().map(|(position, _)| *position).collect();
    let mut once = positions.clone();
    once.dedup();
    assert_eq!(
        once,
        (0..long.count as i64).collect::<Vec<_>>(),
        "every message appears once, in position order"
    );
    let parts: Vec<u64> = seen
        .iter()
        .filter(|(position, _)| *position == long.long_request_at as i64)
        .map(|(_, part)| part.expect("the long request is split into parts"))
        .collect();
    assert!(
        parts.len() >= 3,
        "a 30,000-character request takes 3 pages or more"
    );
    assert_eq!(parts, (1..=parts.len() as u64).collect::<Vec<_>>());
    assert_eq!(
        long_request, long.long_request,
        "the parts of the long request add up to all of it"
    );
    assert!(
        seen.iter()
            .filter(|(position, _)| *position != long.long_request_at as i64)
            .all(|(_, part)| part.is_none()),
        "only a message larger than a page is split"
    );
}

#[test]
fn read_shows_full_answers_and_cuts_tool_calls_and_results() {
    let setup = Setup::new();
    let long = long_session(&setup.root);
    setup.import();
    let mut by_position = std::collections::HashMap::new();
    for page in 1..=setup.pages() {
        let json = setup.local_json(&["read", KEY, "--page", &page.to_string()]);
        for message in json["messages"].as_array().expect("a page lists messages") {
            by_position.insert(
                message["position"].as_u64().unwrap() as usize,
                message.clone(),
            );
        }
    }
    let chars = |position: &usize| {
        by_position[position]["text"]
            .as_str()
            .unwrap()
            .chars()
            .count()
    };
    for (position, answer) in &long.assistant_texts {
        assert_eq!(by_position[position]["role"], "assistant");
        assert_eq!(
            by_position[position]["text"],
            answer.as_str(),
            "an answer is shown in full"
        );
    }
    for position in &long.tool_calls {
        assert_eq!(by_position[position]["role"], "tool_call");
        assert!(
            by_position[position]["text"]
                .as_str()
                .unwrap()
                .starts_with("Bash {")
        );
        assert_eq!(
            chars(position),
            2000,
            "a 3,000-character tool call is cut to 2,000"
        );
    }
    for position in &long.tool_results {
        assert_eq!(by_position[position]["role"], "tool_result");
        assert_eq!(
            chars(position),
            4000,
            "a 5,000-character tool result is cut to 4,000"
        );
    }
    assert_eq!(by_position[&0]["at"], "2026-03-01T09:00:00Z");
}

#[test]
fn grep_finds_a_phrase_inside_a_tool_result_and_names_its_page() {
    let setup = Setup::new();
    let long = long_session(&setup.root);
    setup.import();
    let expected_page = (1..=setup.pages())
        .find(|page| {
            setup.local_json(&["read", KEY, "--page", &page.to_string()])["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|message| message["position"] == long.phrase_at)
        })
        .expect("some page shows the tool result");
    assert!(expected_page > 1, "the phrase is past the first page");

    let found = setup.local_json(&["grep", "9a1f3c5e", "quokka-SEVENTEEN", "MARKER"]);
    assert_eq!(found["key"], KEY);
    assert_eq!(found["total"], 1);
    let only = &found["matches"][0];
    assert_eq!(only["position"], long.phrase_at);
    assert_eq!(only["role"], "tool_result");
    assert_eq!(only["page"], expected_page);
    let context = only["context"].as_str().unwrap();
    assert!(
        context.contains(PHRASE),
        "the context holds the match: {context}"
    );
    assert!(
        context.chars().count() <= 200 + PHRASE.len() + 6,
        "the context is about 200 characters around the match"
    );

    let text = setup.local(&["grep", KEY, "quokka-seventeen"]);
    assert!(
        text.contains(&format!(
            "[{}] tool_result, page {expected_page}:",
            long.phrase_at
        )),
        "the printed match names its page: {text}"
    );
    let many = setup.local_json(&["grep", KEY, "row1"]);
    assert!(many["total"].as_u64().unwrap() > 20);
    assert_eq!(
        many["matches"].as_array().unwrap().len(),
        20,
        "grep shows the first 20 matches"
    );
    assert_eq!(
        setup.local_json(&["grep", KEY, "no such words here"])["total"],
        0
    );
}

#[test]
fn a_secret_in_a_transcript_never_reaches_read_or_grep() {
    let setup = Setup::new();
    let token = ["gh", "p_", &"Zq7Lm2Xc".repeat(5)].concat();
    let key = ["s", "k-", &"proj".repeat(6)].concat();
    let mut transcript = Transcript::new(SESSION_ID);
    transcript.text("user", &format!("Deploy with the token {token} please."));
    transcript.tool_call(json!({
        "command": format!("curl -H 'Authorization: Bearer {token}' https://example.com/deploy")
    }));
    transcript.tool_result(&format!("deploy ok, used API_KEY={key} and {token}"));
    transcript.text("assistant", &format!("Deployed. The key was {key}."));
    transcript.write(&setup.root);
    setup.import();

    let outputs = [
        setup.local(&["read", KEY]),
        setup.local(&["read", KEY, "--json"]),
        setup.local(&["grep", KEY, "deploy"]),
        setup.local(&["grep", KEY, "deploy", "--json"]),
        setup.local(&["grep", KEY, "authorization", "--json"]),
        setup.local(&["grep", KEY, &token, "--json"]),
    ];
    for output in &outputs {
        assert!(!output.contains(&token), "a token leaked: {output}");
        assert!(!output.contains(&key), "a key leaked: {output}");
        assert!(
            !output.contains("Zq7Lm2Xc"),
            "part of a token leaked: {output}"
        );
    }
    assert!(
        outputs[0].contains("[redacted]"),
        "read marks what it removed"
    );
    let deploys = json_of(&outputs[3]);
    assert!(deploys["total"].as_u64().unwrap() >= 3);
    assert!(
        deploys["matches"]
            .as_array()
            .unwrap()
            .iter()
            .any(|found| found["context"].as_str().unwrap().contains("[redacted]")),
        "grep context shows where a secret was"
    );
}

#[test]
fn a_fork_shows_its_copied_history_marked_as_copied() {
    let setup = Setup::new();
    let mut original = Transcript::new(SESSION_ID);
    original.text("user", "Sketch the watering schedule.");
    original.text("assistant", "Water the beds every second morning.");
    original.write(&setup.root);
    let mut fork = Transcript::new(SESSION_ID);
    fork.text("user", "Sketch the watering schedule.");
    fork.text("assistant", "Water the beds every second morning.");
    fork.session = FORK_ID.to_owned();
    fork.text("user", "Now add the compost days.");
    let lines = fork
        .lines
        .iter()
        .enumerate()
        .map(|(n, line)| match n {
            2 => line.replace("2026-03-01T09:00:02Z", "2026-03-02T09:00:00Z"),
            _ => line.clone(),
        })
        .collect::<String>();
    fs::write(
        setup
            .root
            .join("-home-dev-garden-planner")
            .join(format!("{FORK_ID}.jsonl")),
        lines,
    )
    .expect("fork transcript is written");
    setup.import();

    let fork_key = format!("claude-code/{FORK_ID}");
    let page = setup.local_json(&["read", &fork_key]);
    let copied: Vec<&Value> = page["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| &message["copied_from"])
        .collect();
    assert_eq!(copied, [&json!(KEY), &json!(KEY), &Value::Null]);
    let text = setup.local(&["read", &fork_key]);
    assert!(
        text.contains(&format!(
            "[0] user, 2026-03-01T09:00:00Z, copied from {KEY}\nSketch the watering schedule."
        )),
        "the copied history is shown with its owner: {text}"
    );
    assert!(text.contains("[2] user, 2026-03-02T09:00:00Z\nNow add the compost days."));
}

/// JSON of a brief without its freshness, which says when and by whom it was built.
fn without_freshness(mut brief: Value) -> Value {
    brief
        .as_object_mut()
        .expect("a brief is an object")
        .remove("freshness");
    brief
}

#[test]
fn remote_queries_through_a_running_hub_match_local_ones() {
    let setup = Setup::new();
    long_session(&setup.root);
    setup.import();
    let questions: [&[&str]; 6] = [
        &["find", "quokka", "seventeen"],
        &["find", "raised", "beds"],
        &["brief", "9a1f3c5e"],
        &["read", KEY, "--page", "2"],
        &["read", KEY, "--page", "4"],
        &["grep", KEY, "Quokka"],
    ];
    let normal = |args: &[&str], json: Value| match args[0] {
        "brief" => without_freshness(json),
        _ => json,
    };
    let local: Vec<Value> = questions
        .iter()
        .map(|args| normal(args, setup.local_json(args)))
        .collect();

    let _hub = setup.hub();
    let socket = setup.data.join("hub.sock");
    let bridge = setup.bridge_command();
    let targets: [(&str, Vec<&str>, Option<&str>); 3] = [
        ("the hub socket", vec!["--hub-socket", path(&socket)], None),
        ("a hub command", vec!["--hub-command", &bridge], None),
        ("PRIME_RADIANT_HUB", vec![], Some(&bridge)),
    ];
    for (target, flags, env) in &targets {
        for (args, expected) in questions.iter().zip(&local) {
            let args: Vec<&str> = [*args, &flags[..], &["--json"]].concat();
            let mut remote = command(&args);
            if let Some(env) = env {
                remote.env("PRIME_RADIANT_HUB", env);
            }
            let printed = checked(remote.output().expect("the binary runs"), &args);
            assert_eq!(
                &normal(&args, json_of(&printed)),
                expected,
                "{args:?} through {target} answers as the local folder does"
            );
        }
    }

    let printed = checked(
        command(&["read", KEY])
            .env("PRIME_RADIANT_HUB", &bridge)
            .output()
            .expect("the binary runs"),
        &["read", KEY],
    );
    assert!(
        printed.ends_with(&format!("Next page: prime-radiant read {KEY} --page 2\n")),
        "with the hub from the environment, the next command needs no flag"
    );
    let missing = command(&["brief", "no-such-session", "--hub-socket", path(&socket)])
        .output()
        .expect("the binary runs");
    assert!(!missing.status.success());
    assert_eq!(
        String::from_utf8_lossy(&missing.stderr),
        "error: no session matches no-such-session\n",
        "a refusal reads as it does locally"
    );
}

/// Makes a folder and everything in it read-only, and writable again when dropped.
struct ReadOnly(PathBuf);

impl ReadOnly {
    fn set(dir: &Path, writable: bool) {
        for entry in fs::read_dir(dir).expect("folder is readable") {
            let path = entry.expect("entry is readable").path();
            if path.is_dir() {
                Self::set(&path, writable);
            } else if !path.ends_with("hub.sock") {
                let mode = if writable { 0o600 } else { 0o400 };
                fs::set_permissions(&path, Permissions::from_mode(mode)).expect("mode is set");
            }
        }
        let mode = if writable { 0o700 } else { 0o500 };
        fs::set_permissions(dir, Permissions::from_mode(mode)).expect("mode is set");
    }
}

impl Drop for ReadOnly {
    fn drop(&mut self) {
        Self::set(&self.0, true);
    }
}

#[test]
fn a_remote_brief_needs_no_write_access_to_the_data_folder() {
    let setup = Setup::new();
    long_session(&setup.root);
    setup.import();
    let _hub = setup.hub();
    let socket = setup.data.join("hub.sock");
    let remote = |args: &[&str]| {
        let args = [args, &["--hub-socket", path(&socket), "--json"]].concat();
        json_of(&run(&args))
    };
    remote(&["stats"]);
    let _read_only = ReadOnly(setup.data.clone());
    ReadOnly::set(&setup.data, false);

    let first = remote(&["brief", KEY]);
    assert_eq!(first["key"], KEY);
    assert_eq!(first["freshness"]["rebuilt"], true);
    assert_eq!(
        remote(&["brief", KEY])["freshness"]["rebuilt"],
        false,
        "the hub stored the brief it built"
    );
    assert_eq!(remote(&["find", "quokka"])[0]["key"], KEY);
    assert_eq!(remote(&["read", KEY])["page"], 1);
    assert_eq!(remote(&["grep", KEY, "quokka"])["total"], 1);
}
