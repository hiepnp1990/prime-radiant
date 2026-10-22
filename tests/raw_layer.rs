use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use prime_radiant::collect::{self, LocalFile, claude_code, import_claude_code, plan};
use prime_radiant::raw::{
    Applied, Progress, Rejection, Sha256, Source, Store, StreamKey, Write, WriteMode,
};
use tempfile::TempDir;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/claude-code");
const ORIGIN: &str = "example-origin";
const SESSION: &str = "-home-dev-example-project/33831ab7-5c2a-48a4-a8d7-0b5125040f4d";

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

    fn store(&self) -> Store {
        Store::open(&self.data).expect("store opens")
    }

    fn path(&self, locator: &str) -> PathBuf {
        self.root.join(locator)
    }

    fn count(&self, table: &str) -> i64 {
        let db = rusqlite::Connection::open(self.data.join("raw.db")).expect("raw.db opens");
        db.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .expect("count query runs")
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

fn key(locator: &str) -> StreamKey {
    StreamKey {
        origin: ORIGIN.to_owned(),
        source: Source::ClaudeCode,
        locator: locator.to_owned(),
    }
}

fn main_transcript() -> String {
    format!("{SESSION}.jsonl")
}

fn custom_title() -> String {
    format!("{SESSION}/custom-title.json")
}

fn segment(key: &StreamKey, generation: u64, start: u64, bytes: &[u8]) -> Write {
    Write::Segment {
        key: key.clone(),
        generation,
        start,
        bytes: bytes.to_vec(),
        sha256: Sha256::of(bytes),
    }
}

fn import(setup: &Setup, store: &mut Store) -> collect::ImportSummary {
    let summary = import_claude_code(&setup.root, ORIGIN, store).expect("import runs");
    assert!(
        summary.rejected.is_empty(),
        "no write is rejected: {:?}",
        summary.rejected
    );
    summary
}

fn plan_for(setup: &Setup, store: &Store, locator: &str) -> Vec<Write> {
    let files: Vec<LocalFile> = claude_code::scan(&setup.root)
        .expect("scan runs")
        .into_iter()
        .filter(|file| file.locator == locator)
        .collect();
    assert_eq!(files.len(), 1, "{locator} is scanned");
    plan(
        ORIGIN,
        Source::ClaudeCode,
        &files[0],
        &store.state(ORIGIN).expect("state reads"),
    )
    .expect("plan runs")
}

#[test]
fn scan_classifies_transcripts_as_append_and_rewritten_files_as_replace() {
    let setup = Setup::new();
    fs::write(setup.path(&format!("{SESSION}/notes.txt")), "ignored")
        .expect("extra file is written");

    let scanned: Vec<(String, WriteMode)> = claude_code::scan(&setup.root)
        .expect("scan runs")
        .into_iter()
        .map(|file| (file.locator, file.write_mode))
        .collect();

    let expected = [
        (main_transcript(), WriteMode::Append),
        (custom_title(), WriteMode::Replace),
        (
            format!("{SESSION}/subagents/agent-a75ce3543dd257a2d.jsonl"),
            WriteMode::Append,
        ),
        (
            format!("{SESSION}/subagents/agent-a75ce3543dd257a2d.meta.json"),
            WriteMode::Replace,
        ),
        (
            format!("{SESSION}/tool-results/toolu_01XzYowh5RHpSrKco2mQp1Tm.txt"),
            WriteMode::Replace,
        ),
    ];
    assert_eq!(
        scanned, expected,
        "every kept file is scanned with its write mode, and notes.txt is ignored"
    );
}

#[test]
fn import_creates_one_stream_per_file_holding_the_exact_bytes() {
    let setup = Setup::new();
    let mut store = setup.store();

    let summary = import(&setup, &mut store);

    let files = claude_code::scan(&setup.root).expect("scan runs");
    let states = store.state(ORIGIN).expect("state reads");
    assert_eq!(states.len(), files.len(), "one stream per scanned file");
    assert_eq!(
        summary.files_scanned,
        files.len(),
        "summary counts the scanned files"
    );
    for file in &files {
        let state = &states[&key(&file.locator)];
        assert_eq!(
            state.write_mode(),
            file.write_mode,
            "{} keeps its write mode",
            file.locator
        );
        let source = fs::read(&file.path).expect("source file is readable");
        assert_eq!(
            store.contents(&key(&file.locator)).expect("contents read"),
            Some(source.clone()),
            "{} is stored byte for byte",
            file.locator
        );
        if let Progress::Append {
            confirmed_offset, ..
        } = state.progress
        {
            assert_eq!(
                confirmed_offset,
                source.len() as u64,
                "{} is confirmed to its end",
                file.locator
            );
        }
    }
}

#[test]
fn second_import_plans_nothing_and_changes_nothing() {
    let setup = Setup::new();
    let mut store = setup.store();
    import(&setup, &mut store);
    let before = (
        store.state(ORIGIN).expect("state reads"),
        setup.count("streams"),
        setup.count("segments"),
        setup.count("versions"),
    );

    let summary = import(&setup, &mut store);

    assert_eq!(
        summary.writes_planned, 0,
        "a second import plans zero writes"
    );
    let after = (
        store.state(ORIGIN).expect("state reads"),
        setup.count("streams"),
        setup.count("segments"),
        setup.count("versions"),
    );
    assert_eq!(
        after, before,
        "stream state, stream count, segment count, and version count are unchanged"
    );
}

#[test]
fn appended_line_sends_exactly_the_new_bytes() {
    let setup = Setup::new();
    let mut store = setup.store();
    import(&setup, &mut store);
    let locator = main_transcript();
    let path = setup.path(&locator);
    let old_len = fs::metadata(&path).expect("transcript exists").len();
    let line = b"{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"one more\"}}\n";
    OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("transcript opens for append")
        .write_all(line)
        .expect("line is appended");

    assert_eq!(
        plan_for(&setup, &store, &locator),
        vec![segment(&key(&locator), 0, old_len, line)],
        "the plan is one segment holding only the new line, starting at the old end"
    );

    let summary = import(&setup, &mut store);
    assert_eq!(summary.writes_planned, 1, "the import sends one write");
    assert_eq!(
        summary.bytes_appended,
        line.len() as u64,
        "the import appends only the new line"
    );
    assert_eq!(
        store.contents(&key(&locator)).expect("contents read"),
        Some(fs::read(&path).expect("transcript is readable")),
        "the stream matches the grown transcript"
    );
}

#[test]
fn rewritten_start_begins_a_new_generation() {
    let setup = Setup::new();
    let mut store = setup.store();
    import(&setup, &mut store);
    let locator = main_transcript();
    let path = setup.path(&locator);
    let mut rewritten = b"{\"type\":\"summary\",\"summary\":\"compacted\"}\n".to_vec();
    rewritten.extend(fs::read(&path).expect("transcript is readable"));
    fs::write(&path, &rewritten).expect("transcript is rewritten");

    let planned = plan_for(&setup, &store, &locator);
    assert_eq!(
        planned.first(),
        Some(&Write::NewGeneration {
            key: key(&locator),
            generation: 1
        }),
        "the plan starts generation 1"
    );
    assert_eq!(
        planned.get(1),
        Some(&segment(&key(&locator), 1, 0, &rewritten)),
        "then sends the whole file from offset 0"
    );

    let summary = import(&setup, &mut store);
    assert_eq!(
        summary.generations_started, 1,
        "the import starts one generation"
    );
    let state = &store.state(ORIGIN).expect("state reads")[&key(&locator)];
    assert_eq!(state.generation, 1, "the stream is on generation 1");
    assert_eq!(
        store.contents(&key(&locator)).expect("contents read"),
        Some(rewritten),
        "the new generation holds the rewritten file"
    );
}

#[test]
fn shrunk_transcript_begins_a_new_generation() {
    let setup = Setup::new();
    let mut store = setup.store();
    import(&setup, &mut store);
    let locator = main_transcript();
    let path = setup.path(&locator);
    let original = fs::read(&path).expect("transcript is readable");
    let first_line = &original[..=original
        .iter()
        .position(|b| *b == b'\n')
        .expect("transcript has a line")];
    fs::write(&path, first_line).expect("transcript is shortened");

    let summary = import(&setup, &mut store);

    assert_eq!(
        summary.generations_started, 1,
        "a shorter file starts a new generation"
    );
    assert_eq!(
        store.contents(&key(&locator)).expect("contents read"),
        Some(first_line.to_vec()),
        "the new generation holds the shortened file"
    );
}

#[test]
fn changed_replace_file_stores_one_version_and_unchanged_stores_none() {
    let setup = Setup::new();
    let mut store = setup.store();
    import(&setup, &mut store);
    let locator = custom_title();
    let renamed = b"{\"customTitle\": \"Add JSON and CSV output flags\"}\n";
    fs::write(setup.path(&locator), renamed).expect("title file is rewritten");

    let changed = import(&setup, &mut store);
    assert_eq!(
        changed.writes_planned, 1,
        "only the changed title file is sent"
    );
    assert_eq!(changed.versions_stored, 1, "one new version is stored");
    assert_eq!(
        store.contents(&key(&locator)).expect("contents read"),
        Some(renamed.to_vec()),
        "the latest version is the new title"
    );
    let versions = setup.count("versions");

    let unchanged = import(&setup, &mut store);
    assert_eq!(
        unchanged.versions_stored, 0,
        "an unchanged replace file stores no version"
    );
    assert_eq!(
        setup.count("versions"),
        versions,
        "the version count is unchanged"
    );
}

#[test]
fn reapplied_writes_change_nothing_and_bad_segments_are_rejected() {
    let temp = tempfile::tempdir().expect("temp directory is created");
    let mut store = Store::open(temp.path()).expect("store opens");
    let log = key("project/session.jsonl");
    let first = segment(&log, 0, 0, b"{\"type\":\"user\"}\n");

    assert_eq!(
        store.apply(&first).expect("apply runs"),
        Applied::Appended {
            confirmed_offset: 16
        }
    );
    let state = store.state(ORIGIN).expect("state reads");
    assert_eq!(
        store.apply(&first).expect("apply runs"),
        Applied::AlreadyHad,
        "re-applying a segment reports it as already had"
    );
    assert_eq!(
        store.state(ORIGIN).expect("state reads"),
        state,
        "re-applying a segment changes no state"
    );

    assert_eq!(
        store
            .apply(&segment(&log, 0, 20, b"late\n"))
            .expect("apply runs"),
        Applied::Rejected(Rejection::Gap {
            confirmed_offset: 16
        }),
        "a segment past the confirmed offset is rejected as a gap"
    );
    assert_eq!(
        store
            .apply(&Write::Segment {
                key: log.clone(),
                generation: 0,
                start: 16,
                bytes: b"next\n".to_vec(),
                sha256: Sha256::of(b"other\n"),
            })
            .expect("apply runs"),
        Applied::Rejected(Rejection::ChecksumMismatch),
        "a segment whose bytes do not match its checksum is rejected"
    );
    assert_eq!(
        store.state(ORIGIN).expect("state reads"),
        state,
        "rejected segments change no state"
    );

    let next = Write::NewGeneration {
        key: log.clone(),
        generation: 1,
    };
    assert_eq!(
        store.apply(&next).expect("apply runs"),
        Applied::NewGenerationStarted { generation: 1 }
    );
    assert_eq!(
        store.apply(&next).expect("apply runs"),
        Applied::AlreadyHad,
        "re-applying a new generation is a no-op"
    );

    let title = key("project/session/custom-title.json");
    let version = Write::Version {
        key: title.clone(),
        sha256: Sha256::of(b"{}"),
        bytes: b"{}".to_vec(),
    };
    assert_eq!(
        store.apply(&version).expect("apply runs"),
        Applied::VersionStored
    );
    assert_eq!(
        store.apply(&version).expect("apply runs"),
        Applied::VersionUnchanged,
        "re-applying a version is a no-op"
    );
}

#[test]
fn bytes_past_the_committed_offset_are_trimmed() {
    let temp = tempfile::tempdir().expect("temp directory is created");
    let log = key("project/session.jsonl");
    let committed = b"{\"type\":\"user\"}\n";
    let mut store = Store::open(temp.path()).expect("store opens");
    store
        .apply(&segment(&log, 0, 0, committed))
        .expect("apply runs");
    drop(store);

    let generation_files: Vec<PathBuf> = fs::read_dir(temp.path().join("streams"))
        .expect("streams directory exists")
        .map(|entry| {
            entry
                .expect("directory entry is readable")
                .path()
                .join("0.log")
        })
        .collect();
    assert_eq!(
        generation_files.len(),
        1,
        "one stream directory with a generation 0 file"
    );
    append_junk(&generation_files[0]);

    let mut store = Store::open(temp.path()).expect("store reopens");
    assert_eq!(
        fs::metadata(&generation_files[0])
            .expect("generation file exists")
            .len(),
        committed.len() as u64,
        "the generation file is trimmed to the committed offset"
    );
    append_junk(&generation_files[0]);
    let next = b"{\"type\":\"assistant\"}\n";
    assert_eq!(
        store
            .apply(&segment(&log, 0, committed.len() as u64, next))
            .expect("apply runs"),
        Applied::Appended {
            confirmed_offset: (committed.len() + next.len()) as u64
        },
        "the resent segment appends at the committed offset"
    );
    assert_eq!(
        store.contents(&log).expect("contents read"),
        Some([committed.as_slice(), next].concat()),
        "the stream holds the committed and resent bytes, without junk from before or after the reopen"
    );
}

fn append_junk(path: &Path) {
    OpenOptions::new()
        .append(true)
        .open(path)
        .expect("generation file opens")
        .write_all(
            b"{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":\"cut off",
        )
        .expect(
            "junk is appended past the committed offset, as a crash or failed write would leave it",
        );
}
