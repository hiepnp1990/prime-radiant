//! What a collector is doing: one `Snapshot`, kept in a state file for `prime-radiant status` and shown on stderr.
//!
//! The collector's thread only changes the snapshot and queues what to print. An output thread does every write: it
//! rewrites the state file at most once a second, and at once on every phase change, by writing a temporary file
//! beside it and renaming that over it, and it prints to stderr. A terminal that is paused or a state folder that
//! hangs therefore stops only the output, never the sync. For as long as the collector runs, the output thread also
//! holds an exclusive lock on `<state file>.lock`, so a reader can tell a running collector from a stopped one even
//! after the system gives the stopped one's pid to another process.

use std::collections::VecDeque;
use std::fs::{self, DirBuilder, File, OpenOptions, TryLockError};
use std::io::{self, IsTerminal, Write as _};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::index::Timestamp;

/// How often the state file may be rewritten, and the line on a terminal redrawn.
const TICK: Duration = Duration::from_secs(1);
/// A collector with nothing new still rewrites its state file this often, so a stopped one was last seen recently.
const HEARTBEAT: Duration = Duration::from_secs(10);
/// The span the sending rate is measured over.
const RATE_WINDOW: Duration = Duration::from_secs(30);
const RATE_MIN_SPAN: Duration = Duration::from_secs(3);
/// How often a log, where stderr is not a terminal, gets a progress line between phase changes.
const LOG_FIRST_SYNC: Duration = Duration::from_secs(30);
const LOG_LIVE: Duration = Duration::from_secs(5 * 60);
/// The longest line drawn on a terminal, so that it never wraps and `\r` can redraw it.
const LINE_CHARS: usize = 79;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub origin: String,
    pub pid: u32,
    pub started_at: Timestamp,
    pub updated_at: Timestamp,
    pub phase: Phase,
    /// Bytes the hub confirmed from this process since it started.
    pub sent_bytes: u64,
    pub last_sent: Option<Sent>,
    /// Warnings this process printed since it started.
    pub errors: u64,
    pub last_error: Option<Note>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "name", rename_all = "snake_case")]
