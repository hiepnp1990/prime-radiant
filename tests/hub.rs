use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{self, BufReader, Write as _};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use prime_radiant::collect::{self, t3_code};
use prime_radiant::index::Timestamp;
use prime_radiant::protocol::{
    self, Request, Response, Role, read_request, read_response, write_request, write_response,
};
use prime_radiant::query::{Ask, Find};
use prime_radiant::raw::{Progress, RawRef, Sha256, Source, StreamKey, View, Write, WriteMode};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde_json::{Value, json};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_prime-radiant");
const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/claude-code");
const CODEX_FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/codex");
const GROK_FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/grok");
const CODEX_ID: &str = "5f0c2a9e-7d41-4b8a-9e36-1a2b3c4d5e6f";
const CODEX_DAY: &str = "sessions/2026/01/15";
const GROK_ID: &str = "7c1e9a4b-3d2f-4b8e-a6c0-5e9d1f3b7a02";
const GROK_FOLDER: &str = "sessions/%2Fhome%2Fdev%2Fexample-project";
const THREADS: [&str; 2] = [
    "0d5c1e4a-6a0b-4c8e-9f3e-2b7a1c9d4e01",
    "5b8e2f71-9c3d-4a6e-8b10-4f7d2c6a9e02",
];
const ORIGIN: &str = "example-origin";
const SESSION_ID: &str = "33831ab7-5c2a-48a4-a8d7-0b5125040f4d";
const MAIN_KEY: &str = "claude-code/33831ab7-5c2a-48a4-a8d7-0b5125040f4d";
const LARGE_ID: &str = "7e3b0c4d-1a2f-4e5d-8c9b-0a1b2c3d4e5f";
const LARGE_BYTES: usize = 40 << 20;
/// Generous for a loaded CI machine running debug builds, but bounded.
const CONVERGE: Duration = Duration::from_secs(180);
const POLL: Duration = Duration::from_millis(100);
/// Long enough for the collector to send a file the watcher reported, and for a sweep to run after it.
const QUIET_AND_A_SWEEP: Duration = Duration::from_secs(3);

struct Setup {
    temp: TempDir,
    data: PathBuf,
}

impl Setup {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("temp directory is created");
        let data = temp.path().join("data");
        Self { temp, data }
    }

    /// A projects folder holding a copy of the fixtures.
    fn fixture_root(&self, name: &str) -> PathBuf {
        let root = self.temp.path().join(name);
        copy_dir(Path::new(FIXTURES), &root);
        root
    }

    fn hub(&self) -> Proc {
        let hub = Proc::spawn(
            "hub",
            &["hub", "serve", "--data", path(&self.data)],
            self.temp.path(),
        );
        let deadline = Instant::now() + Duration::from_secs(30);
        while UnixStream::connect(self.data.join("hub.sock")).is_err() {
            assert!(Instant::now() < deadline, "the hub listens within 30s");
            thread::sleep(Duration::from_millis(20));
        }
        hub
    }

    fn bridge_command(&self) -> String {
        format!(
            "{} hub connect --data {}",
            quote(Path::new(BIN)),
            quote(&self.data)
        )
    }

    fn collector(&self, name: &str, origin: &str, root: &Path, hub_command: &str) -> Proc {
        self.collector_with(name, origin, root, hub_command, &[])
    }

    fn collector_with(
        &self,
        name: &str,
        origin: &str,
        root: &Path,
        hub_command: &str,
        extra: &[&str],
    ) -> Proc {
        self.collector_of(name, origin, &claude_code(root), hub_command, extra)
    }

    fn collector_of(
        &self,
        name: &str,
        origin: &str,
        sources: &[(Source, PathBuf)],
        hub_command: &str,
        extra: &[&str],
    ) -> Proc {
        let mut args = vec![
            "collect".to_owned(),
            "--origin".to_owned(),
            origin.to_owned(),
        ];
        for (source, root) in sources {
            args.push(format!("--{}", source.as_str()));
            args.push(path(root).to_owned());
        }
        args.extend(["--hub-command".to_owned(), hub_command.to_owned()]);
        args.extend(extra.iter().map(|arg| (*arg).to_owned()));
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        Proc::spawn(name, &args, self.temp.path())
    }

    /// A copy of the Codex and Grok fixtures, and a made-up T3 Code database with its writer.
    fn other_sources(&self) -> (PathBuf, PathBuf, PathBuf, Connection) {
        let codex = self.temp.path().join("codex");
        copy_dir(Path::new(CODEX_FIXTURES), &codex);
        let grok = self.temp.path().join("grok");
        copy_dir(Path::new(GROK_FIXTURES), &grok);
        let t3 = self.temp.path().join("t3").join("statev2.sqlite");
        fs::create_dir_all(t3.parent().expect("database has a folder"))
            .expect("database folder is created");
        let writer = t3_database(&t3);
        (codex, grok, t3, writer)
    }

    fn version_count(&self, locator: &str) -> i64 {
        self.raw_db()
            .query_row(
                "SELECT COUNT(*) FROM versions v JOIN streams st ON st.id = v.stream WHERE st.locator = ?1",
                [locator],
                |row| row.get(0),
            )
            .expect("version count query runs")
    }

    fn segment_count(&self, locator: &str) -> i64 {
        self.raw_db()
            .query_row(
                "SELECT COUNT(*) FROM segments s JOIN streams st ON st.id = s.stream WHERE st.locator = ?1",
                [locator],
                |row| row.get(0),
            )
            .expect("segment count query runs")
    }

    fn raw_db(&self) -> Connection {
        Connection::open_with_flags(self.data.join("raw.db"), OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("raw.db opens read-only")
    }

    /// The stream's row ID, generation, and confirmed offset, once the hub has the stream.
    fn confirmed(&self, db: &Connection, locator: &str) -> Option<(i64, i64, u64)> {
        db.query_row(
            "SELECT id, generation, confirmed_offset FROM streams WHERE locator = ?1",
            [locator],
            |row| Ok((row.get(0)?, row.get(1)?, row.get::<_, i64>(2)? as u64)),
        )
        .optional()
        .expect("stream query runs")
    }

    /// Waits until the hub has confirmed part, but not all, of a stream, and returns the confirmed offset.
    fn wait_for_partial(&self, locator: &str, len: u64) -> u64 {
        let deadline = Instant::now() + CONVERGE;
        let db = self.raw_db_when_ready();
        loop {
            match self.confirmed(&db, locator) {
                Some((_, _, offset)) if offset == len => {
                    panic!("{locator} synced completely before the test could interrupt it")
                }
                Some((_, _, offset)) if offset > 0 => return offset,
                _ => {}
            }
            assert!(Instant::now() < deadline, "{locator} started syncing");
            thread::sleep(Duration::from_millis(2));
        }
    }

    fn raw_db_when_ready(&self) -> Connection {
        let deadline = Instant::now() + CONVERGE;
        while !self.data.join("raw.db").is_file() {
            assert!(Instant::now() < deadline, "the hub creates raw.db");
            thread::sleep(POLL);
        }
        self.raw_db()
    }

    /// For every generation file on disk, how many bytes it holds past its committed segments.
    fn bytes_past_committed(&self) -> u64 {
        let db = self.raw_db();
        let mut past = 0;
        for stream in fs::read_dir(self.data.join("streams")).expect("streams folder exists") {
            let stream = stream.expect("directory entry is readable").path();
            let id: i64 = stream
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.parse().ok())
                .expect("stream folders are named by row ID");
            for log in fs::read_dir(&stream).expect("stream folder is readable") {
                let log = log.expect("directory entry is readable").path();
                let generation: i64 = log
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .and_then(|stem| stem.parse().ok())
                    .expect("generation files are named by generation");
                let committed: i64 = db
                    .query_row(
                        "SELECT COALESCE(MAX(end_offset), 0) FROM segments WHERE stream = ?1 AND generation = ?2",
                        [id, generation],
                        |row| row.get(0),
                    )
                    .expect("committed query runs");
                let len = fs::metadata(&log).expect("generation file exists").len();
                past += len.saturating_sub(committed as u64);
            }
        }
        past
    }

    /// Waits until the hub holds every file under `root` for `origin`, then compares them byte for byte.
    fn wait_until_copied(&self, origin: &str, root: &Path) {
        self.wait_until_synced(origin, &claude_code(root));
    }

    /// Waits until the hub holds every stream the sources feed for `origin`, then compares them byte for byte.
    fn wait_until_synced(&self, origin: &str, sources: &[(Source, PathBuf)]) {
        let deadline = Instant::now() + CONVERGE;
        loop {
            let due = due(sources);
            let behind = behind(&self.data, origin, &due);
            if behind.is_empty() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the hub caught up with {sources:?} within {CONVERGE:?}, except {behind:?}"
            );
            thread::sleep(POLL);
        }
        let (copies, sources) = (hub_copies(&self.data, origin), source_copies(sources));
        assert_eq!(
            copies.keys().collect::<Vec<_>>(),
            sources.keys().collect::<Vec<_>>(),
            "the hub has one stream of {origin} per source file and thread"
        );
        for (locator, source) in &sources {
            let copy = &copies[locator];
            let first_difference = copy.iter().zip(source).position(|(a, b)| a != b);
            assert!(
                copy == source,
                "{locator:?}: the hub holds {} bytes and the source {}, first differing at {first_difference:?}",
                copy.len(),
                source.len()
            );
        }
    }

    /// The folder where every collector this test starts keeps its state.
    fn state_dir(&self) -> PathBuf {
        self.temp.path().join("prime-radiant")
    }

    /// The state file of the collector for `origin`, once it has written one.
    fn collector_state(&self, origin: &str) -> Option<Value> {
        let file = self.state_dir().join(format!("collect-{origin}.json"));
        serde_json::from_slice(&fs::read(file).ok()?).ok()
    }

    /// Waits until the collector for `origin` reports `phase`, and returns its state.
    fn wait_for_phase(&self, origin: &str, phase: &str) -> Value {
        let deadline = Instant::now() + CONVERGE;
        loop {
            if let Some(state) = self.collector_state(origin)
                && state["phase"]["name"] == phase
            {
                return state;
            }
            assert!(
                Instant::now() < deadline,
                "the collector reached {phase}, not {:?}",
                self.collector_state(origin)
            );
            thread::sleep(POLL);
        }
    }

    /// The stdout of `status` over this test's state folder, with `args`. No variable names a hub.
    fn status(&self, args: &[&str]) -> String {
        let out = self.temp.path().join("status.out");
        let mut child = Command::new(BIN)
            .args(["status", "--state-dir", path(&self.state_dir())])
            .args(args)
            .env_remove("PRIME_RADIANT_HUB")
            .stdin(Stdio::null())
            .stdout(File::create(&out).expect("status output file is created"))
            .stderr(Stdio::inherit())
            .spawn()
            .expect("the built binary starts");
        let deadline = Instant::now() + Duration::from_secs(30);
        let exit = loop {
            if let Some(exit) = child.try_wait().expect("the child's status reads") {
                break exit;
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("status {args:?} kept running");
            }
            thread::sleep(Duration::from_millis(20));
        };
        assert!(exit.success(), "status {args:?} succeeds, not {exit}");
        fs::read_to_string(out).expect("status output reads")
    }

    fn status_json(&self, args: &[&str]) -> Value {
        let out = self.status(&[&["--json"], args].concat());
        serde_json::from_str(&out).expect("status --json prints one JSON object")
    }

    /// Every generation's segments start at 0, each starts where the last ended, and the current one ends at the
    /// confirmed offset, so no byte range is missing, overlapping, or stored twice.
    fn assert_segments_contiguous(&self) -> usize {
        let db = self.raw_db();
        let mut query = db
            .prepare(
                "SELECT st.locator, s.generation, s.start_offset, s.end_offset, st.generation, st.confirmed_offset
                 FROM segments s JOIN streams st ON st.id = s.stream
                 ORDER BY st.id, s.generation, s.start_offset",
            )
            .expect("segment query prepares");
        let rows = query
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                ))
            })
            .expect("segment query runs")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("segment rows read");
        let mut ends: HashMap<(String, i64), (i64, i64, i64)> = HashMap::new();
        for (locator, generation, start, end, current, confirmed) in &rows {
            let last = ends
                .entry((locator.clone(), *generation))
                .or_insert((0, *current, *confirmed));
            assert_eq!(
                *start, last.0,
                "{locator} generation {generation}: a segment starts where the previous one ended"
            );
            assert!(end > start, "{locator}: segments are not empty");
            last.0 = *end;
        }
        for ((locator, generation), (end, current, confirmed)) in &ends {
            if generation == current {
                assert_eq!(
                    end, confirmed,
                    "{locator}: the segments end at the confirmed offset"
                );
            }
        }
        rows.len()
    }
}

