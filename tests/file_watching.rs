use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use notify::{Event, EventKind, RecursiveMode, Watcher};

const EVENT_TIMEOUT: Duration = Duration::from_secs(5);

fn wait_for(events: &Receiver<notify::Result<Event>>, matches: impl Fn(&Event) -> bool) -> bool {
    let deadline = Instant::now() + EVENT_TIMEOUT;
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match events.recv_timeout(left) {
            Ok(Ok(event)) if matches(&event) => return true,
            Ok(Ok(_)) => {}
            Ok(Err(error)) => panic!("watcher reported an error: {error}"),
            Err(_) => return false,
        }
    }
    false
}

fn touches(event: &Event, path: &Path) -> bool {
    event.paths.iter().any(|p| p == path)
}

#[test]
fn recursive_watcher_reports_nested_creates_and_appends() {
    let temp = tempfile::tempdir().expect("temp directory is created");
    // macOS temp paths resolve through /private, and FSEvents reports the resolved path.
    let root = temp.path().canonicalize().expect("temp path canonicalizes");
    let project = root.join("projects").join("-home-dev-example-project");
    fs::create_dir_all(&project).expect("nested folder is created");

    let (tx, events) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(tx).expect("watcher starts");
    #[cfg(target_os = "macos")]
    let _: &notify::FsEventWatcher = &watcher;
    #[cfg(target_os = "linux")]
    let _: &notify::INotifyWatcher = &watcher;
    watcher
        .watch(&root, RecursiveMode::Recursive)
        .expect("root is watched recursively");

    let transcript = project.join("session.jsonl");
    fs::write(&transcript, "{\"type\":\"user\"}\n").expect("transcript is created");
    assert!(
        wait_for(&events, |event| touches(event, &transcript)),
        "an event for the new file in a nested folder arrives within 5 seconds"
    );

    while events.try_recv().is_ok() {}

    let mut file = OpenOptions::new()
        .append(true)
        .open(&transcript)
        .expect("transcript opens for append");
    file.write_all(b"{\"type\":\"assistant\"}\n")
        .expect("line is appended");
    drop(file);
    assert!(
        wait_for(&events, |event| {
            touches(event, &transcript) && matches!(event.kind, EventKind::Modify(_))
        }),
        "a modify event for the appended file arrives within 5 seconds"
    );
}