pub enum Phase {
    /// Reaching the hub for the first time.
    Connecting,
    /// Comparing each local file with the hub's confirmed state, to learn what the hub lacks.
    Scanning {
        connected_since: Timestamp,
        files_checked: u64,
    },
    /// Sending what the scan found the hub lacks. Every connection starts with one.
    FirstSync {
        connected_since: Timestamp,
        progress: SyncProgress,
    },
    /// Sending each change as it happens.
    Live {
        connected_since: Timestamp,
        /// The files the last full scan found. A T3 Code database counts as one.
        files: u64,
        /// The connection's first sync, finished.
        first_sync: SyncProgress,
    },
    /// Waiting to reach the hub again.
    Reconnecting {
        down_since: Timestamp,
        retry_at: Timestamp,
        reason: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncProgress {
    pub started_at: Timestamp,
    /// The files the hub lacked bytes of, and how many of them are done.
    pub files_total: u64,
    pub files_done: u64,
    /// The bytes the hub lacked, and how many it has confirmed since. A file that grows while it is sent adds its new
    /// bytes, and one that vanishes or is skipped takes away the bytes it did not send, so the two meet at the end.
    pub bytes_total: u64,
    pub bytes_done: u64,
    /// The pace of the last half minute, or `None` until there is enough to measure.
    pub rate: Option<Rate>,
    /// The file being sent now.
    pub current: Option<Current>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rate {
    pub bytes_per_second: u64,
    pub seconds_left: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Current {
    pub locator: String,
    pub bytes_total: u64,
    pub bytes_done: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sent {
    pub at: Timestamp,
    pub locator: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Note {
    pub at: Timestamp,
    pub message: String,
}

impl Snapshot {
    /// What the collector is doing as of `now`, such as `first sync 42% · 8.1 of 19.3 GiB · 17 MiB/s · about 11 min
    /// left`.
    pub fn summary(&self, now: Timestamp) -> String {
        match &self.phase {
            Phase::Connecting => "connecting to the hub".to_owned(),
            Phase::Scanning {
                files_checked: 0, ..
            } => "scanning · comparing local files with the hub".to_owned(),
            Phase::Scanning { files_checked, .. } => format!(
                "scanning · {} files compared with the hub",
                count(*files_checked)
            ),
            Phase::FirstSync { progress, .. } => {
                let mut line = format!(
                    "first sync {}% · {}",
                    progress.percent(),
                    size_of(progress.bytes_done, progress.bytes_total)
                );
                if progress.bytes_done < progress.bytes_total {
                    line += &match progress.rate {
                        Some(rate) => format!(
                            " · {}/s · about {} left",
                            size(rate.bytes_per_second),
                            duration(rate.seconds_left as i64 * 1000)
                        ),
                        None => " · measuring speed".to_owned(),
                    };
                }
                line
            }
            Phase::Live { files, .. } => format!(
                "live · watching {} files · {}",
                count(*files),
                match &self.last_sent {
                    Some(sent) => format!("last sent {}", ago(sent.at, now)),
                    None => "nothing sent yet".to_owned(),
                }
            ),
            Phase::Reconnecting {
                retry_at, reason, ..
            } => match retry_at.0 - now.0 {
                wait if wait >= 1000 => format!("reconnecting in {} ({reason})", duration(wait)),
                _ => format!("reconnecting now ({reason})"),
            },
        }
    }

    /// The summary after the origin, as stderr shows it.
    fn line(&self, now: Timestamp) -> String {
        format!("{} · {}", self.origin, self.summary(now))
    }

    fn connected_since(&self) -> Option<Timestamp> {
        match self.phase {
            Phase::Scanning {
                connected_since, ..
            }
            | Phase::FirstSync {
                connected_since, ..
            }
            | Phase::Live {
                connected_since, ..
            } => Some(connected_since),
            Phase::Connecting | Phase::Reconnecting { .. } => None,
        }
    }
}

impl SyncProgress {
    /// Rounded down, so that it reads 100 only once everything is done. Files count when no bytes are lacking, as
    /// when the hub lacks only empty files.
    pub fn percent(&self) -> u64 {
        match (self.bytes_total, self.files_total) {
            (0, 0) => 100,
            (0, files) => self.files_done * 100 / files,
            (bytes, _) => self.bytes_done * 100 / bytes,
        }
    }
}

/// `$XDG_STATE_HOME/prime-radiant`, or `~/.local/state/prime-radiant`, or `None` without either variable.
pub fn default_dir() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_STATE_HOME").map(PathBuf::from) {
        Some(dir) if dir.is_absolute() => dir,
        _ => Path::new(&std::env::var_os("HOME")?).join(".local/state"),
    };
    Some(base.join("prime-radiant"))
}

/// The name of the state file of a collector for `origin`, with any byte that could not be in a file name escaped.
pub fn file_name(origin: &str) -> String {
    let mut name = "collect-".to_owned();
    for byte in origin.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.".contains(&byte) {
            name.push(byte as char);
        } else {
            name += &format!("%{byte:02X}");
        }
    }
    name + ".json"
}

fn beside(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    name.into()
}

/// A state file as `status` finds it.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum Found {
    Collector {
        file: PathBuf,
        running: bool,
        state: Box<Snapshot>,
    },
    Unreadable {
        file: PathBuf,
        error: String,
    },
}

/// Every state file in `dir`, by name. A folder that does not exist holds none.
pub fn read_dir(dir: &Path) -> io::Result<Vec<Found>> {
    let entries = match fs::read_dir(dir) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        entries => entries?,
    };
    let mut files = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path
            .extension()
            .is_some_and(|extension| extension == "json")
        {
            files.push(path);
        }
    }
    files.sort();
    Ok(files.into_iter().map(read).collect())
}

