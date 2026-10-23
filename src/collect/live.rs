//! The collector process: one connection to the hub, an initial sync, then file watching and a periodic sweep.
//!
//! The collector keeps no offsets of its own. The hub's confirmed state is the truth: it arrives on every connect
//! and with the answer to every write, and the next write is planned from it. After any failure the collector
//! reconnects and starts over from that state. The one thing it remembers is the size and modification time of
//! each file it left in sync with the hub, so that a sweep reads only what changed.

use std::collections::HashMap;
use std::fs;
use std::io::{self, BufReader, BufWriter};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use super::progress::Reporter;
use super::{Layout, LocalFile, lacking, layout, next_write, plan_version, t3_code};
use crate::client::{HubAddress, Transport};
use crate::protocol::{self, Request, Response, Role};
use crate::raw::{Applied, Source, StreamKey, StreamState, Write, WriteMode};
use crate::{Error, Result};

/// A changed file is sent once it has had no new writes for this long.
pub const QUIET: Duration = Duration::from_secs(2);
/// A file that never goes quiet, such as a running session's transcript, is still sent this long after it changed.
pub const MAX_WAIT: Duration = Duration::from_secs(4);
/// How often to check a T3 Code database for changes.
const DATABASE_POLL: Duration = Duration::from_secs(1);
/// How long the hub may go without reading a byte of a request or sending its response.
const STALL_TIMEOUT: Duration = Duration::from_secs(60);
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);
const WRITE_CHUNK: usize = 64 * 1024;

pub struct Options {
    pub origin: String,
    /// Each source with its root folder, or for T3 Code its database. The collector only reads them.
    pub sources: Vec<(Source, PathBuf)>,
    pub hub: HubAddress,
    /// How often to rescan every source, to catch changes the watcher missed.
    pub sweep: Duration,
    /// Where to keep what the collector is doing, for `prime-radiant status`.
    pub state_file: Option<PathBuf>,
}

