//! The hub: the process that owns a data directory and serves it on `<dir>/hub.sock`.
//!
//! Each database has one writer. The intake thread owns the `Store` and is the only writer of `raw.db`. The index
//! thread owns the `Indexer` and is the only writer of `index.db`; it also answers queries, because a query may
//! rebuild a brief, which the indexer then stores. Each connection gets a thread that parses requests and hands them
//! to the owner.

use std::collections::HashMap;
use std::fs::{self, DirBuilder, Permissions};
use std::io::{self, BufReader, BufWriter, Read, Write as _};
use std::net::Shutdown;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use crate::index::{Indexer, Timestamp};
use crate::protocol::{self, OriginStats, Request, Response, Role};
use crate::query::{Answer, Ask, Query};
use crate::raw::{Applied, Source, Store, StreamKey, StreamState, View, Write};
use crate::{Error, Result};

/// How long the index waits after a write for more writes before it syncs.
const SYNC_DELAY: Duration = Duration::from_millis(200);
/// The longest the index waits during a steady stream of writes.
const MAX_SYNC_DELAY: Duration = Duration::from_secs(1);
const SYNC_RETRY_DELAY: Duration = Duration::from_secs(30);

pub fn socket_path(dir: &Path) -> PathBuf {
    dir.join("hub.sock")
}

/// Serves `dir` until the process is killed. Safe to kill at any point: the store trims uncommitted bytes when it
/// opens, and the index catches up from the raw layer.
pub fn serve(dir: &Path) -> Result<()> {
    DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    // The store's lock makes this the only writer of `dir`, so any socket file is left over from a hub that died.
    let store = Store::open(dir)?;
    let indexer = Indexer::open(dir)?;
    let path = socket_path(dir);
    if let Err(error) = fs::remove_file(&path)
        && error.kind() != io::ErrorKind::NotFound
    {
        return Err(error.into());
    }
    let listener = UnixListener::bind(&path).map_err(|error| at(&path, error))?;
    fs::set_permissions(&path, Permissions::from_mode(0o600))?;

    let (index, index_commands) = mpsc::channel();
    let query_dir = dir.to_owned();
    thread::Builder::new()
        .name("index".to_owned())
        .spawn(move || run_index(indexer, &query_dir, index_commands))?;
    let (intake, intake_commands) = mpsc::channel();
    let owners = Owners {
        intake,
        index: index.clone(),
        dir: dir.to_owned(),
    };
    thread::Builder::new()
        .name("accept".to_owned())
        .spawn(move || accept(listener, owners))?;
    eprintln!("hub: serving {}", path.display());
    run_intake(store, intake_commands, index)
}

/// Copies stdin to the hub's socket and the socket to stdout, so a collector on another machine can reach the hub
/// through `ssh <hub-host> .local/bin/prime-radiant hub connect`. Returns when the hub closes the connection.
pub fn connect(dir: &Path) -> io::Result<()> {
    let path = socket_path(dir);
    let mut socket = UnixStream::connect(&path).map_err(|error| at(&path, error))?;
    let mut to_hub = socket.try_clone()?;
    thread::spawn(move || {
        let _ = io::copy(&mut io::stdin().lock(), &mut to_hub);
        // The hub sees the end of the requests, answers what it has, and closes, which ends the loop below.
        let _ = to_hub.shutdown(Shutdown::Write);
    });
    let mut stdout = io::stdout().lock();
    let mut buffer = vec![0; 64 * 1024];
    loop {
        let n = match socket.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(n) => n,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        stdout.write_all(&buffer[..n])?;
        stdout.flush()?;
    }
}

fn at(socket: &Path, error: io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("{}: {error}", socket.display()))
}

enum Intake {
    State {
        origin: String,
        source: Source,
        reply: Sender<Result<Vec<(StreamKey, StreamState)>>>,
    },
    Write {
        write: Write,
        reply: Sender<Result<(Applied, Option<StreamState>)>>,
    },
    /// When the hub last stored a write from each origin since it started.
    LastWrites {
        reply: Sender<HashMap<String, Timestamp>>,
    },
}

enum Index {
    /// New bytes or a new version reached the raw layer.
    Changed,
    Query {
        ask: Ask,
        reply: Sender<Result<Response>>,
    },
}

#[derive(Clone)]
struct Owners {
    intake: Sender<Intake>,
    index: Sender<Index>,
    /// The data directory, which a connection reads through its own `View`.
    dir: PathBuf,
}