/// A child process that is killed when the test ends, and whose stderr is printed if the test fails.
struct Proc {
    name: String,
    child: Child,
    log: PathBuf,
}

impl Proc {
    fn spawn(name: &str, args: &[&str], logs: &Path) -> Self {
        let log = logs.join(format!("{name}.log"));
        let stderr = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)
            .expect("log file opens");
        // A collector keeps its state under XDG_STATE_HOME, so each test's stays in its own folder.
        let child = Command::new(BIN)
            .args(args)
            .env("XDG_STATE_HOME", logs)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr)
            .spawn()
            .expect("the built binary starts");
        Self {
            name: name.to_owned(),
            child,
            log,
        }
    }

    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    fn signal(&self, signal: &str) {
        send_signal(self.child.id(), signal);
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        self.kill();
        if thread::panicking() {
            eprintln!(
                "--- {} stderr ---\n{}",
                self.name,
                fs::read_to_string(&self.log).unwrap_or_default()
            );
        }
    }
}

fn send_signal(pid: u32, signal: &str) {
    let status = Command::new("kill")
        .arg(format!("-{signal}"))
        .arg(pid.to_string())
        .status()
        .expect("kill runs");
    assert!(status.success(), "kill -{signal} {pid} succeeds");
}

/// A connection to the hub's socket, speaking the protocol directly.
struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Client {
    fn connect(data: &Path) -> Self {
        let writer = UnixStream::connect(data.join("hub.sock")).expect("the hub accepts");
        writer
            .set_read_timeout(Some(Duration::from_secs(60)))
            .expect("timeout is set");
        let reader = BufReader::new(writer.try_clone().expect("socket clones"));
        Self { reader, writer }
    }

    fn hello(data: &Path, origin: &str, role: Role) -> Self {
        let mut client = Self::connect(data);
        let hello = Request::Hello {
            protocol: protocol::VERSION,
            origin: origin.to_owned(),
            role,
        };
        assert_eq!(
            client.call(hello),
            Some(Response::Hello {
                protocol: protocol::VERSION
            })
        );
        client
    }

    /// The response, or `None` if the hub closed the connection instead.
    fn call(&mut self, request: Request) -> Option<Response> {
        write_request(&mut self.writer, &request).expect("request is sent");
        self.next()
    }

    fn next(&mut self) -> Option<Response> {
        read_response(&mut self.reader).expect("response frame decodes")
    }

    fn find_keys(&mut self, query: &str) -> Vec<String> {
        let request = Request::Ask(Ask::Find(Find {
            query: query.to_owned(),
            limit: 10,
            ..Find::default()
        }));
        match self.call(request) {
            Some(Response::Found {
                hits: Value::Array(hits),
            }) => hits
                .iter()
                .map(|hit| hit["key"].as_str().expect("a hit has a key").to_owned())
                .collect(),
            other => panic!("find answers with hits, not {other:?}"),
        }
    }
}

fn claude_code(root: &Path) -> Vec<(Source, PathBuf)> {
    vec![(Source::ClaudeCode, root.to_owned())]
}