/// Runs until the process is killed. Returns only if a source or the file watcher is unusable at the start.
pub fn run(options: Options) -> Result<()> {
    let sources = options
        .sources
        .iter()
        .map(|(source, path)| Input::resolve(*source, path))
        .collect::<io::Result<Vec<_>>>()?;
    let (inbound, events) = mpsc::channel();
    let files = inbound.clone();
    let watcher = notify::recommended_watcher(move |event| {
        let _ = files.send(Inbound::Files(event));
    })
    .map_err(|error| Error::Io(io::Error::other(format!("cannot start watching: {error}"))))?;
    let mut watch = Watch {
        watcher,
        paths: sources
            .iter()
            .flat_map(Input::watches)
            .map(|(path, mode)| (path, mode, false))
            .collect(),
    };
    let report = Reporter::start(&options.origin, options.state_file.clone());
    watch.ensure(&report);
    let mut synced = HashMap::new();

    let mut backoff = BACKOFF_MIN;
    let mut link = 0;
    loop {
        link += 1;
        // Changes queued before the connection are covered by the initial sync, which checks every file afterwards.
        while events.try_recv().is_ok() {}
        let started = match Link::open(&options.hub, link, &inbound) {
            Ok(link) => Session::start(
                &options,
                &sources,
                &mut watch,
                &mut synced,
                link,
                &events,
                &report,
            ),
            Err(error) => Err(error),
        };
        let error = match started {
            Ok(mut session) => {
                let error = session.run();
                if session.swept {
                    backoff = BACKOFF_MIN;
                }
                error
            }
            Err(error) => error,
        };
        report.reconnecting(&error, backoff);
        thread::sleep(backoff);
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

/// A source as the collector reads it, with its paths resolved the way the watcher reports them.
enum Input {
    /// Files under a root, each its own stream, which the watcher covers.
    Files {
        layout: &'static Layout,
        root: PathBuf,
    },
    /// A T3 Code database, whose threads are each a replace stream. T3 Code keeps the database and its write-ahead
    /// log open, and FSEvents reports a write only once the writer closes the file, so the collector polls their
    /// sizes and modification times instead of watching them.
    Database { path: PathBuf, wal: PathBuf },
}

impl Input {
    fn resolve(source: Source, path: &Path) -> io::Result<Self> {
        // Watchers report resolved paths, such as /private/var on macOS, so locators come from the resolved path.
        let path = path.canonicalize().map_err(|error| {
            io::Error::new(error.kind(), format!("{}: {error}", path.display()))
        })?;
        Ok(match layout(source) {
            Some(layout) => Input::Files { layout, root: path },
            None => {
                let mut wal = path.clone().into_os_string();
                wal.push("-wal");
                Input::Database {
                    path,
                    wal: wal.into(),
                }
            }
        })
    }

    fn source(&self) -> Source {
        match self {
            Input::Files { layout, .. } => layout.source,
            Input::Database { .. } => Source::T3Code,
        }
    }

    /// What the watcher covers: each folder that holds the files, and the root, so that a folder created later and a
    /// file kept in the root are seen. Codex and Grok keep much else under their roots.
    fn watches(&self) -> Vec<(PathBuf, RecursiveMode)> {
        match self {
            Input::Files { layout, root } => {
                let folders = layout.folders(root);
                let root_too =
                    (!folders.contains(root)).then(|| (root.clone(), RecursiveMode::NonRecursive));
                folders
                    .into_iter()
                    .map(|folder| (folder, RecursiveMode::Recursive))
                    .chain(root_too)
                    .collect()
            }
            Input::Database { .. } => Vec::new(),
        }
    }
}

/// The file watcher. A path can be missing, or fail to watch, for example at the inotify watch limit, so each sweep
/// retries it.
struct Watch {
    watcher: RecommendedWatcher,
    /// Each path to watch, how, and whether it is watched yet.
    paths: Vec<(PathBuf, RecursiveMode, bool)>,
}

impl Watch {
    fn ensure(&mut self, report: &Reporter) {
        let Self { watcher, paths } = self;
        for (path, mode, watching) in paths {
            // A folder that a source has not created yet, such as Codex's archive, is watched once it exists.
            if *watching || !path.exists() {
                continue;
            }
            match watcher.watch(path, *mode) {
                Ok(()) => *watching = true,
                Err(error) => report.warn(format!(
                    "cannot watch {}, so its changes wait for the next sweep: {error}",
                    path.display()
                )),
            }
        }
    }
}

/// A file's size and modification time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Stamp {
    len: u64,
    modified: SystemTime,
}

/// The stamp of the file at `path`, or `None` when there is no such file.
fn stamp(path: &Path) -> io::Result<Option<Stamp>> {
    match fs::metadata(path) {
        Ok(meta) => Ok(Some(Stamp {
            len: meta.len(),
            modified: meta.modified()?,
        })),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// The stamps of a database and of its write-ahead log, which SQLite deletes and recreates.
fn database_stamps(path: &Path, wal: &Path) -> io::Result<Vec<Option<Stamp>>> {
    Ok(vec![stamp(path)?, stamp(wal)?])
}

/// What a sync left in step with the hub.
struct Synced {
    /// The stamps of what the sync read, taken before it read them: the file, or a database and its write-ahead log.
    stamps: Vec<Option<Stamp>>,
    /// The hub's state of each stream the sync left matching what it read.
    streams: Vec<(StreamKey, StreamState)>,
}

/// Why a file is synced, which decides whether an unchanged size and modification time are enough to skip it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Cause {
    /// A scan listed it, in a sweep or in a new folder. A change the watcher missed still moves the stamp.
    Scan,
    /// The watcher, or a poll of a database, reported a change, so it is read whatever its stamp.
    Event,
}

/// Everything the collector waits for arrives on one channel.
enum Inbound {
    Files(notify::Result<notify::Event>),
    /// From the reader or writer thread of connection `link`.
    Hub {
        link: u64,
        event: HubEvent,
    },
}

enum HubEvent {
    Response(Response),
    /// Part of a request reached the hub's side of the connection.
    Wrote,
    Closed(io::Error),
}

/// One connection to the hub. Dropping it closes the connection, which stops its threads.
struct Link {
    id: u64,
    requests: Sender<protocol::Request>,
    socket: Option<UnixStream>,
    child: Option<Child>,
}

impl Link {
    fn open(address: &HubAddress, id: u64, inbound: &Sender<Inbound>) -> io::Result<Self> {
        let Transport {
            from_hub,
            to_hub,
            socket,
            child,
        } = address.open()?;

        let responses = inbound.clone();
        thread::spawn(move || {
            let mut from_hub = BufReader::new(from_hub);
            let closed = loop {
                match protocol::read_response(&mut from_hub) {
                    Ok(Some(response)) => {
                        let event = HubEvent::Response(response);
                        if responses.send(Inbound::Hub { link: id, event }).is_err() {
                            return;
                        }
                    }
                    Ok(None) => {
                        break io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "the hub closed the connection",
                        );
                    }
                    Err(error) => break error,
                }
            };
            let event = HubEvent::Closed(closed);
            let _ = responses.send(Inbound::Hub { link: id, event });
        });

        let (requests, queue) = mpsc::channel::<Request>();
        let progress = inbound.clone();
        thread::spawn(move || {
            let mut to_hub = BufWriter::new(Reporting {
                inner: to_hub,
                link: id,
                inbound: progress.clone(),
            });
            for request in queue {
                if let Err(error) = protocol::write_request(&mut to_hub, &request) {
                    let event = HubEvent::Closed(error);
                    let _ = progress.send(Inbound::Hub { link: id, event });
                    return;
                }
            }
        });

        Ok(Self {
            id,
            requests,
            socket,
            child,
        })
    }

    fn send(&self, request: Request) -> io::Result<()> {
        self.requests
            .send(request)
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "the connection is closed"))
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        if let Some(socket) = &self.socket {
            let _ = socket.shutdown(Shutdown::Both);
        }
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Writes in chunks and reports each one, so that a slow link that still moves bytes does not count as a stall.
struct Reporting<W> {
    inner: W,
    link: u64,
    inbound: Sender<Inbound>,
}

impl<W: io::Write> io::Write for Reporting<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(&buf[..buf.len().min(WRITE_CHUNK)])?;
        let event = HubEvent::Wrote;
        let _ = self.inbound.send(Inbound::Hub {
            link: self.link,
            event,
        });
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// A change not yet sent.
struct Unsent<'a> {
    target: Target<'a>,
    first: Instant,
    last: Instant,
}