/// Runs one step of an owner thread, turning a panic into an error, so that a bug that one stream or one request
/// triggers fails that request instead of stopping the hub. A panic inside a transaction rolls it back, and bytes
/// written past the committed offset are overwritten by the next segment, so the owner can go on.
fn guard<T>(what: &str, step: impl FnOnce() -> Result<T>) -> Result<T> {
    panic::catch_unwind(AssertUnwindSafe(step))
        .unwrap_or_else(|_| Err(Error::Internal(format!("{what} panicked"))))
}

/// Applies writes one at a time. Returns only if the index thread stopped, which ends the hub.
fn run_intake(mut store: Store, commands: Receiver<Intake>, index: Sender<Index>) -> Result<()> {
    let mut last_writes = HashMap::new();
    for command in commands {
        match command {
            Intake::State {
                origin,
                source,
                reply,
            } => {
                let streams = guard("reading stream state", || {
                    let mut streams: Vec<_> = store
                        .state(&origin)?
                        .into_iter()
                        .filter(|(key, _)| key.source == source)
                        .collect();
                    streams.sort_by(|a, b| a.0.locator.cmp(&b.0.locator));
                    Ok(streams)
                });
                let _ = reply.send(streams);
            }
            Intake::Write { write, reply } => {
                let result = guard("applying a write", || {
                    let applied = store.apply(&write)?;
                    Ok((applied, store.stream_state(write.key())?))
                });
                let changed = matches!(
                    result,
                    Ok((
                        Applied::Appended { .. }
                            | Applied::VersionStored
                            | Applied::NewGenerationStarted { .. },
                        _
                    ))
                );
                if changed {
                    last_writes.insert(write.key().origin.clone(), Timestamp::now());
                }
                // Before the reply, so that a query sent after the confirmation finds the index due for a sync.
                if changed && index.send(Index::Changed).is_err() {
                    return Err(Error::Internal("the index thread stopped".to_owned()));
                }
                let _ = reply.send(result);
            }
            Intake::LastWrites { reply } => {
                let _ = reply.send(last_writes.clone());
            }
        }
    }
    Ok(())
}

/// When the index syncs next: shortly after a burst of changes ends, at most `MAX_SYNC_DELAY` into a long burst,
/// and `SYNC_RETRY_DELAY` after a failed sync.
struct SyncTimer {
    due: Option<Instant>,
    burst_start: Option<Instant>,
}

impl SyncTimer {
    fn changed(&mut self, now: Instant) {
        let start = *self.burst_start.get_or_insert(now);
        self.due = Some((now + SYNC_DELAY).min(start + MAX_SYNC_DELAY));
    }

    fn synced(&mut self, ok: bool, now: Instant) {
        self.burst_start = None;
        self.due = (!ok).then_some(now + SYNC_RETRY_DELAY);
    }
}

/// Syncs the index once per burst of changes, and before every query so that answers include every confirmed write.
fn run_index(mut indexer: Indexer, dir: &Path, commands: Receiver<Index>) {
    // The raw layer may hold writes that a hub confirmed and then died before indexing.
    let mut timer = SyncTimer {
        due: Some(Instant::now()),
        burst_start: None,
    };
    loop {
        let command = match timer.due {
            None => match commands.recv() {
                Ok(command) => Some(command),
                Err(_) => return,
            },
            Some(due) => match commands.recv_timeout(due.saturating_duration_since(Instant::now()))
            {
                Ok(command) => Some(command),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return,
            },
        };
        match command {
            Some(Index::Changed) => timer.changed(Instant::now()),
            Some(Index::Query { ask, reply }) => {
                if timer.due.is_some() {
                    sync(&mut indexer, &mut timer);
                }
                let answered = guard("answering a query", || answer(dir, &mut indexer, ask));
                let _ = reply.send(answered);
            }
            None => sync(&mut indexer, &mut timer),
        }
    }
}

fn sync(indexer: &mut Indexer, timer: &mut SyncTimer) {
    let result = guard("indexing", || indexer.sync());
    if let Err(error) = &result {
        eprintln!(
            "hub: indexing failed, retrying within {}s: {error}",
            SYNC_RETRY_DELAY.as_secs()
        );
    }
    timer.synced(result.is_ok(), Instant::now());
}

/// Answers from a fresh read-only view of the index, and stores a brief the answer rebuilt, so that the index thread's
/// indexer stays the only writer of `index.db`.
fn answer(dir: &Path, indexer: &mut Indexer, ask: Ask) -> Result<Response> {
    let answer = Query::open(dir, Timestamp::now())?.answer(ask)?;
    if let Answer::Brief(brief) = &answer
        && brief.freshness.rebuilt
    {
        indexer.store_brief(brief)?;
    }
    Ok(answer.into())
}