/// A stream by its source's name and its locator.
type Locator = (&'static str, String);

/// What the hub must confirm for a stream to match its source as it is now.
#[derive(Debug)]
enum Due {
    /// An append stream: every byte of the file.
    Len(u64),
    /// A replace stream: the file or thread snapshot with this checksum.
    Checksum(Sha256),
}

/// What the hub must confirm for each stream the sources feed. A file that vanishes during the scan is left out.
fn due(sources: &[(Source, PathBuf)]) -> BTreeMap<Locator, Due> {
    let mut due = BTreeMap::new();
    for (source, path) in sources {
        let Some(layout) = collect::layout(*source) else {
            for thread in t3_code::snapshots(path).expect("the database reads") {
                due.insert(
                    (source.as_str(), thread.locator),
                    Due::Checksum(Sha256::of(&thread.bytes)),
                );
            }
            continue;
        };
        for file in layout.scan(path).expect("source root scans") {
            let now = match file.write_mode {
                WriteMode::Append => fs::metadata(&file.path).map(|meta| Due::Len(meta.len())),
                WriteMode::Replace => {
                    fs::read(&file.path).map(|bytes| Due::Checksum(Sha256::of(&bytes)))
                }
            };
            if let Ok(now) = now {
                due.insert((source.as_str(), file.locator), now);
            }
        }
    }
    due
}

fn caught_up(data: &Path, origin: &str, sources: &[(Source, PathBuf)]) -> bool {
    behind(data, origin, &due(sources)).is_empty()
}

fn confirmed_now(data: &Path, origin: &str, due: &BTreeMap<Locator, Due>) -> bool {
    behind(data, origin, due).is_empty()
}

/// The streams the hub has not confirmed as `due` requires.
fn behind<'a>(data: &Path, origin: &str, due: &'a BTreeMap<Locator, Due>) -> Vec<&'a Locator> {
    let Ok(streams) = View::open(data).and_then(|view| view.streams()) else {
        return due.keys().collect();
    };
    let states: HashMap<Locator, Progress> = streams
        .into_iter()
        .filter(|stream| stream.key.origin == origin)
        .map(|stream| {
            (
                (stream.key.source.as_str(), stream.key.locator),
                stream.state.progress,
            )
        })
        .collect();
    due.iter()
        .filter(|(locator, due)| match (due, states.get(*locator)) {
            (
                Due::Len(len),
                Some(Progress::Append {
                    confirmed_offset, ..
                }),
            ) => confirmed_offset != len,
            (Due::Checksum(sha256), Some(Progress::Replace { latest_checksum })) => {
                latest_checksum != sha256
            }
            _ => true,
        })
        .map(|(locator, _)| locator)
        .collect()
}

/// The bytes of every source file the raw layer keeps, and of every thread snapshot.
fn source_copies(sources: &[(Source, PathBuf)]) -> BTreeMap<Locator, Vec<u8>> {
    let mut copies = BTreeMap::new();
    for (source, path) in sources {
        let Some(layout) = collect::layout(*source) else {
            for thread in t3_code::snapshots(path).expect("the database reads") {
                copies.insert((source.as_str(), thread.locator), thread.bytes);
            }
            continue;
        };
        for file in layout.scan(path).expect("source root scans") {
            let bytes = fs::read(&file.path).expect("source file reads");
            copies.insert((source.as_str(), file.locator), bytes);
        }
    }
    copies
}

/// Every stream of the origin, with the bytes the hub confirmed.
fn hub_copies(data: &Path, origin: &str) -> BTreeMap<Locator, Vec<u8>> {
    let view = View::open(data).expect("raw layer opens");
    view.streams()
        .expect("streams list")
        .into_iter()
        .filter(|stream| stream.key.origin == origin)
        .map(|stream| {
            let bytes = match stream.state.progress {
                Progress::Append {
                    confirmed_offset: 0,
                    ..
                } => Vec::new(),
                Progress::Append {
                    confirmed_offset, ..
                } => view
                    .read(&RawRef {
                        stream: stream.id,
                        generation: stream.state.generation,
                        start: 0,
                        end: confirmed_offset,
                    })
                    .expect("confirmed bytes read"),
                Progress::Replace { latest_checksum } => {
                    view.version(latest_checksum).expect("latest version reads")
                }
            };
            ((stream.key.source.as_str(), stream.key.locator), bytes)
        })
        .collect()
}

/// A made-up transcript of about `size` bytes: a user request, then filler lines that carry no message, then an
/// answer. The filler keeps indexing cheap, since these tests are about moving bytes.
fn write_transcript(root: &Path, project: &str, session: &str, size: usize, topic: &str) -> String {
    let dir = root.join(project);
    fs::create_dir_all(&dir).expect("project folder is created");
    let filler = "lorem ipsum dolor sit amet ".repeat(600);
    // Message IDs are unique per session, because a message ID that two sessions share marks a copied message.
    let id = |n: u64| format!("{}{n:012x}", &session[..24]);
    let mut text = line(
        session,
        "user",
        &id(1),
        None,
        json!(format!(
            "Please look into the {topic} for the example project."
        )),
    );
    let mut i = 0;
    while text.len() < size {
        i += 1;
        let progress =
            json!({"type": "progress", "sessionId": session, "data": format!("step {i} {filler}")});
        text += &format!("{progress}\n");
    }
    text += &line(
        session,
        "assistant",
        &id(2),
        Some(&id(1)),
        json!([{"type": "text", "text": format!("Done with the {topic}.")}]),
    );
    fs::write(dir.join(format!("{session}.jsonl")), &text).expect("transcript is written");
    format!("{project}/{session}.jsonl")
}

/// Appends about `size` bytes of filler lines to the transcript at `path` and returns its new length.
fn grow_transcript(path: &Path, session: &str, size: usize) -> u64 {
    let filler = "lorem ipsum dolor sit amet ".repeat(600);
    let mut text = String::new();
    let mut i = 0;
    while text.len() < size {
        i += 1;
        let progress =
            json!({"type": "progress", "sessionId": session, "data": format!("more {i} {filler}")});
        text += &format!("{progress}\n");
    }
    let mut file = OpenOptions::new()
        .append(true)
        .open(path)
        .expect("transcript opens for append");
    file.write_all(text.as_bytes()).expect("filler is appended");
    file.metadata().expect("transcript has metadata").len()
}

fn line(session: &str, kind: &str, uuid: &str, parent: Option<&str>, content: Value) -> String {
    let line = json!({
        "parentUuid": parent,
        "isSidechain": false,
        "cwd": "/home/dev/example-project",
        "sessionId": session,
        "type": kind,
        "message": {"role": kind, "content": content},
        "uuid": uuid,
        "timestamp": "2026-03-01T09:00:00.000Z",
    });
    format!("{line}\n")
}

/// A made-up T3 Code database with two threads, held open by a writer the way a running T3 Code holds it.
fn t3_database(path: &Path) -> Connection {
    let db = Connection::open(path).expect("example database is created");
    db.pragma_update(None, "journal_mode", "WAL")
        .expect("example database uses a write-ahead log");
    db.execute_batch(
        "CREATE TABLE projection_projects (project_id TEXT PRIMARY KEY, workspace_root TEXT);
         CREATE TABLE orchestration_v2_projection_threads (
             thread_id TEXT PRIMARY KEY, project_id TEXT, title TEXT, created_at TEXT, archived_at TEXT,
             deleted_at TEXT
         );
         CREATE TABLE orchestration_v2_projection_messages (
             message_id TEXT PRIMARY KEY, thread_id TEXT, role TEXT, created_at TEXT, payload_json TEXT
         );
         CREATE TABLE orchestration_v2_projection_provider_threads (
             provider_thread_id TEXT PRIMARY KEY, thread_id TEXT, provider TEXT, payload_json TEXT
         );
         INSERT INTO projection_projects VALUES ('example-project', '/home/dev/example-project');",
    )
    .expect("schema is created");
    for (thread, title) in THREADS
        .iter()
        .zip(["Add a JSON output flag", "Draft the release notes"])
    {
        db.execute(
            "INSERT INTO orchestration_v2_projection_threads (thread_id, project_id, title, created_at)
             VALUES (?1, 'example-project', ?2, '2026-01-15T10:00:00.000Z')",
            params![thread, title],
        )
        .expect("thread is inserted");
        t3_message(&db, thread, 1, &format!("A made-up request: {title}."));
    }
    db
}

fn t3_message(db: &Connection, thread: &str, n: u32, text: &str) {
    db.execute(
        "INSERT INTO orchestration_v2_projection_messages VALUES (?1, ?2, 'user', ?3, ?4)",
        params![
            format!("{thread}-{n}"),
            thread,
            format!("2026-01-15T10:00:{n:02}.000Z"),
            json!({ "text": text }).to_string()
        ],
    )
    .expect("message is inserted");
}

fn append(path: &Path, text: &str) {
    OpenOptions::new()
        .append(true)
        .open(path)
        .expect("file opens for append")
        .write_all(text.as_bytes())
        .expect("text is appended");
}

