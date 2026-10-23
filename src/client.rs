//! Reaching the hub: a collector keeps one connection open, and a query command asks one question and hangs up.

use std::fmt;
use std::io::{self, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

use crate::protocol::{self, OriginStats, Request, Response, Role};
use crate::{Error, Result};

/// The origin a query client names in its hello. A client writes nothing, so its origin is never stored.
const CLIENT_ORIGIN: &str = "client";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HubAddress {
    /// The hub's socket on this machine.
    Socket(PathBuf),
    /// A shell command whose stdin and stdout reach the hub, such as `ssh <host> prime-radiant hub connect --data <dir>`.
    Command(String),
}

impl fmt::Display for HubAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HubAddress::Socket(path) => write!(f, "the hub socket {}", path.display()),
            HubAddress::Command(command) => write!(f, "the hub command {command}"),
        }
    }
}

/// Both directions of a connection to the hub, and what to close when it ends.
pub(crate) struct Transport {
    pub from_hub: Box<dyn Read + Send>,
    pub to_hub: Box<dyn Write + Send>,
    pub socket: Option<UnixStream>,
    pub child: Option<Child>,
}

impl HubAddress {
    pub(crate) fn open(&self) -> io::Result<Transport> {
        match self {
            HubAddress::Socket(path) => {
                let socket = UnixStream::connect(path).map_err(|error| {
                    io::Error::new(error.kind(), format!("{}: {error}", path.display()))
                })?;
                Ok(Transport {
                    from_hub: Box::new(socket.try_clone()?),
                    to_hub: Box::new(socket.try_clone()?),
                    socket: Some(socket),
                    child: None,
                })
            }
            HubAddress::Command(command) => {
                let mut child = Command::new("sh")
                    .arg("-c")
                    .arg(command)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .spawn()?;
                Ok(Transport {
                    from_hub: Box::new(child.stdout.take().expect("stdout is piped")),
                    to_hub: Box::new(child.stdin.take().expect("stdin is piped")),
                    socket: None,
                    child: Some(child),
                })
            }
        }
    }
}

/// Asks the hub one question as a read-only client. A refusal from the hub is an `Error::Usage` with its message, the
/// error the same question gets from a local data folder.
pub fn ask(hub: &HubAddress, request: Request) -> Result<Response> {
    let Transport {
        from_hub,
        mut to_hub,
        socket,
        child,
    } = hub.open()?;
    let hello = Request::Hello {
        protocol: protocol::VERSION,
        origin: CLIENT_ORIGIN.to_owned(),
        role: Role::Client,
    };
    protocol::write_request(&mut to_hub, &hello)?;
    protocol::write_request(&mut to_hub, &request)?;
    // The end of the requests lets the hub, and `hub connect` in between, finish and close.
    drop(to_hub);
    if let Some(socket) = &socket {
        socket.shutdown(Shutdown::Write)?;
    }
    let mut from_hub = BufReader::new(from_hub);
    let mut next = || -> Result<Response> {
        protocol::read_response(&mut from_hub)?.ok_or_else(|| {
            Error::Io(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the hub closed the connection without an answer",
            ))
        })
    };
    let answer = match next()? {
        Response::Hello {
            protocol: protocol::VERSION,
        } => next()?,
        other => other,
    };
    if let Some(mut child) = child {
        child.wait()?;
    }
    match answer {
        Response::Error { message } => Err(Error::Usage(message)),
        answer => Ok(answer),
    }
}

/// Why the hub named no origins.
#[derive(Debug)]
pub enum NoOrigins {
    /// The hub could not decode the question, so it is older than this client.
    Older(String),
    /// The hub, or the command in between, answered with an error.
    Refused(String),
    Unreachable(io::Error),
    Other(String),
}

impl fmt::Display for NoOrigins {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NoOrigins::Older(message) => write!(
                f,
                "the hub does not know this question, so it is older than this client: {message}"
            ),
            NoOrigins::Refused(message) => write!(f, "the hub could not answer: {message}"),
            NoOrigins::Unreachable(error) => write!(f, "cannot reach the hub: {error}"),
            NoOrigins::Other(message) => f.write_str(message),
        }
    }
}

/// Asks the hub how much it holds from each origin.
pub fn origins(hub: &HubAddress) -> std::result::Result<Vec<OriginStats>, NoOrigins> {
    match ask(hub, Request::Origins) {
        Ok(Response::Origins { origins }) => Ok(origins),
        Ok(other) => Err(NoOrigins::Other(format!("the hub answered {other:?}"))),
        // A hub answers "bad frame" to a request it cannot decode, which this client sends only to a hub older than
        // the question.
        Err(Error::Usage(message)) if message.starts_with("bad frame") => {
            Err(NoOrigins::Older(message))
        }
        Err(Error::Usage(message)) => Err(NoOrigins::Refused(message)),
        Err(Error::Io(error)) => Err(NoOrigins::Unreachable(error)),
        Err(error) => Err(NoOrigins::Other(error.to_string())),
    }
}