fn accept(listener: UnixListener, owners: Owners) {
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let owners = owners.clone();
                let spawned = thread::Builder::new()
                    .name("connection".to_owned())
                    .spawn(move || serve_connection(stream, &owners));
                if let Err(error) = spawned {
                    eprintln!("hub: cannot start a connection thread: {error}");
                }
            }
            Err(error) => {
                eprintln!("hub: accept failed: {error}");
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

/// Who is on the other end of a connection, from its hello.
struct Peer {
    origin: String,
    role: Role,
}

/// A response, and whether the connection closes after it.
struct Reply {
    response: Response,
    close: bool,
}

impl Reply {
    fn open(response: Response) -> Self {
        Self {
            response,
            close: false,
        }
    }

    fn close(message: impl Into<String>) -> Self {
        Self {
            response: Response::Error {
                message: message.into(),
            },
            close: true,
        }
    }
}

fn serve_connection(stream: UnixStream, owners: &Owners) {
    let Ok(read_half) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(read_half);
    let mut writer = BufWriter::new(stream);
    let mut peer = None;
    loop {
        let reply = match protocol::read_request(&mut reader) {
            Ok(Some(request)) => respond(&mut peer, request, owners),
            Ok(None) => return,
            Err(error) => Reply::close(format!("bad frame: {error}")),
        };
        if protocol::write_response(&mut writer, &reply.response).is_err() || reply.close {
            return;
        }
    }
}

fn respond(peer: &mut Option<Peer>, request: Request, owners: &Owners) -> Reply {
    let Some(known) = peer.as_ref() else {
        return match request {
            Request::Hello {
                protocol: protocol::VERSION,
                origin,
                role,
            } => {
                *peer = Some(Peer { origin, role });
                Reply::open(Response::Hello {
                    protocol: protocol::VERSION,
                })
            }
            Request::Hello {
                protocol: version, ..
            } => Reply::close(format!(
                "this hub speaks protocol {}, not {version}",
                protocol::VERSION
            )),
            _ => Reply::close("the first request must be hello"),
        };
    };
    match request {
        Request::Hello { .. } => Reply::close("hello was already sent"),
        Request::State { origin, source } => ask(&owners.intake, |reply| Intake::State {
            origin,
            source,
            reply,
        })
        .map_or_else(stopped, |result| {
            reply(result.map(|streams| Response::State { streams }))
        }),
        Request::Write(_) if known.role != Role::Collector => {
            Reply::close("only a collector can write")
        }
        Request::Write(write) if write.key().origin != known.origin => Reply::close(format!(
            "a collector for origin {} cannot write to origin {}",
            known.origin,
            write.key().origin
        )),
        Request::Write(write) => ask(&owners.intake, |reply| Intake::Write { write, reply })
            .map_or_else(stopped, |result| {
                reply(result.map(|(outcome, state)| Response::Applied { outcome, state }))
            }),
        Request::Ask(question) => ask(&owners.index, |reply| Index::Query {
            ask: question,
            reply,
        })
        .map_or_else(stopped, reply),
        Request::Origins => origins(owners),
    }
}

/// Reads the totals through this connection's own view, so that the intake thread only hands over the times.
fn origins(owners: &Owners) -> Reply {
    let Some(last_writes) = ask(&owners.intake, |reply| Intake::LastWrites { reply }) else {
        return stopped();
    };
    let totals = View::open(&owners.dir).and_then(|view| view.origins());
    reply(totals.map(|totals| {
        Response::Origins {
            origins: totals
                .into_iter()
                .map(|totals| OriginStats {
                    last_write: last_writes.get(&totals.origin).copied(),
                    origin: totals.origin,
                    streams: totals.streams,
                    confirmed_bytes: totals.confirmed_bytes,
                })
                .collect(),
        }
    }))
}

/// Sends a command to an owner thread and waits for its answer, or `None` if the owner stopped.
fn ask<C, T>(owner: &Sender<C>, command: impl FnOnce(Sender<T>) -> C) -> Option<T> {
    let (reply, answer) = mpsc::channel();
    owner.send(command(reply)).ok()?;
    answer.recv().ok()
}

fn reply(result: Result<Response>) -> Reply {
    Reply::open(result.unwrap_or_else(|error| Response::Error {
        message: error.to_string(),
    }))
}

fn stopped() -> Reply {
    Reply::close("the hub is shutting down")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_panic_in_an_owner_step_becomes_an_error() {
        let failed = guard("a made-up step", || -> Result<()> {
            panic!("a made-up bug")
        });
        assert!(
            matches!(failed, Err(Error::Internal(_))),
            "the panic is an internal error, not {failed:?}"
        );
        assert!(
            matches!(guard("a made-up step", || Ok(7)), Ok(7)),
            "a step that returns passes its result through"
        );
    }
}