/// Every frame in a recording of one direction of a connection. A recording can end inside a frame.
fn recorded<T>(
    path: &Path,
    read: impl Fn(&mut BufReader<File>) -> io::Result<Option<T>>,
) -> Vec<T> {
    let mut input = BufReader::new(File::open(path).expect("recording opens"));
    let mut frames = Vec::new();
    while let Ok(Some(frame)) = read(&mut input) {
        frames.push(frame);
    }
    frames
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

fn quote(path: &Path) -> String {
    format!("'{}'", self::path(path).replace('\'', r"'\''"))
}

#[test]
fn initial_sync_copies_every_file_byte_for_byte() {
    let setup = Setup::new();
    let root = setup.fixture_root("projects");
    let large = write_transcript(
        &root,
        "-home-dev-generated-project",
        LARGE_ID,
        9 << 20,
        "build cache",
    );
    let _hub = setup.hub();
    let _collector = setup.collector("collector", ORIGIN, &root, &setup.bridge_command());

    setup.wait_until_copied(ORIGIN, &root);
    setup.assert_segments_contiguous();
    assert!(
        setup.segment_count(&large) >= 2,
        "the 9 MB transcript arrived in several segments"
    );
    let mode = fs::metadata(setup.data.join("hub.sock"))
        .expect("the socket exists")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600, "only the owner can reach the socket");
}

#[test]
fn collector_killed_mid_sync_resumes_without_gaps_or_duplicates() {
    let setup = Setup::new();
    let root = setup.fixture_root("projects");
    let large = write_transcript(
        &root,
        "-home-dev-generated-project",
        LARGE_ID,
        LARGE_BYTES,
        "build cache",
    );
    let len = fs::metadata(root.join(&large))
        .expect("transcript exists")
        .len();
    let _hub = setup.hub();

    let mut collector = setup.collector("collector", ORIGIN, &root, &setup.bridge_command());
    setup.wait_for_partial(&large, len);
    collector.kill();
    let (_, _, at_kill) = setup
        .confirmed(&setup.raw_db(), &large)
        .expect("the stream exists");
    assert!(
        at_kill < len,
        "the collector died with {at_kill} of {len} bytes confirmed"
    );

    let (requests, responses) = (
        setup.temp.path().join("requests.bin"),
        setup.temp.path().join("responses.bin"),
    );
    let recording = format!(
        "tee {} | {} | tee {}",
        quote(&requests),
        setup.bridge_command(),
        quote(&responses)
    );
    let _restarted = setup.collector("collector-restarted", ORIGIN, &root, &recording);
    setup.wait_until_copied(ORIGIN, &root);
    setup.assert_segments_contiguous();

    let resume_at = recorded(&responses, read_response)
        .into_iter()
        .find_map(|response| match response {
            Response::State { streams } => streams
                .into_iter()
                .find(|(key, _)| key.locator == large)
                .map(|(_, state)| state.progress),
            _ => None,
        });
    let Some(Progress::Append {
        confirmed_offset: resume_at,
        ..
    }) = resume_at
    else {
        panic!("the hub told the restarted collector its state for the large stream");
    };
    assert!(resume_at >= at_kill, "the hub kept what it confirmed");
    let mut next = resume_at;
    for request in recorded(&requests, read_request) {
        match request {
            Request::Write(Write::Segment {
                key, start, bytes, ..
            }) if key.locator == large => {
                assert_eq!(
                    start, next,
                    "the restarted collector sends only bytes the hub lacks, in order"
                );
                next += bytes.len() as u64;
            }
            Request::Write(other) => panic!(
                "the restarted collector resent a write to {}, which the hub already had",
                other.key().locator
            ),
            _ => {}
        }
    }
    assert_eq!(
        next, len,
        "the restarted collector sent the rest of the file"
    );
}

#[test]
fn killed_bridge_makes_the_collector_reconnect_and_converge() {
    let setup = Setup::new();
    let root = setup.fixture_root("projects");
    let large = write_transcript(
        &root,
        "-home-dev-generated-project",
        LARGE_ID,
        LARGE_BYTES,
        "build cache",
    );
    let len = fs::metadata(root.join(&large))
        .expect("transcript exists")
        .len();
    let _hub = setup.hub();
    let pids = setup.temp.path().join("bridge.pids");
    let command = format!(
        "echo $$ >> {}; exec {}",
        quote(&pids),
        setup.bridge_command()
    );
    let _collector = setup.collector("collector", ORIGIN, &root, &command);

    setup.wait_for_partial(&large, len);
    let first_bridge: u32 = fs::read_to_string(&pids)
        .expect("the bridge wrote its pid")
        .lines()
        .next()
        .and_then(|pid| pid.parse().ok())
        .expect("the pid file starts with a pid");
    send_signal(first_bridge, "KILL");

    setup.wait_until_copied(ORIGIN, &root);
    setup.assert_segments_contiguous();
    let bridges = fs::read_to_string(&pids)
        .expect("pid file reads")
        .lines()
        .count();
    assert!(
        bridges >= 2,
        "the collector started a new bridge ({bridges} in all)"
    );
}

#[test]
fn hub_killed_mid_write_restarts_with_nothing_past_the_committed_offsets() {
    let setup = Setup::new();
    let root = setup.fixture_root("projects");
    let large = write_transcript(
        &root,
        "-home-dev-generated-project",
        LARGE_ID,
        LARGE_BYTES,
        "build cache",
    );
    let mut len = fs::metadata(root.join(&large))
        .expect("transcript exists")
        .len();
    let mut hub = setup.hub();
    let collector = setup.collector("collector", ORIGIN, &root, &setup.bridge_command());

    let db = setup.raw_db_when_ready();
    let deadline = Instant::now() + CONVERGE;
    let mut torn_writes = 0;
    while torn_writes == 0 {
        assert!(Instant::now() < deadline, "the hub was caught mid-write");
        let Some((id, generation, offset)) = setup.confirmed(&db, &large) else {
            thread::sleep(Duration::from_millis(1));
            continue;
        };
        if offset >= len {
            // A fast machine can write the whole stream between two looks, so give the hub more to write.
            len = grow_transcript(&root.join(&large), LARGE_ID, LARGE_BYTES);
            continue;
        }
        let log = setup
            .data
            .join("streams")
            .join(id.to_string())
            .join(format!("{generation}.log"));
        if fs::metadata(&log).map_or(0, |meta| meta.len()) <= offset {
            thread::sleep(Duration::from_millis(1));
            continue;
        }
        // Freeze the collector so nothing new arrives while the restarted hub is inspected.
        collector.signal("STOP");
        hub.kill();
        torn_writes = setup.bytes_past_committed();
        hub = setup.hub();
        assert_eq!(
            setup.bytes_past_committed(),
            0,
            "the restarted hub trimmed every byte past the committed offsets"
        );
        collector.signal("CONT");
    }

    setup.wait_until_copied(ORIGIN, &root);
    setup.assert_segments_contiguous();
}

#[test]
fn appended_line_becomes_findable_through_the_hub() {
    let setup = Setup::new();
    let root = setup.fixture_root("projects");
    let _hub = setup.hub();
    let _collector = setup.collector("collector", ORIGIN, &root, &setup.bridge_command());
    setup.wait_until_copied(ORIGIN, &root);

    let word = "quokkalantern";
    let mut client = Client::hello(&setup.data, "example-reader", Role::Client);
    assert!(
        client.find_keys(word).is_empty(),
        "no session mentions {word} yet"
    );

    let transcript = root
        .join("-home-dev-example-project")
        .join(format!("{SESSION_ID}.jsonl"));
    let appended = line(
        SESSION_ID,
        "user",
        "c0000000-0000-4000-8000-0000000000a1",
        Some("1ba1c71c-5c5a-4d58-aa20-8f86f83a4023"),
        json!(format!("Also rename the {word} option.")),
    );
    OpenOptions::new()
        .append(true)
        .open(&transcript)
        .expect("transcript opens for append")
        .write_all(appended.as_bytes())
        .expect("line is appended");
    let start = Instant::now();

    let mut on_hub = None;
    loop {
        if on_hub.is_none() && caught_up(&setup.data, ORIGIN, &claude_code(&root)) {
            on_hub = Some(start.elapsed());
        }
        if client.find_keys(word).iter().any(|key| key == MAIN_KEY) {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "{word} became findable"
        );
        thread::sleep(Duration::from_millis(20));
    }
    let findable = start.elapsed();
    println!(
        "append to findable through the hub: {} ms (confirmed in the raw layer after {} ms)",
        findable.as_millis(),
        on_hub.unwrap_or(findable).as_millis()
    );
    assert!(
        findable < Duration::from_secs(5),
        "decision 0003: a new line is findable on the hub within 5 seconds, took {findable:?}"
    );
}