enum Target<'a> {
    File {
        source: Source,
        file: LocalFile,
    },
    /// A new folder. Its first files can land before the watcher covers it, so it is scanned once it is quiet.
    Folder {
        layout: &'static Layout,
        root: &'a Path,
    },
    /// A T3 Code database whose stamps changed.
    Database {
        path: &'a Path,
        wal: &'a Path,
    },
}

impl Unsent<'_> {
    fn due(&self) -> Instant {
        (self.last + QUIET).min(self.first + MAX_WAIT)
    }
}

/// A file or database the first sync covers.
enum Item<'a> {
    File { source: Source, file: LocalFile },
    Database { path: &'a Path, wal: &'a Path },
}

/// An item, and how many bytes of it the hub lacked when the scan compared them, or `None` when the hub had them all
/// or the scan could not tell.
struct Compared<'a> {
    item: Item<'a>,
    lacking: Option<u64>,
}

impl Compared<'_> {
    fn name(&self) -> String {
        match &self.item {
            Item::File { file, .. } => file.locator.clone(),
            Item::Database { path, .. } => path.display().to_string(),
        }
    }
}

struct Session<'a> {
    origin: &'a str,
    sources: &'a [Input],
    report: &'a Reporter,
    sweep_every: Duration,
    watch: &'a mut Watch,
    /// What this process last left in step with the hub, by the path it read. It outlives the connection.
    synced: &'a mut HashMap<PathBuf, Synced>,
    events: &'a Receiver<Inbound>,
    link: Link,
    /// The hub's confirmed state of every stream of this origin from the sources collected.
    states: HashMap<StreamKey, StreamState>,
    unsent: HashMap<PathBuf, Unsent<'a>>,
    next_sweep: Instant,
    /// When to poll the databases next, or `None` without a database to poll.
    next_poll: Option<Instant>,
    /// The stamps each database had when it was last looked at.
    polled: HashMap<PathBuf, Vec<Option<Stamp>>>,
    /// Whether a sweep finished on this connection, which proves the connection works for more than a hello.
    swept: bool,
}