fn read(file: PathBuf) -> Found {
    let state = fs::read(&file).and_then(|bytes| Ok(serde_json::from_slice(&bytes)?));
    match state {
        Ok(state) => Found::Collector {
            running: locked(&beside(&file, ".lock")),
            file,
            state,
        },
        Err(error) => Found::Unreadable {
            file,
            error: error.to_string(),
        },
    }
}

/// Whether a process holds the lock, taking a shared lock for a moment to find out.
fn locked(lock: &Path) -> bool {
    File::open(lock)
        .is_ok_and(|file| matches!(file.try_lock_shared(), Err(TryLockError::WouldBlock)))
}

/// The collector's side of its progress. Every method changes the snapshot at once and returns without writing
/// anything, so it never waits on a terminal or a disk.
pub struct Reporter {
    state: Arc<Mutex<State>>,
    output: Sender<Output>,
}

/// What the collector's thread changes and the output thread reads.
struct State {
    snapshot: Snapshot,
    /// Whether the snapshot changed since the state file was written.
    dirty: bool,
    /// The first sync's confirmed bytes over the rate window, oldest first.
    samples: VecDeque<(Instant, u64)>,
}

/// What the output thread prints, in the order the collector's thread queued it.
enum Output {
    /// A line of its own, such as a warning.
    Line(String),
    /// The phase changed. A terminal keeps the old phase's last line, then shows the note, if any.
    Phase {
        kept: Option<String>,
        note: Option<String>,
        /// The new phase's line for a log, or `None` when the note already says it.
        entered: Option<String>,
    },
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Reporter {
    /// Starts reporting as a collector for `origin` that is connecting to the hub. Without a state file, only stderr
    /// shows what it does.
    pub fn start(origin: &str, state_file: Option<PathBuf>) -> Self {
        let now = Timestamp::now();
        let snapshot = Snapshot {
            origin: origin.to_owned(),
            pid: std::process::id(),
            started_at: now,
            updated_at: now,
            phase: Phase::Connecting,
            sent_bytes: 0,
            last_sent: None,
            errors: 0,
            last_error: None,
        };
        let entered = Some(snapshot.line(now));
        let state = Arc::new(Mutex::new(State {
            snapshot,
            dirty: true,
            samples: VecDeque::new(),
        }));
        let (output, queued) = mpsc::channel();
        let printer = Printer {
            state: Arc::clone(&state),
            file: None,
            screen: match io::stderr().is_terminal() {
                true => Screen::Terminal { drawn: false },
                false => Screen::Log,
            },
            written: None,
            next_log: None,
        };
        thread::spawn(move || printer.run(state_file, queued));
        let _ = output.send(Output::Phase {
            kept: None,
            note: None,
            entered,
        });
        Self { state, output }
    }

    fn change(&self, change: impl FnOnce(&mut Snapshot)) {
        let mut state = lock(&self.state);
        change(&mut state.snapshot);
        state.dirty = true;
    }

    /// Prints a warning and counts it as an error.
    pub fn warn(&self, message: String) {
        let line = format!("collect: {message}");
        count_error(&mut lock(&self.state), message);
        let _ = self.output.send(Output::Line(line));
    }

    /// Moves to the phase `next` makes of the current snapshot, if it makes one.
    fn enter(&self, next: impl FnOnce(&Snapshot) -> Option<Phase>, note: Option<String>) {
        let mut state = lock(&self.state);
        let Some(phase) = next(&state.snapshot) else {
            return;
        };
        let now = Timestamp::now();
        let kept = Some(state.snapshot.line(now));
        state.snapshot.phase = phase;
        state.dirty = true;
        state.samples.clear();
        let entered = match state.snapshot.phase {
            Phase::Reconnecting { .. } if note.is_some() => None,
            _ => Some(state.snapshot.line(now)),
        };
        drop(state);
        let _ = self.output.send(Output::Phase {
            kept,
            note,
            entered,
        });
    }

    /// The hub answered and sent its state, which `message` describes, so the scan starts.
    pub fn connected(&self, message: &str) {
        let scanning = Phase::Scanning {
            connected_since: Timestamp::now(),
            files_checked: 0,
        };
        self.enter(|_| Some(scanning), Some(format!("collect: {message}")));
    }

    /// The scan compared one more file with the hub's state.
    pub fn compared(&self) {
        self.change(|snapshot| {
            if let Phase::Scanning { files_checked, .. } = &mut snapshot.phase {
                *files_checked += 1;
            }
        });
    }

    /// The scan found `files` files of which the hub lacks `bytes` in all.
    pub fn first_sync(&self, files: u64, bytes: u64) {
        self.enter(
            |snapshot| {
                Some(Phase::FirstSync {
                    connected_since: snapshot.connected_since().unwrap_or_else(Timestamp::now),
                    progress: SyncProgress {
                        started_at: Timestamp::now(),
                        files_total: files,
                        files_done: 0,
                        bytes_total: bytes,
                        bytes_done: 0,
                        rate: None,
                        current: None,
                    },
                })
            },
            None,
        );
    }

    /// The first sync starts on a file the scan found `bytes` missing from.
    pub fn sending(&self, locator: &str, bytes: u64) {
        self.change(|snapshot| {
            if let Phase::FirstSync { progress, .. } = &mut snapshot.phase {
                progress.current = Some(Current {
                    locator: locator.to_owned(),
                    bytes_total: bytes,
                    bytes_done: 0,
                });
            }
        });
    }

    /// The first sync is done with the file it was sending.
    pub fn sent_file(&self) {
        self.change(|snapshot| {
            if let Phase::FirstSync { progress, .. } = &mut snapshot.phase
                && let Some(current) = progress.current.take()
            {
                progress.bytes_total = progress
                    .bytes_total
                    .saturating_sub(current.bytes_total.saturating_sub(current.bytes_done));
                progress.files_done += 1;
            }
        });
    }

    /// The hub confirmed it holds `bytes` more of the stream at `locator`.
    pub fn confirmed(&self, locator: &str, bytes: u64) {
        self.change(|snapshot| {
            snapshot.sent_bytes += bytes;
            snapshot.last_sent = Some(Sent {
                at: Timestamp::now(),
                locator: locator.to_owned(),
            });
            let Phase::FirstSync { progress, .. } = &mut snapshot.phase else {
                return;
            };
            progress.bytes_done += bytes;
            match &mut progress.current {
                Some(current) => {
                    current.bytes_done += bytes;
                    // The file grew since the scan.
                    if current.bytes_done > current.bytes_total {
                        progress.bytes_total += current.bytes_done - current.bytes_total;
                        current.bytes_total = current.bytes_done;
                    }
                }
                // A file that the scan found in step changed before the sync reached it.
                None => progress.bytes_total += bytes,
            }
        });
    }

    /// The first sync is done, and changes are sent as they happen.
    pub fn live(&self, files: u64) {
        self.enter(
            |snapshot| match &snapshot.phase {
                Phase::FirstSync {
                    connected_since,
                    progress,
                } => Some(Phase::Live {
                    connected_since: *connected_since,
                    files,
                    first_sync: SyncProgress {
                        rate: None,
                        current: None,
                        ..progress.clone()
                    },
                }),
                _ => None,
            },
            None,
        );
    }

    /// A full scan while live found `files` files.
    pub fn watching(&self, count: u64) {
        self.change(|snapshot| {
            if let Phase::Live { files, .. } = &mut snapshot.phase {
                *files = count;
            }
        });
    }

    /// The connection failed with `error`, and the next try is in `wait`.
    pub fn reconnecting(&self, error: &io::Error, wait: Duration) {
        let message = format!("{error}; reconnecting in {}s", wait.as_secs());
        let now = Timestamp::now();
        let retry_at = Timestamp(now.0 + wait.as_millis() as i64);
        let mut state = lock(&self.state);
        count_error(&mut state, message.clone());
        if let Phase::Reconnecting {
            retry_at: next,
            reason: why,
            ..
        } = &mut state.snapshot.phase
        {
            // Another failed try: the warning says so, and the phase goes on.
            *next = retry_at;
            *why = reason(error);
            drop(state);
            let _ = self
                .output
                .send(Output::Line(format!("collect: {message}")));
            return;
        }
        drop(state);
        let reconnecting = Phase::Reconnecting {
            down_since: now,
            retry_at,
            reason: reason(error),
        };
        self.enter(|_| Some(reconnecting), Some(format!("collect: {message}")));
    }
}

fn count_error(state: &mut State, message: String) {
    state.snapshot.errors += 1;
    state.snapshot.last_error = Some(Note {
        at: Timestamp::now(),
        message,
    });
    state.dirty = true;
}

/// Why the connection failed, in a few words where the error is a common one.
fn reason(error: &io::Error) -> String {
    use io::ErrorKind::*;
    match error.kind() {
        NotFound | ConnectionRefused | AddrNotAvailable => "hub unreachable".to_owned(),
        UnexpectedEof | BrokenPipe | ConnectionReset | ConnectionAborted => {
            "connection lost".to_owned()
        }
        TimedOut => "the hub stopped answering".to_owned(),
        _ => error.to_string(),
    }
}

/// The output thread: the only writer of the state file and of the collector's stderr after it starts.
struct Printer {
    state: Arc<Mutex<State>>,
    file: Option<StateFile>,
    screen: Screen,
    written: Option<Instant>,
    /// When a log gets its next progress line.
    next_log: Option<Instant>,
}

enum Screen {
    /// A terminal, with whether the progress line is on it now.
    Terminal {
        drawn: bool,
    },
    Log,
}

struct StateFile {
    path: PathBuf,
    temp: PathBuf,
    /// Held until the process ends.
    _lock: File,
    /// Whether the last write failed, so that a lasting failure is reported once.
    failing: bool,
}

impl StateFile {
    fn open(path: PathBuf) -> io::Result<Self> {
        if let Some(dir) = path.parent() {
            DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        }
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(beside(&path, ".lock"))?;
        // `status` holds a shared lock for a moment, so a lock that is taken may be free a moment later.
        for _ in 0..20 {
            match lock.try_lock() {
                Ok(()) => {
                    return Ok(Self {
                        temp: beside(&path, ".tmp"),
                        path,
                        _lock: lock,
                        failing: false,
                    });
                }
                Err(TryLockError::WouldBlock) => thread::sleep(Duration::from_millis(50)),
                Err(TryLockError::Error(error)) => return Err(error),
            }
        }
        Err(io::Error::other("another running collector keeps it"))
    }