#[test]
fn two_collectors_with_different_origins_write_at_once() {
    let setup = Setup::new();
    let roots = [
        (
            "origin-a",
            "machine-a",
            "a1b2c3d4-0000-4000-8000-00000000000a",
            "release notes",
        ),
        (
            "origin-b",
            "machine-b",
            "b1b2c3d4-0000-4000-8000-00000000000b",
            "flaky timer",
        ),
    ];
    for (_, name, session, topic) in roots {
        let root = setup.temp.path().join(name);
        write_transcript(&root, "-home-dev-shared-project", session, 24 << 20, topic);
    }
    let hub = setup.hub();
    // Paused, the hub still accepts connections, so both collectors are waiting on it when it resumes.
    hub.signal("STOP");
    let pids = setup.temp.path().join("bridge.pids");
    let command = format!(
        "echo $$ >> {}; exec {}",
        quote(&pids),
        setup.bridge_command()
    );
    let _collectors: Vec<Proc> = roots
        .iter()
        .map(|(origin, name, _, _)| {
            setup.collector(name, origin, &setup.temp.path().join(name), &command)
        })
        .collect();
    let deadline = Instant::now() + CONVERGE;
    while fs::read_to_string(&pids).map_or(0, |pids| pids.lines().count()) < roots.len() {
        assert!(Instant::now() < deadline, "both collectors start a bridge");
        thread::sleep(Duration::from_millis(20));
    }
    thread::sleep(Duration::from_millis(500));
    hub.signal("CONT");

    for (origin, name, _, _) in roots {
        setup.wait_until_copied(origin, &setup.temp.path().join(name));
    }
    setup.assert_segments_contiguous();
    let db = setup.raw_db();
    let mut query = db
        .prepare(
            "SELECT st.origin FROM segments s JOIN streams st ON st.id = s.stream ORDER BY s.rowid",
        )
        .expect("commit order query prepares");
    let order = query
        .query_map([], |row| row.get::<_, String>(0))
        .expect("commit order query runs")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("commit order reads");
    let switches = order.windows(2).filter(|pair| pair[0] != pair[1]).count();
    assert!(
        switches >= 2,
        "the hub committed the two origins' segments interleaved: {order:?}"
    );

    let mut client = Client::hello(&setup.data, "example-reader", Role::Client);
    for (origin, _, session, _) in roots {
        let request = Request::State {
            origin: origin.to_owned(),
            source: Source::ClaudeCode,
        };
        let Some(Response::State { streams }) = client.call(request) else {
            panic!("state answers with streams");
        };
        let locators: Vec<&str> = streams
            .iter()
            .map(|(key, _)| key.locator.as_str())
            .collect();
        assert_eq!(
            locators,
            [format!("-home-dev-shared-project/{session}.jsonl")],
            "{origin} has only its own stream"
        );
    }
    for (_, _, session, topic) in roots {
        assert_eq!(
            client.find_keys(topic),
            [format!("claude-code/{session}")],
            "each origin's session is indexed on its own"
        );
    }
}

#[test]
fn bad_requests_close_only_their_own_connection() {
    let setup = Setup::new();
    let _hub = setup.hub();
    let foreign = StreamKey {
        origin: "origin-b".to_owned(),
        source: Source::ClaudeCode,
        locator: "-home-dev-example-project/session.jsonl".to_owned(),
    };
    let segment = Request::Write(Write::Segment {
        key: foreign,
        generation: 0,
        start: 0,
        sha256: Sha256::of(b"{}\n"),
        bytes: b"{}\n".to_vec(),
    });

    let mut garbage = Client::connect(&setup.data);
    garbage
        .writer
        .write_all(b"\x00\x00\x00\x09not json!\x00\x00\x00\x00")
        .expect("garbage is sent");
    let cases = [
        ("a malformed frame", garbage, None),
        (
            "a request before hello",
            Client::connect(&setup.data),
            Some(Request::Ask(Ask::Stats)),
        ),
        (
            "a newer protocol",
            Client::connect(&setup.data),
            Some(Request::Hello {
                protocol: protocol::VERSION + 1,
                origin: ORIGIN.to_owned(),
                role: Role::Collector,
            }),
        ),
        (
            "a write from a client",
            Client::hello(&setup.data, "origin-b", Role::Client),
            Some(segment.clone()),
        ),
        (
            "a write to another origin",
            Client::hello(&setup.data, "origin-a", Role::Collector),
            Some(segment),
        ),
    ];
    for (case, mut client, request) in cases {
        let response = match request {
            Some(request) => client.call(request),
            None => client.next(),
        };
        assert!(
            matches!(response, Some(Response::Error { .. })),
            "{case} gets an error, not {response:?}"
        );
        assert_eq!(client.next(), None, "{case} closes the connection");
    }

    let mut client = Client::hello(&setup.data, "example-reader", Role::Client);
    assert!(
        matches!(
            client.call(Request::Ask(Ask::Stats)),
            Some(Response::Stats { .. })
        ),
        "the hub still answers new connections"
    );
}

#[test]
fn a_second_writer_cannot_open_a_served_data_directory() {
    let setup = Setup::new();
    let root = setup.fixture_root("projects");
    let _hub = setup.hub();
    let data = path(&setup.data);
    for args in [
        vec!["hub", "serve", "--data", data],
        vec![
            "import",
            "--data",
            data,
            "--origin",
            ORIGIN,
            "--claude-code",
            path(&root),
        ],
    ] {
        let log = setup.temp.path().join("second-writer.log");
        let mut child = Command::new(BIN)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(File::create(&log).expect("log file is created"))
            .spawn()
            .expect("the built binary starts");
        let deadline = Instant::now() + Duration::from_secs(30);
        let status = loop {
            if let Some(status) = child.try_wait().expect("the child's status reads") {
                break status;
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("{args:?} kept running beside the hub");
            }
            thread::sleep(POLL);
        };
        let stderr = fs::read_to_string(&log).expect("log reads");
        assert!(!status.success(), "{args:?} exits with an error");
        assert!(
            stderr.contains("another process is writing"),
            "{args:?} says why: {stderr}"
        );
    }
    let mut client = Client::hello(&setup.data, "example-reader", Role::Client);
    assert!(
        matches!(
            client.call(Request::Ask(Ask::Stats)),
            Some(Response::Stats { .. })
        ),
        "the hub keeps serving"
    );
}

#[test]
fn a_write_the_hub_cannot_store_skips_only_that_file() {
    let setup = Setup::new();
    let root = setup.fixture_root("projects");
    let _hub = setup.hub();
    // With streams/ read-only the hub cannot start a transcript's log, but blobs/ still takes rewritten files.
    let streams = setup.data.join("streams");
    fs::set_permissions(&streams, Permissions::from_mode(0o500))
        .expect("streams/ is made read-only");
    let _collector = setup.collector_with(
        "collector",
        ORIGIN,
        &root,
        &setup.bridge_command(),
        &["--sweep-seconds", "2"],
    );

    let (transcripts, rewritten): (BTreeMap<Locator, Due>, BTreeMap<Locator, Due>) =
        due(&claude_code(&root))
            .into_iter()
            .partition(|(_, due)| matches!(due, Due::Len(_)));
    let deadline = Instant::now() + CONVERGE;
    while !confirmed_now(&setup.data, ORIGIN, &rewritten) {
        assert!(
            Instant::now() < deadline,
            "the rewritten files reach the hub past the transcripts it cannot store"
        );
        thread::sleep(POLL);
    }
    assert!(
        !confirmed_now(&setup.data, ORIGIN, &transcripts),
        "the transcripts could not be stored yet"
    );

    fs::set_permissions(&streams, Permissions::from_mode(0o700))
        .expect("streams/ is writable again");
    setup.wait_until_copied(ORIGIN, &root);
}