impl<'a> Session<'a> {
    /// Says hello on a new connection, and loads the hub's state.
    fn start(
        options: &'a Options,
        sources: &'a [Input],
        watch: &'a mut Watch,
        synced: &'a mut HashMap<PathBuf, Synced>,
        link: Link,
        events: &'a Receiver<Inbound>,
        report: &'a Reporter,
    ) -> io::Result<Self> {
        let mut session = Self {
            origin: &options.origin,
            sources,
            report,
            sweep_every: options.sweep,
            watch,
            synced,
            events,
            link,
            states: HashMap::new(),
            unsent: HashMap::new(),
            next_sweep: Instant::now(),
            next_poll: sources
                .iter()
                .any(|input| matches!(input, Input::Database { .. }))
                .then(Instant::now),
            polled: HashMap::new(),
            swept: false,
        };
        let hello = Request::Hello {
            protocol: protocol::VERSION,
            origin: options.origin.clone(),
            role: Role::Collector,
        };
        match session.call(hello)? {
            Response::Hello {
                protocol: protocol::VERSION,
            } => {}
            other => return Err(unexpected(&other)),
        }
        let mut loaded = Vec::new();
        for source in sources.iter().map(Input::source) {
            if loaded.contains(&source) {
                continue;
            }
            loaded.push(source);
            let state = Request::State {
                origin: options.origin.clone(),
                source,
            };
            match session.call(state)? {
                Response::State { streams } => session.states.extend(streams),
                other => return Err(unexpected(&other)),
            }
        }
        report.connected(&format!(
            "connected; the hub has {} streams from {}",
            session.states.len(),
            session.origin
        ));
        Ok(session)
    }

    /// Syncs every source, then sends changes as they happen. Returns the error that ended the connection.
    fn run(&mut self) -> io::Error {
        // Set first, so that a rescan requested during the first sync can still move it earlier.
        self.next_sweep = Instant::now() + self.sweep_every;
        if let Err(error) = self.first_sync() {
            return error;
        }
        self.swept = true;
        loop {
            if let Err(error) = self.step() {
                return error;
            }
        }
    }

    /// Compares every file with the hub's confirmed state, then sends what the hub lacks. The totals it reports come
    /// from that comparison, so that they are what this connection has to send.
    fn first_sync(&mut self) -> io::Result<()> {
        self.watch.ensure(self.report);
        let compared = self.compare();
        let lacking = compared.iter().filter_map(|compared| compared.lacking);
        self.report
            .first_sync(lacking.clone().count() as u64, lacking.sum());
        for compared in &compared {
            if let Some(bytes) = compared.lacking {
                self.report.sending(&compared.name(), bytes);
            }
            match &compared.item {
                Item::File { source, file } => self.sync_file(*source, file, Cause::Scan)?,
                Item::Database { path, wal } => self.sync_database(path, wal, Cause::Scan)?,
            }
            if compared.lacking.is_some() {
                self.report.sent_file();
            }
        }
        self.report.live(compared.len() as u64);
        Ok(())
    }