    fn write(&self, snapshot: &Snapshot) -> io::Result<()> {
        fs::write(&self.temp, serde_json::to_vec_pretty(snapshot)?)?;
        fs::rename(&self.temp, &self.path)
    }
}

impl Printer {
    fn run(mut self, state_file: Option<PathBuf>, queued: Receiver<Output>) {
        match state_file {
            Some(path) => match StateFile::open(path.clone()) {
                Ok(file) => self.file = Some(file),
                Err(error) => self.warn(format!(
                    "cannot keep progress in {}, so status will not show this collector: {error}",
                    path.display()
                )),
            },
            None => self.warn(
                "neither XDG_STATE_HOME nor HOME is set, so status will not show this collector; give --state-file"
                    .to_owned(),
            ),
        }
        let mut next_tick = Instant::now() + TICK;
        loop {
            match queued.recv_timeout(next_tick.saturating_duration_since(Instant::now())) {
                Ok(Output::Line(line)) => self.print(&line),
                Ok(Output::Phase {
                    kept,
                    note,
                    entered,
                }) => self.entered(kept, note, entered),
                Err(RecvTimeoutError::Timeout) => {
                    self.tick();
                    next_tick = Instant::now() + TICK;
                }
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    fn warn(&mut self, message: String) {
        let line = format!("collect: {message}");
        count_error(&mut lock(&self.state), message);
        self.print(&line);
    }

    /// Prints a line of its own, above the progress line on a terminal.
    fn print(&mut self, line: &str) {
        match self.screen {
            Screen::Terminal { drawn: true } => {
                let _ = writeln!(io::stderr(), "\r\x1b[2K{line}");
                self.draw();
            }
            Screen::Terminal { drawn: false } | Screen::Log => {
                let _ = writeln!(io::stderr(), "{line}");
            }
        }
    }

    fn draw(&mut self) {
        if let Screen::Terminal { drawn } = &mut self.screen {
            *drawn = true;
            let line = lock(&self.state).snapshot.line(Timestamp::now());
            let _ = write!(io::stderr(), "\r\x1b[2K{}", fit(&line));
        }
    }

    fn entered(&mut self, kept: Option<String>, note: Option<String>, entered: Option<String>) {
        self.save();
        self.next_log = match lock(&self.state).snapshot.phase {
            Phase::FirstSync { .. } => Some(Instant::now() + LOG_FIRST_SYNC),
            Phase::Live { .. } => Some(Instant::now() + LOG_LIVE),
            _ => None,
        };
        match &mut self.screen {
            Screen::Terminal { drawn } => {
                if *drawn && let Some(kept) = kept {
                    let _ = writeln!(io::stderr(), "\r\x1b[2K{}", fit(&kept));
                    *drawn = false;
                }
                if let Some(note) = note {
                    let _ = writeln!(io::stderr(), "{note}");
                }
                self.draw();
            }
            Screen::Log => {
                for line in note
                    .into_iter()
                    .chain(entered.map(|line| format!("collect: {line}")))
                {
                    let _ = writeln!(io::stderr(), "{line}");
                }
            }
        }
    }

    fn tick(&mut self) {
        let now = Instant::now();
        let dirty = measure(&mut lock(&self.state), now);
        let due = self.written.is_none_or(|at| {
            let since = now.duration_since(at);
            (dirty && since >= TICK) || since >= HEARTBEAT
        });
        if due {
            self.save();
        }
        match self.screen {
            Screen::Terminal { .. } => self.draw(),
            Screen::Log => {
                if let Some(next) = self.next_log
                    && now >= next
                {
                    let (line, every) = {
                        let state = lock(&self.state);
                        let every = match state.snapshot.phase {
                            Phase::FirstSync { .. } => LOG_FIRST_SYNC,
                            _ => LOG_LIVE,
                        };
                        (state.snapshot.line(Timestamp::now()), every)
                    };
                    let _ = writeln!(io::stderr(), "collect: {line}");
                    self.next_log = Some(now + every);
                }
            }
        }
    }

    fn save(&mut self) {
        let Some(file) = &mut self.file else {
            return;
        };
        let snapshot = {
            let mut state = lock(&self.state);
            state.snapshot.updated_at = Timestamp::now();
            state.dirty = false;
            state.snapshot.clone()
        };
        let written = file.write(&snapshot);
        let was_failing = std::mem::replace(&mut file.failing, written.is_err());
        let path = file.path.clone();
        self.written = Some(Instant::now());
        if let Err(error) = written
            && !was_failing
        {
            self.warn(format!(
                "cannot write progress to {}: {error}",
                path.display()
            ));
        }
    }
}

/// Updates the first sync's rate from the bytes it confirmed over the rate window, and returns whether the snapshot
/// changed since the state file was written.
fn measure(state: &mut State, now: Instant) -> bool {
    let State {
        snapshot,
        dirty,
        samples,
    } = state;
    if let Phase::FirstSync { progress, .. } = &mut snapshot.phase {
        samples.push_back((now, progress.bytes_done));
        while samples
            .front()
            .is_some_and(|(at, _)| now.duration_since(*at) > RATE_WINDOW)
        {
            samples.pop_front();
        }
        let (since, before) = samples[0];
        let span = now.duration_since(since);
        let rate = (span >= RATE_MIN_SPAN && progress.bytes_done > before).then(|| {
            let per_second = (progress.bytes_done - before) as f64 / span.as_secs_f64();
            Rate {
                bytes_per_second: per_second as u64,
                seconds_left: (progress.bytes_total.saturating_sub(progress.bytes_done) as f64
                    / per_second)
                    .ceil() as u64,
            }
        });
        if progress.rate != rate {
            progress.rate = rate;
            *dirty = true;
        }
    }
    *dirty
}

/// The line cut to fit a terminal.
fn fit(line: &str) -> String {
    match line.char_indices().nth(LINE_CHARS - 1) {
        Some((end, _)) => format!("{}…", &line[..end]),
        None => line.to_owned(),
    }
}

/// `n` with a comma between each group of three digits, such as `3,704`.
pub fn count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];

/// The largest unit `bytes` is at least one of, and how many of that unit it is.
fn unit(bytes: u64) -> (usize, f64) {
    let mut i = 0;
    while i + 1 < UNITS.len() && bytes >= 1 << (10 * (i + 1)) {
        i += 1;
    }
    (i, bytes as f64 / (1u64 << (10 * i)) as f64)
}

/// Such as `512 B`, `8.1 MiB`, or `17 MiB`.
pub fn size(bytes: u64) -> String {
    let (i, value) = unit(bytes);
    let amount = match i {
        0 => value.to_string(),
        _ if value < 10.0 => format!("{value:.1}"),
        _ => format!("{value:.0}"),
    };
    format!("{amount} {}", UNITS[i])
}

/// Such as `8.1 of 19.3 GiB`, both in the unit of `total`.
pub fn size_of(done: u64, total: u64) -> String {
    let (i, _) = unit(total.max(done));
    let of = |bytes: u64| match i {
        0 => bytes.to_string(),
        _ => format!("{:.1}", bytes as f64 / (1u64 << (10 * i)) as f64),
    };
    format!("{} of {} {}", of(done), of(total), UNITS[i])
}

/// A span of milliseconds in its largest whole unit, such as `40 s`, `11 min`, `2 h 5 min`, or `3 days`.
pub fn duration(ms: i64) -> String {
    let seconds = ms.max(0) / 1000;
    match seconds {
        s if s < 60 => format!("{s} s"),
        s if s < 3600 => format!("{} min", s / 60),
        s if s < 86_400 => match s / 60 % 60 {
            0 => format!("{} h", s / 3600),
            minutes => format!("{} h {minutes} min", s / 3600),
        },
        s if s < 2 * 86_400 => "1 day".to_owned(),
        s => format!("{} days", s / 86_400),
    }
}

/// Such as `4 s ago`.
pub fn ago(at: Timestamp, now: Timestamp) -> String {
    format!("{} ago", duration(now.0 - at.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_read_the_way_the_progress_line_shows_them() {
        assert_eq!(count(3704), "3,704");
        assert_eq!(count(999), "999");
        assert_eq!(count(1_000_000), "1,000,000");
        assert_eq!(size(512), "512 B");
        assert_eq!(size(17 << 20), "17 MiB");
        assert_eq!(size(8_697_308_774), "8.1 GiB");
        assert_eq!(size_of(8_697_308_774, 20_723_217_203), "8.1 of 19.3 GiB");
        assert_eq!(size_of(0, 300), "0 of 300 B");
        assert_eq!(duration(4_000), "4 s");
        assert_eq!(duration(11 * 60_000 + 20_000), "11 min");
        assert_eq!(duration(2 * 3_600_000 + 5 * 60_000), "2 h 5 min");
        assert_eq!(duration(3 * 86_400_000), "3 days");
    }

    #[test]
    fn a_state_file_name_keeps_every_origin_apart() {
        assert_eq!(file_name("laptop"), "collect-laptop.json");
        assert_ne!(file_name("a/b"), file_name("a_b"));
        assert!(!file_name("../up").contains('/'));
    }
}