#[test]
fn hub_indexes_past_a_line_whose_timestamp_used_to_stop_the_indexer() {
    let setup = Setup::new();
    let root = setup.fixture_root("projects");
    let _hub = setup.hub();
    let _collector = setup.collector("collector", ORIGIN, &root, &setup.bridge_command());
    setup.wait_until_copied(ORIGIN, &root);
    let mut client = Client::hello(&setup.data, "example-reader", Role::Client);
    let mut wait_until_found = |word: &str, key: &str| {
        let deadline = Instant::now() + CONVERGE;
        while !client.find_keys(word).iter().any(|hit| hit == key) {
            assert!(Instant::now() < deadline, "{key} became findable by {word}");
            thread::sleep(POLL);
        }
    };

    let session = "d1e2f3a4-0000-4000-8000-0000000000ff";
    let project = root.join("-home-dev-zz-project");
    fs::create_dir_all(&project).expect("project folder is created");
    // Byte 3 of the fraction falls inside a two-byte character. The parser used to slice there and panic.
    let broken = json!({
        "type": "user",
        "uuid": "f0000000-0000-4000-8000-000000000001",
        "parentUuid": null,
        "sessionId": session,
        "message": {"role": "user", "content": "A made-up request about the zebracorn flag."},
        "timestamp": "2026-03-01T09:00:00.12\u{e9}Z",
    });
    fs::write(
        project.join(format!("{session}.jsonl")),
        format!("{broken}\n"),
    )
    .expect("transcript is written");
    setup.wait_until_copied(ORIGIN, &root);
    wait_until_found("zebracorn", &format!("claude-code/{session}"));

    append(
        &root
            .join("-home-dev-example-project")
            .join(format!("{SESSION_ID}.jsonl")),
        &line(
            SESSION_ID,
            "user",
            "c0000000-0000-4000-8000-0000000000b1",
            Some("1ba1c71c-5c5a-4d58-aa20-8f86f83a4023"),
            json!("Also rename the quokkalantern option."),
        ),
    );
    setup.wait_until_copied(ORIGIN, &root);
    wait_until_found("quokkalantern", MAIN_KEY);
}

/// inotify reports opens, so a collector that reacted to its own reads would re-read every file forever. A sweep
/// every second reads nothing either, because no file changed.
#[cfg(target_os = "linux")]
#[test]
fn idle_collector_reads_nothing() {
    let setup = Setup::new();
    let root = setup.fixture_root("projects");
    let output = root
        .join("-home-dev-example-project")
        .join(SESSION_ID)
        .join("tool-results")
        .join("toolu_made_up_large.txt");
    fs::write(&output, "made-up tool output\n".repeat(64 * 1024)).expect("tool output is written");
    let _hub = setup.hub();
    let collector = setup.collector_with(
        "collector",
        ORIGIN,
        &root,
        &setup.bridge_command(),
        &["--sweep-seconds", "1"],
    );
    setup.wait_until_copied(ORIGIN, &root);

    let read_bytes = || -> u64 {
        fs::read_to_string(format!("/proc/{}/io", collector.child.id()))
            .expect("the collector's I/O counters read")
            .lines()
            .find_map(|line| line.strip_prefix("rchar: "))
            .and_then(|count| count.trim().parse().ok())
            .expect("the counters include rchar")
    };
    thread::sleep(Duration::from_secs(3));
    let before = read_bytes();
    thread::sleep(Duration::from_secs(6));
    let read = read_bytes() - before;
    assert!(
        read < 256 * 1024,
        "an idle collector read {read} bytes in 6 seconds"
    );
}

#[test]
fn collector_watches_every_source_and_sends_each_change() {
    let setup = Setup::new();
    let claude = setup.fixture_root("projects");
    let (codex, grok, t3, writer) = setup.other_sources();
    let sources = vec![
        (Source::ClaudeCode, claude.clone()),
        (Source::Codex, codex.clone()),
        (Source::Grok, grok.clone()),
        (Source::T3Code, t3.clone()),
    ];
    let _hub = setup.hub();
    // The only sweep is the initial sync, so every later change has to come through the watcher, or for T3 Code,
    // through the poll of its database.
    let _collector = setup.collector_of(
        "collector",
        ORIGIN,
        &sources,
        &setup.bridge_command(),
        &["--sweep-seconds", "3600"],
    );
    setup.wait_until_synced(ORIGIN, &sources);

    let made_up = |id: &str| format!("{}\n", json!({"type": "made_up", "id": id}));
    append(
        &claude
            .join("-home-dev-example-project")
            .join(format!("{SESSION_ID}.jsonl")),
        &line(
            SESSION_ID,
            "user",
            "c0000000-0000-4000-8000-0000000000c1",
            Some("1ba1c71c-5c5a-4d58-aa20-8f86f83a4023"),
            json!("A made-up request."),
        ),
    );
    let live_rollout = codex
        .join(CODEX_DAY)
        .join(format!("rollout-2026-01-15T10-00-00-{CODEX_ID}.jsonl"));
    append(&live_rollout, &made_up("codex-append"));
    let new_day = codex.join("sessions/2026/01/16");
    fs::create_dir_all(&new_day).expect("a new day folder is created");
    fs::write(
        new_day.join("rollout-2026-01-16T09-00-00-9a8b7c6d-0000-4000-8000-000000000001.jsonl"),
        made_up("codex-new-day"),
    )
    .expect("a new rollout is written");
    // Codex creates its archive the first time it archives a thread, after the collector started.
    let archive = codex.join("archived_sessions");
    fs::create_dir(&archive).expect("the archive is created");
    fs::copy(
        live_rollout.with_extension("jsonl.zst"),
        archive.join(format!("rollout-2026-01-15T10-00-00-{CODEX_ID}.jsonl.zst")),
    )
    .expect("a finished rollout is archived");
    // Codex writes its session index in its home folder the first time it names a thread.
    fs::write(
        codex.join("session_index.jsonl"),
        format!(
            "{}\n",
            json!({"id": CODEX_ID, "thread_name": "Scriptable listing", "updated_at": "2026-01-15T10:30:00.000000Z"})
        ),
    )
    .expect("the session index is written");
    append(
        &grok.join(GROK_FOLDER).join(GROK_ID).join("updates.jsonl"),
        &made_up("grok-append"),
    );
    let new_session = grok
        .join(GROK_FOLDER)
        .join("3c9d8e7f-0000-4000-8000-000000000001");
    fs::create_dir(&new_session).expect("a new Grok session folder is created");
    fs::write(
        new_session.join("updates.jsonl"),
        made_up("grok-new-session"),
    )
    .expect("the new session's updates are written");
    t3_message(&writer, THREADS[0], 2, "A made-up follow-up.");

    setup.wait_until_synced(ORIGIN, &sources);
    assert_eq!(
        setup.version_count(&format!("threads/{}.json", THREADS[0])),
        2,
        "the changed T3 Code thread is sent again"
    );
    assert_eq!(
        setup.version_count(&format!("threads/{}.json", THREADS[1])),
        1,
        "the unchanged thread is not"
    );
    let mut client = Client::hello(&setup.data, "example-reader", Role::Client);
    for (id, key) in [
        (SESSION_ID.to_owned(), MAIN_KEY.to_owned()),
        (CODEX_ID.to_owned(), format!("codex/{CODEX_ID}")),
        ("scriptable".to_owned(), format!("codex/{CODEX_ID}")),
        (GROK_ID.to_owned(), format!("grok/{GROK_ID}")),
        (THREADS[0].to_owned(), format!("t3-code/{}", THREADS[0])),
    ] {
        assert_eq!(
            client.find_keys(&id).first(),
            Some(&key),
            "the hub finds {id}"
        );
    }
}