    /// Every file and database of every source, with what the hub lacks of each. One the hub has in full is
    /// remembered as synced, as `sync_file` and `sync_database` would remember it, so the sync after skips it unread.
    fn compare(&mut self) -> Vec<Compared<'a>> {
        let mut compared = Vec::new();
        let sources = self.sources;
        for input in sources {
            match input {
                Input::Files { layout, root } => {
                    let files = match layout.scan(root) {
                        Ok(files) => files,
                        Err(error) => {
                            self.report
                                .warn(format!("cannot scan {}: {error}", root.display()));
                            continue;
                        }
                    };
                    for file in files {
                        let lacking = self.file_lacking(layout.source, &file);
                        self.report.compared();
                        compared.push(Compared {
                            item: Item::File {
                                source: layout.source,
                                file,
                            },
                            lacking,
                        });
                    }
                }
                Input::Database { path, wal } => {
                    let lacking = self.database_lacking(path, wal);
                    self.report.compared();
                    compared.push(Compared {
                        item: Item::Database { path, wal },
                        lacking,
                    });
                }
            }
        }
        compared
    }

    /// The bytes of the file that the hub lacks. A file that cannot be read counts as none, and its sync reports why.
    fn file_lacking(&mut self, source: Source, file: &LocalFile) -> Option<u64> {
        let Ok(Some(stamp)) = stamp(&file.path) else {
            return None;
        };
        let stamps = vec![Some(stamp)];
        if self.unchanged(&file.path, &stamps)
            || (file.write_mode == WriteMode::Replace
                && stamp.len > u64::from(protocol::MAX_PAYLOAD_BYTES))
        {
            return None;
        }
        let key = StreamKey {
            origin: self.origin.to_owned(),
            source,
            locator: file.locator.clone(),
        };
        let lacking = lacking(file, self.states.get(&key)).ok()?;
        if lacking.is_none() {
            self.remember(&file.path, stamps, vec![key]);
        }
        lacking
    }

    /// The bytes of the database's threads that the hub lacks, read as `sync_database` reads them.
    fn database_lacking(&mut self, path: &Path, wal: &Path) -> Option<u64> {
        let stamps = database_stamps(path, wal).ok()?;
        if self.unchanged(path, &stamps) {
            return None;
        }
        let threads = t3_code::snapshots(path).ok()?;
        let (mut lacking, mut in_step) = (None, true);
        let mut keys = Vec::with_capacity(threads.len());
        for thread in threads {
            let key = StreamKey {
                origin: self.origin.to_owned(),
                source: Source::T3Code,
                locator: thread.locator,
            };
            let len = thread.bytes.len() as u64;
            if len > u64::from(protocol::MAX_PAYLOAD_BYTES) {
                in_step = false;
            } else if plan_version(key.clone(), thread.bytes, self.states.get(&key)).is_some() {
                *lacking.get_or_insert(0) += len;
            }
            keys.push(key);
        }
        if lacking.is_none() && in_step {
            self.remember(path, stamps, keys);
        }
        lacking
    }

    fn step(&mut self) -> io::Result<()> {
        let now = Instant::now();
        if now >= self.next_sweep {
            // Set first, so that a rescan requested during the sweep can still move it earlier.
            self.next_sweep = now + self.sweep_every;
            self.sweep()?;
            self.swept = true;
        }
        if self.next_poll.is_some_and(|poll| now >= poll) {
            self.next_poll = Some(now + DATABASE_POLL);
            self.poll();
        }
        let due: Vec<PathBuf> = self
            .unsent
            .iter()
            .filter(|(_, unsent)| unsent.due() <= now)
            .map(|(path, _)| path.clone())
            .collect();
        for path in due {
            match self.unsent.remove(&path).map(|unsent| unsent.target) {
                Some(Target::File { source, file }) => {
                    self.sync_file(source, &file, Cause::Event)?
                }
                Some(Target::Folder { layout, root }) => {
                    // The folder may be one that holds the source's files, such as Codex's archive, so watch it too.
                    self.watch.ensure(self.report);
                    self.sync_scan(layout.source, &path, layout.scan_under(root, &path))?;
                }
                Some(Target::Database { path, wal }) => {
                    self.sync_database(path, wal, Cause::Event)?;
                }
                None => {}
            }
        }
        let wake = self
            .unsent
            .values()
            .map(Unsent::due)
            .chain(self.next_poll)
            .fold(self.next_sweep, Instant::min);
        match self
            .events
            .recv_timeout(wake.saturating_duration_since(Instant::now()))
        {
            Ok(Inbound::Files(event)) => self.note(event),
            Ok(Inbound::Hub { link, event }) if link == self.link.id => match event {
                HubEvent::Wrote => {}
                HubEvent::Closed(error) => return Err(error),
                HubEvent::Response(response) => return Err(unexpected(&response)),
            },
            Ok(Inbound::Hub { .. }) | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => unreachable!("run holds a sender"),
        }
        Ok(())
    }

    fn sweep(&mut self) -> io::Result<()> {
        self.watch.ensure(self.report);
        let sources = self.sources;
        let mut files = 0;
        for input in sources {
            match input {
                Input::Files { layout, root } => {
                    files += self.sync_scan(layout.source, root, layout.scan(root))?;
                }
                Input::Database { path, wal } => {
                    files += 1;
                    self.sync_database(path, wal, Cause::Scan)?;
                }
            }
        }
        self.report.watching(files);
        Ok(())
    }

    /// Syncs the files a scan of `dir` found, and returns how many it found.
    fn sync_scan(
        &mut self,
        source: Source,
        dir: &Path,
        scan: io::Result<Vec<LocalFile>>,
    ) -> io::Result<u64> {
        let files = match scan {
            Ok(files) => files,
            Err(error) => {
                self.report
                    .warn(format!("cannot scan {}: {error}", dir.display()));
                return Ok(0);
            }
        };
        for file in &files {
            self.sync_file(source, file, Cause::Scan)?;
        }
        Ok(files.len() as u64)
    }

    /// Sends writes until the hub's copy of the file matches it. A problem with one file skips that file; a
    /// connection error ends the session.
    fn sync_file(&mut self, source: Source, file: &LocalFile, cause: Cause) -> io::Result<()> {
        let stamp = match stamp(&file.path) {
            Ok(Some(stamp)) => stamp,
            // Claude Code deletes old transcripts, so a file can vanish at any time.
            Ok(None) => {
                self.synced.remove(&file.path);
                return Ok(());
            }
            Err(error) => {
                self.report
                    .warn(format!("skipping {}: {error}", file.locator));
                return Ok(());
            }
        };
        let stamps = vec![Some(stamp)];
        if cause == Cause::Scan && self.unchanged(&file.path, &stamps) {
            return Ok(());
        }
        if file.write_mode == WriteMode::Replace
            && stamp.len > u64::from(protocol::MAX_PAYLOAD_BYTES)
        {
            self.report.warn(format!(
                "skipping {}: it is over the {}-byte limit",
                file.locator,
                protocol::MAX_PAYLOAD_BYTES
            ));
            return Ok(());
        }
        let key = StreamKey {
            origin: self.origin.to_owned(),
            source,
            locator: file.locator.clone(),
        };
        loop {
            let write = match next_write(&key, file, self.states.get(&key)) {
                Ok(Some(write)) => write,
                Ok(None) => break,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    self.synced.remove(&file.path);
                    return Ok(());
                }
                Err(error) => {
                    self.report
                        .warn(format!("skipping {}: {error}", file.locator));
                    return Ok(());
                }
            };
            let before = self.states.get(&key).cloned();
            match self.send(write)? {
                Some(Applied::VersionStored | Applied::VersionUnchanged) => break,
                // Each of these moves the hub's state forward; stop if it did not, rather than resend forever.
                Some(
                    Applied::Appended { .. }
                    | Applied::NewGenerationStarted { .. }
                    | Applied::AlreadyHad,
                ) if before.as_ref() != self.states.get(&key) => {}
                _ => return Ok(()),
            }
        }
        self.remember(&file.path, stamps, vec![key]);
        Ok(())
    }

    /// Notes each database whose stamps changed since it was last looked at, as the watcher notes a file.
    fn poll(&mut self) {
        let now = Instant::now();
        let sources = self.sources;
        for input in sources {
            let Input::Database { path, wal } = input else {
                continue;
            };
            let Ok(stamps) = database_stamps(path, wal) else {
                continue;
            };
            if self.polled.get(path) == Some(&stamps) {
                continue;
            }
            self.polled.insert(path.clone(), stamps);
            self.mark(path.clone(), Target::Database { path, wal }, now);
        }
    }

    /// Notes a change to send once it is due.
    fn mark(&mut self, key: PathBuf, target: Target<'a>, now: Instant) {
        self.unsent
            .entry(key)
            .and_modify(|unsent| unsent.last = now)
            .or_insert(Unsent {
                target,
                first: now,
                last: now,
            });
    }

    /// Reads every thread of a T3 Code database in one snapshot and sends each thread whose snapshot changed.
    fn sync_database(&mut self, path: &Path, wal: &Path, cause: Cause) -> io::Result<()> {
        let stamps = match database_stamps(path, wal) {
            Ok(stamps) => stamps,
            Err(error) => {
                self.report
                    .warn(format!("cannot read {}: {error}", path.display()));
                return Ok(());
            }
        };
        self.polled.insert(path.to_owned(), stamps.clone());
        if cause == Cause::Scan && self.unchanged(path, &stamps) {
            return Ok(());
        }
        let threads = match t3_code::snapshots(path) {
            Ok(threads) => threads,
            Err(error) => {
                self.report
                    .warn(format!("cannot read {}: {error}", path.display()));
                return Ok(());
            }
        };
        let mut keys = Vec::with_capacity(threads.len());
        let mut in_step = true;
        for thread in threads {
            let key = StreamKey {
                origin: self.origin.to_owned(),
                source: Source::T3Code,
                locator: thread.locator,
            };
            if thread.bytes.len() as u64 > u64::from(protocol::MAX_PAYLOAD_BYTES) {
                self.report.warn(format!(
                    "skipping {}: it is over the {}-byte limit",
                    key.locator,
                    protocol::MAX_PAYLOAD_BYTES
                ));
                in_step = false;
            } else if let Some(write) =
                plan_version(key.clone(), thread.bytes, self.states.get(&key))
            {
                in_step &= matches!(
                    self.send(write)?,
                    Some(Applied::VersionStored | Applied::VersionUnchanged)
                );
            }
            keys.push(key);
        }
        if in_step {
            self.remember(path, stamps, keys);
        }
        Ok(())
    }

    /// Sends one write and takes the stream's state from the answer. Returns the outcome, or `None` if the hub
    /// could not store the write.
    fn send(&mut self, write: Write) -> io::Result<Option<Applied>> {
        let key = write.key().clone();
        let bytes = match &write {
            Write::Segment { bytes, .. } | Write::Version { bytes, .. } => bytes.len() as u64,
            Write::NewGeneration { .. } => 0,
        };
        let (outcome, state) = match self.call(Request::Write(write))? {
            Response::Applied { outcome, state } => (outcome, state),
            // The hub could not store this write, for example because its disk is full. Other files may still fit.
            Response::Error { message } => {
                self.report.warn(format!(
                    "the hub could not store {}: {message}",
                    key.locator
                ));
                return Ok(None);
            }
            other => return Err(unexpected(&other)),
        };
        match outcome {
            Applied::Rejected(rejection) => self.report.warn(format!(
                "the hub rejected a write to {}: {rejection:?}",
                key.locator
            )),
            _ => self.report.confirmed(&key.locator, bytes),
        }
        match state {
            Some(state) => self.states.insert(key, state),
            None => self.states.remove(&key),
        };
        Ok(Some(outcome))
    }

    /// Whether what was read from `path` still has `stamps`, and the hub still holds what this process left there.
    fn unchanged(&self, path: &Path, stamps: &[Option<Stamp>]) -> bool {
        self.synced.get(path).is_some_and(|synced| {
            synced.stamps == stamps
                && synced
                    .streams
                    .iter()
                    .all(|(key, state)| self.states.get(key) == Some(state))
        })
    }

    fn remember(&mut self, path: &Path, stamps: Vec<Option<Stamp>>, keys: Vec<StreamKey>) {
        let streams = keys
            .into_iter()
            .filter_map(|key| {
                let state = self.states.get(&key)?.clone();
                Some((key, state))
            })
            .collect();
        self.synced
            .insert(path.to_owned(), Synced { stamps, streams });
    }

    /// Sends one request and waits for its response, noting file changes that arrive meanwhile.
    fn call(&mut self, request: Request) -> io::Result<Response> {
        self.link.send(request)?;
        let mut deadline = Instant::now() + STALL_TIMEOUT;
        loop {
            match self
                .events
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                Ok(Inbound::Files(event)) => self.note(event),
                Ok(Inbound::Hub { link, event }) if link == self.link.id => match event {
                    HubEvent::Wrote => deadline = Instant::now() + STALL_TIMEOUT,
                    HubEvent::Response(response) => return Ok(response),
                    HubEvent::Closed(error) => return Err(error),
                },
                Ok(Inbound::Hub { .. }) => {}
                Err(RecvTimeoutError::Timeout) => {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("the hub made no progress for {}s", STALL_TIMEOUT.as_secs()),
                    ));
                }
                Err(RecvTimeoutError::Disconnected) => unreachable!("run holds a sender"),
            }
        }
    }

    fn note(&mut self, event: notify::Result<notify::Event>) {
        let now = Instant::now();
        let event = match event {
            Ok(event) => event,
            Err(error) => {
                self.report.warn(format!(
                    "the watcher failed, so every source is swept soon: {error}"
                ));
                self.next_sweep = self.next_sweep.min(now + QUIET);
                return;
            }
        };
        if event.need_rescan() {
            self.next_sweep = self.next_sweep.min(now + QUIET);
        }
        // inotify reports every open, the collector's own reads included, and a read changes nothing.
        if matches!(event.kind, EventKind::Access(_)) {
            return;
        }
        let sources = self.sources;
        for path in event.paths {
            for input in sources {
                let Input::Files { layout, root } = input else {
                    continue;
                };
                let target = match layout.classify(root, &path) {
                    Some(file) => Target::File {
                        source: layout.source,
                        file,
                    },
                    None if matches!(event.kind, EventKind::Create(_))
                        && path.is_dir()
                        && layout.holds(root, &path) =>
                    {
                        Target::Folder { layout, root }
                    }
                    None => continue,
                };
                self.mark(path.clone(), target, now);
            }
        }
    }
}

fn unexpected(response: &Response) -> io::Error {
    match response {
        Response::Error { message } => io::Error::other(format!("the hub refused: {message}")),
        other => io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unexpected response from the hub: {other:?}"),
        ),
    }
}