/// Every source file is made unreadable once it is on the hub, so any later read of one logs a failure.
#[test]
fn a_sweep_reads_no_file_whose_size_and_modification_time_are_unchanged() {
    let setup = Setup::new();
    let claude = setup.fixture_root("projects");
    let (_, _, t3, writer) = setup.other_sources();
    let sources = vec![
        (Source::ClaudeCode, claude.clone()),
        (Source::T3Code, t3.clone()),
    ];
    let _hub = setup.hub();
    let collector = setup.collector_of(
        "collector",
        ORIGIN,
        &sources,
        &setup.bridge_command(),
        &["--sweep-seconds", "1"],
    );
    setup.wait_until_synced(ORIGIN, &sources);

    let files = collect::claude_code::LAYOUT
        .scan(&claude)
        .expect("source root scans");
    let wal = t3.with_file_name("statev2.sqlite-wal");
    let unreadable: Vec<&Path> = files
        .iter()
        .map(|file| file.path.as_path())
        .chain([t3.as_path(), wal.as_path()])
        .collect();
    for path in &unreadable {
        fs::set_permissions(path, Permissions::from_mode(0o000))
            .expect("the file is made unreadable");
    }
    let failed_reads = || -> Vec<String> {
        fs::read_to_string(&collector.log)
            .expect("the collector's log reads")
            .lines()
            .filter(|line| {
                line.starts_with("collect: skipping") || line.starts_with("collect: cannot read")
            })
            .map(str::to_owned)
            .collect()
    };
    let wait_for = |what: &str, done: &dyn Fn(&[String]) -> bool| {
        let deadline = Instant::now() + CONVERGE;
        loop {
            let failed = failed_reads();
            if done(&failed) {
                return;
            }
            assert!(Instant::now() < deadline, "{what}: {failed:?}");
            thread::sleep(POLL);
        }
    };
    // The watcher reports each change of mode, and the collector reads a file the watcher reports whatever its size
    // and modification time.
    wait_for(
        "each transcript and rewritten file fails to read once",
        &|failed| {
            files.iter().all(|file| {
                failed
                    .iter()
                    .any(|line| line.contains(&format!(" {}: ", file.locator)))
            })
        },
    );
    thread::sleep(QUIET_AND_A_SWEEP);
    let before = failed_reads();
    thread::sleep(Duration::from_secs(4));
    let after = failed_reads();
    assert_eq!(
        after[before.len()..],
        [] as [String; 0],
        "sweeps every second, and database polls, read no file whose size and modification time stayed the same"
    );
    t3_message(&writer, THREADS[0], 2, "A made-up follow-up.");
    wait_for("a database whose log grew fails to read", &|failed| {
        failed[after.len()..]
            .iter()
            .any(|line| line.contains("statev2.sqlite"))
    });
    for path in &unreadable {
        fs::set_permissions(path, Permissions::from_mode(0o600))
            .expect("the file is readable again");
    }
}

#[test]
fn changes_made_while_the_hub_is_down_reach_it_once_it_is_back() {
    let setup = Setup::new();
    let claude = setup.fixture_root("projects");
    let (_, _, t3, writer) = setup.other_sources();
    let sources = vec![
        (Source::ClaudeCode, claude.clone()),
        (Source::T3Code, t3.clone()),
    ];
    let mut hub = setup.hub();
    let _collector = setup.collector_of(
        "collector",
        ORIGIN,
        &sources,
        &setup.bridge_command(),
        &["--sweep-seconds", "3600"],
    );
    setup.wait_until_synced(ORIGIN, &sources);

    hub.kill();
    let session = claude.join("-home-dev-example-project").join(SESSION_ID);
    append(
        &claude
            .join("-home-dev-example-project")
            .join(format!("{SESSION_ID}.jsonl")),
        &line(
            SESSION_ID,
            "user",
            "c0000000-0000-4000-8000-0000000000d1",
            Some("1ba1c71c-5c5a-4d58-aa20-8f86f83a4023"),
            json!("A made-up request."),
        ),
    );
    fs::write(
        session.join("custom-title.json"),
        r#"{"customTitle":"A renamed example session"}"#,
    )
    .expect("the title is rewritten");
    t3_message(&writer, THREADS[1], 2, "A made-up follow-up.");
    // The collector cannot send these yet, and a new connection starts from a sweep, so only the sweep can find
    // them. Each changed its file's size or modification time.
    thread::sleep(Duration::from_secs(2));
    hub = setup.hub();
    setup.wait_until_synced(ORIGIN, &sources);

    // A hub that lost its data has none of what the collector sent, so the collector sends all of it again.
    hub.kill();
    fs::remove_dir_all(&setup.data).expect("the hub's data is removed");
    let _hub = setup.hub();
    setup.wait_until_synced(ORIGIN, &sources);
}

/// The number of files and the bytes in all under the sources, which is what an empty hub lacks.
fn local_totals(sources: &[(Source, PathBuf)]) -> (u64, u64) {
    let copies = source_copies(sources);
    let bytes = copies.values().map(|bytes| bytes.len() as u64).sum();
    (copies.len() as u64, bytes)
}

fn time(value: &Value) -> Timestamp {
    value
        .as_str()
        .and_then(Timestamp::parse_utc)
        .unwrap_or_else(|| panic!("{value} is a UTC time"))
}

#[test]
fn the_state_file_shows_the_first_sync_rising_then_live_at_100_percent() {
    let setup = Setup::new();
    let root = setup.fixture_root("projects");
    write_transcript(
        &root,
        "-home-dev-generated-project",
        LARGE_ID,
        LARGE_BYTES,
        "build cache",
    );
    let (files, bytes) = local_totals(&claude_code(&root));
    let _hub = setup.hub();
    let _collector = setup.collector("collector", ORIGIN, &root, &setup.bridge_command());

    let deadline = Instant::now() + CONVERGE;
    let mut done = Vec::new();
    let live = loop {
        let state = setup.collector_state(ORIGIN).unwrap_or_default();
        match state["phase"]["name"].as_str() {
            Some("first_sync") => {
                let progress = &state["phase"]["progress"];
                assert_eq!(
                    progress["bytes_total"], bytes,
                    "the first sync is to send every byte the empty hub lacks"
                );
                assert_eq!(progress["files_total"], files);
                let confirmed = progress["bytes_done"]
                    .as_u64()
                    .expect("bytes done is a count");
                if done.last() != Some(&confirmed) {
                    done.push(confirmed);
                }
            }
            Some("live") => break state,
            _ => {}
        }
        assert!(
            Instant::now() < deadline,
            "the collector went live; the first sync showed {done:?}"
        );
        thread::sleep(Duration::from_millis(20));
    };
    assert!(
        done.windows(2).all(|pair| pair[0] < pair[1]),
        "bytes done only rose: {done:?}"
    );
    assert!(
        done.iter()
            .any(|&confirmed| confirmed > 0 && confirmed < bytes),
        "the state file showed the first sync part way through, not only {done:?}"
    );
    let first_sync = &live["phase"]["first_sync"];
    assert_eq!(
        (&first_sync["bytes_done"], &first_sync["bytes_total"]),
        (&json!(bytes), &json!(bytes)),
        "the first sync ended at 100%"
    );
    assert_eq!(
        (&first_sync["files_done"], &first_sync["files_total"]),
        (&json!(files), &json!(files))
    );
    assert_eq!(live["phase"]["files"], files, "live watches every file");
    assert_eq!(live["sent_bytes"], bytes);
}

#[test]
fn a_service_log_gets_a_line_for_each_phase() {
    let setup = Setup::new();
    let root = setup.fixture_root("projects");
    let _hub = setup.hub();
    let collector = setup.collector("collector", ORIGIN, &root, &setup.bridge_command());
    setup.wait_for_phase(ORIGIN, "live");

    let log = fs::read_to_string(&collector.log).expect("the collector's log reads");
    let (files, _) = local_totals(&claude_code(&root));
    let lines = [
        format!("collect: {ORIGIN} · connecting to the hub"),
        format!("collect: {ORIGIN} · scanning"),
        format!("collect: {ORIGIN} · first sync 0%"),
        format!("collect: {ORIGIN} · live · watching {files} files"),
    ];
    let mut rest = log.as_str();
    for line in &lines {
        let at = rest
            .find(line.as_str())
            .unwrap_or_else(|| panic!("the log has {line:?} after the lines before it:\n{log}"));
        rest = &rest[at + line.len()..];
    }
    assert!(
        !log.contains('\r'),
        "a log that is not a terminal gets whole lines, not redraws:\n{log}"
    );
}

#[test]
fn a_killed_hub_shows_the_collector_reconnecting_with_a_retry_time() {
    let setup = Setup::new();
    let root = setup.fixture_root("projects");
    let mut hub = setup.hub();
    let _collector = setup.collector("collector", ORIGIN, &root, &setup.bridge_command());
    setup.wait_for_phase(ORIGIN, "live");

    hub.kill();
    let state = setup.wait_for_phase(ORIGIN, "reconnecting");
    let phase = &state["phase"];
    assert!(
        time(&phase["retry_at"]) >= time(&state["updated_at"]),
        "the next try is still ahead in {state}"
    );
    assert!(
        phase["reason"]
            .as_str()
            .is_some_and(|reason| !reason.is_empty()),
        "the state says why: {state}"
    );
    let status = setup.status_json(&["--hub-socket", path(&setup.data.join("hub.sock"))]);
    let collector = &status["collectors"][0];
    assert_eq!(collector["running"], true);
    assert_eq!(collector["state"]["phase"]["name"], "reconnecting");
    assert!(
        status["hub"]["unavailable"]
            .as_str()
            .is_some_and(|why| why.contains("cannot reach the hub")),
        "status says why the hub part is missing: {status}"
    );

    let _hub = setup.hub();
    setup.wait_for_phase(ORIGIN, "live");
}

#[test]
fn a_killed_collector_shows_as_stopped_with_when_it_was_last_seen() {
    let setup = Setup::new();
    let root = setup.fixture_root("projects");
    let _hub = setup.hub();
    let mut collector = setup.collector("collector", ORIGIN, &root, &setup.bridge_command());
    setup.wait_for_phase(ORIGIN, "live");
    let pid = collector.child.id();
    collector.kill();

    let text = setup.status(&[]);
    assert!(
        text.contains(&format!("{ORIGIN}: stopped, last seen ")),
        "status shows the collector stopped:\n{text}"
    );
    assert!(
        text.contains("when last seen: live · watching"),
        "status shows what it was doing:\n{text}"
    );
    assert!(
        text.contains("hub: unavailable: no hub to ask"),
        "status says why the hub part is missing:\n{text}"
    );
    let status = setup.status_json(&[]);
    let collector = &status["collectors"][0];
    assert_eq!(collector["running"], false);
    assert_eq!(collector["state"]["pid"], pid);
    assert_eq!(collector["state"]["phase"]["name"], "live");
}

#[test]
fn status_reports_what_the_hub_holds_from_each_origin_and_its_last_write() {
    let setup = Setup::new();
    let root = setup.fixture_root("projects");
    let started = Timestamp(Timestamp::now().0 / 1000 * 1000);
    let mut hub = setup.hub();
    let mut collector = setup.collector("collector", ORIGIN, &root, &setup.bridge_command());
    setup.wait_until_copied(ORIGIN, &root);
    let (files, bytes) = local_totals(&claude_code(&root));

    let status = setup.status_json(&["--hub-socket", path(&setup.data.join("hub.sock"))]);
    let origins = status["hub"]["origins"]
        .as_array()
        .unwrap_or_else(|| panic!("the hub answers with its origins: {status}"));
    assert_eq!(origins.len(), 1, "one origin wrote: {status}");
    let origin = &origins[0];
    assert_eq!(
        (
            &origin["origin"],
            &origin["streams"],
            &origin["confirmed_bytes"]
        ),
        (&json!(ORIGIN), &json!(files), &json!(bytes)),
        "the hub holds one stream per file, every byte confirmed"
    );
    let last_write = time(&origin["last_write"]);
    assert!(
        started <= last_write && last_write <= Timestamp::now(),
        "the last write was during this test: {origin}"
    );

    collector.kill();
    hub.kill();
    let _hub = setup.hub();
    let status = setup.status_json(&["--data", path(&setup.data)]);
    let origin = &status["hub"]["origins"][0];
    assert_eq!(
        (
            &origin["streams"],
            &origin["confirmed_bytes"],
            &origin["last_write"]
        ),
        (&json!(files), &json!(bytes), &Value::Null),
        "a restarted hub keeps the streams and has seen no write: {status}"
    );
    let text = setup.status(&["--data", path(&setup.data)]);
    assert!(
        text.contains(&format!("{ORIGIN}: {files} streams"))
            && text.contains("no write since the hub started"),
        "status says so:\n{text}"
    );
}

#[test]
fn status_tells_an_older_or_silent_hub_apart_and_does_not_wait_forever() {
    let setup = Setup::new();
    let socket = setup.temp.path().join("older-hub.sock");
    let listener = UnixListener::bind(&socket).expect("the stand-in hub listens");
    // A hub from before the origins question answers hello, then fails to decode the question and closes.
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut reader = BufReader::new(stream.try_clone().expect("socket clones"));
            let _ = read_request(&mut reader);
            let hello = Response::Hello {
                protocol: protocol::VERSION,
            };
            let refusal = Response::Error {
                message: "bad frame: unknown variant `origins`, expected one of `hello`, `state`, `write`, `find`, `brief`, `stats`, `read`, `grep`".to_owned(),
            };
            let _ = write_response(&mut stream, &hello);
            let _ = write_response(&mut stream, &refusal);
        }
    });
    let status = setup.status_json(&["--hub-socket", path(&socket)]);
    assert!(
        status["hub"]["unavailable"]
            .as_str()
            .is_some_and(|why| why.contains("older than this client")),
        "status names an older hub as the likely cause: {status}"
    );

    let socket = setup.temp.path().join("silent-hub.sock");
    let listener = UnixListener::bind(&socket).expect("the stand-in hub listens");
    // Accepts and keeps each connection open, and never answers.
    thread::spawn(move || {
        let mut held = Vec::new();
        for stream in listener.incoming() {
            held.push(stream);
        }
    });
    let start = Instant::now();
    let status = setup.status_json(&["--hub-socket", path(&socket)]);
    assert!(
        start.elapsed() < Duration::from_secs(20),
        "status gave up on a silent hub after {:?}",
        start.elapsed()
    );
    assert!(
        status["hub"]["unavailable"]
            .as_str()
            .is_some_and(|why| why.contains("has not answered")),
        "status says the hub did not answer: {status}"
    );
}

#[test]
fn a_restarted_collector_counts_only_what_the_hub_lacks() {
    let setup = Setup::new();
    let root = setup.fixture_root("projects");
    let large = write_transcript(
        &root,
        "-home-dev-generated-project",
        LARGE_ID,
        LARGE_BYTES,
        "build cache",
    );
    let len = fs::metadata(root.join(&large))
        .expect("transcript exists")
        .len();
    let (files, bytes) = local_totals(&claude_code(&root));
    let _hub = setup.hub();
    let mut collector = setup.collector("collector", ORIGIN, &root, &setup.bridge_command());
    setup.wait_for_partial(&large, len);
    collector.kill();

    let status = setup.status_json(&["--hub-socket", path(&setup.data.join("hub.sock"))]);
    let held = status["hub"]["origins"][0]["confirmed_bytes"]
        .as_u64()
        .unwrap_or_else(|| panic!("the hub holds part of the origin: {status}"));
    assert!(
        0 < held && held < bytes,
        "the hub holds {held} of {bytes} bytes"
    );
    let restarted = setup.collector(
        "collector-restarted",
        ORIGIN,
        &root,
        &setup.bridge_command(),
    );
    let pid = restarted.child.id();

    let deadline = Instant::now() + CONVERGE;
    let mut totals = Vec::new();
    let live = loop {
        let state = setup.collector_state(ORIGIN).unwrap_or_default();
        match state["phase"]["name"].as_str() {
            _ if state["pid"] != pid => {}
            Some("first_sync") => {
                let progress = &state["phase"]["progress"];
                totals.push((
                    progress["bytes_total"].clone(),
                    progress["files_total"].clone(),
                ));
            }
            Some("live") => break state,
            _ => {}
        }
        assert!(
            Instant::now() < deadline,
            "the restarted collector went live"
        );
        thread::sleep(Duration::from_millis(20));
    };
    assert!(
        !totals.is_empty(),
        "the state file showed the restarted collector's first sync"
    );
    for (total, file_count) in &totals {
        assert_eq!(
            total,
            &json!(bytes - held),
            "the first sync counts only the bytes the hub lacked"
        );
        assert!(
            file_count.as_u64() < Some(files),
            "it leaves out the files the hub already had, but counts {file_count} of {files}"
        );
    }
    assert_eq!(live["phase"]["first_sync"]["bytes_done"], bytes - held);
}
