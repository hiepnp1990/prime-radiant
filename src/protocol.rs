//! The hub's wire protocol.
//!
//! A frame is a 4-byte big-endian length and a JSON header, then a 4-byte big-endian length and a binary
//! payload. Only segments and versions carry a payload, their bytes, so the bytes are never encoded as text.
//! A peer answers each request with one response, in order.

use std::io::{self, Read};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::raw::{Applied, Sha256, Source, StreamKey, StreamState, Write};

pub const VERSION: u32 = 1;
pub const MAX_HEADER_BYTES: u32 = 64 << 20;
/// A version larger than this cannot be sent.
pub const MAX_PAYLOAD_BYTES: u32 = 256 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Writes the streams of its origin.
    Collector,
    /// Only reads.
    Client,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    /// The first request on every connection.
    Hello {
        protocol: u32,
        origin: String,
        role: Role,
    },
    /// Every stream that the origin has from the source, with its confirmed state.
    State {
        origin: String,
        source: Source,
    },
    Write(Write),
    Find {
        query: String,
        limit: usize,
    },
    Brief {
        session: String,
    },
    Stats,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Hello {
        protocol: u32,
    },
    State {
        streams: Vec<(StreamKey, StreamState)>,
    },
    /// The outcome of a write and the stream's confirmed state after it, or `None` if the stream does not exist.
    Applied {
        outcome: Applied,
        state: Option<StreamState>,
    },
    /// The same JSON that `find --json` prints.
    Found {
        hits: Value,
    },
    /// The same JSON that `brief --json` prints.
    Brief {
        brief: Value,
    },
    /// The same JSON that `stats --json` prints.
    Stats {
        stats: Value,
    },
    /// The hub could not answer. After a protocol violation, it also closes the connection.
    Error {
        message: String,
    },
}

/// A request as its JSON header carries it.
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Header {
    Hello {
        protocol: u32,
        origin: String,
        role: Role,
    },
    State {
        origin: String,
        source: Source,
    },
    Write(WriteHeader),
    Find {
        query: String,
        limit: usize,
    },
    Brief {
        session: String,
    },
    Stats,
}

/// A write without its bytes.
#[derive(Serialize, Deserialize)]
#[serde(tag = "write", rename_all = "snake_case")]
enum WriteHeader {
    Segment {
        key: StreamKey,
        generation: u64,
        start: u64,
        sha256: Sha256,
    },
    Version {
        key: StreamKey,
        sha256: Sha256,
    },
    NewGeneration {
        key: StreamKey,
        generation: u64,
    },
}

pub fn write_request(out: &mut impl io::Write, request: &Request) -> io::Result<()> {
    let (header, payload): (Header, &[u8]) = match request {
        Request::Hello {
            protocol,
            origin,
            role,
        } => (
            Header::Hello {
                protocol: *protocol,
                origin: origin.clone(),
                role: *role,
            },
            &[],
        ),
        Request::State { origin, source } => (
            Header::State {
                origin: origin.clone(),
                source: *source,
            },
            &[],
        ),
        Request::Write(Write::Segment {
            key,
            generation,
            start,
            bytes,
            sha256,
        }) => (
            Header::Write(WriteHeader::Segment {
                key: key.clone(),
                generation: *generation,
                start: *start,
                sha256: *sha256,
            }),
            bytes,
        ),
        Request::Write(Write::Version { key, sha256, bytes }) => (
            Header::Write(WriteHeader::Version {
                key: key.clone(),
                sha256: *sha256,
            }),
            bytes,
        ),
        Request::Write(Write::NewGeneration { key, generation }) => (
            Header::Write(WriteHeader::NewGeneration {
                key: key.clone(),
                generation: *generation,
            }),
            &[],
        ),
        Request::Find { query, limit } => (
            Header::Find {
                query: query.clone(),
                limit: *limit,
            },
            &[],
        ),
        Request::Brief { session } => (
            Header::Brief {
                session: session.clone(),
            },
            &[],
        ),
        Request::Stats => (Header::Stats, &[]),
    };
    write_frame(out, &header, payload)
}

/// The next request, or `None` if the peer closed the connection between frames.
pub fn read_request(input: &mut impl Read) -> io::Result<Option<Request>> {
    let Some((header, payload)) = read_frame(input)? else {
        return Ok(None);
    };
    let request = match serde_json::from_slice(&header)? {
        Header::Write(WriteHeader::Segment {
            key,
            generation,
            start,
            sha256,
        }) => Request::Write(Write::Segment {
            key,
            generation,
            start,
            bytes: payload,
            sha256,
        }),
        Header::Write(WriteHeader::Version { key, sha256 }) => Request::Write(Write::Version {
            key,
            sha256,
            bytes: payload,
        }),
        _ if !payload.is_empty() => {
            return Err(invalid("only a segment or a version carries a payload"));
        }
        Header::Write(WriteHeader::NewGeneration { key, generation }) => {
            Request::Write(Write::NewGeneration { key, generation })
        }
        Header::Hello {
            protocol,
            origin,
            role,
        } => Request::Hello {
            protocol,
            origin,
            role,
        },
        Header::State { origin, source } => Request::State { origin, source },
        Header::Find { query, limit } => Request::Find { query, limit },
        Header::Brief { session } => Request::Brief { session },
        Header::Stats => Request::Stats,
    };
    Ok(Some(request))
}

pub fn write_response(out: &mut impl io::Write, response: &Response) -> io::Result<()> {
    write_frame(out, response, &[])
}

/// The next response, or `None` if the peer closed the connection between frames.
pub fn read_response(input: &mut impl Read) -> io::Result<Option<Response>> {
    let Some((header, payload)) = read_frame(input)? else {
        return Ok(None);
    };
    if !payload.is_empty() {
        return Err(invalid("a response carries no payload"));
    }
    Ok(Some(serde_json::from_slice(&header)?))
}

fn write_frame(
    out: &mut impl io::Write,
    header: &impl Serialize,
    payload: &[u8],
) -> io::Result<()> {
    let header = serde_json::to_vec(header)?;
    let header_len = part_len(&header, MAX_HEADER_BYTES)?;
    let payload_len = part_len(payload, MAX_PAYLOAD_BYTES)?;
    out.write_all(&header_len.to_be_bytes())?;
    out.write_all(&header)?;
    out.write_all(&payload_len.to_be_bytes())?;
    out.write_all(payload)?;
    out.flush()
}

fn part_len(part: &[u8], limit: u32) -> io::Result<u32> {
    u32::try_from(part.len())
        .ok()
        .filter(|len| *len <= limit)
        .ok_or_else(|| {
            invalid(format!(
                "a {}-byte frame part is over the limit",
                part.len()
            ))
        })
}

fn read_frame(input: &mut impl Read) -> io::Result<Option<(Vec<u8>, Vec<u8>)>> {
    let mut len = [0; 4];
    let mut filled = 0;
    while filled < len.len() {
        match input.read(&mut len[filled..]) {
            Ok(0) if filled == 0 => return Ok(None),
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => filled += n,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    let header = read_part(input, u32::from_be_bytes(len), MAX_HEADER_BYTES)?;
    input.read_exact(&mut len)?;
    let payload = read_part(input, u32::from_be_bytes(len), MAX_PAYLOAD_BYTES)?;
    Ok(Some((header, payload)))
}

fn read_part(input: &mut impl Read, len: u32, limit: u32) -> io::Result<Vec<u8>> {
    if len > limit {
        return Err(invalid(format!(
            "a {len}-byte frame part is over the {limit}-byte limit"
        )));
    }
    // The buffer grows as bytes arrive, so a false length cannot claim memory up front.
    let mut part = Vec::new();
    input.take(u64::from(len)).read_to_end(&mut part)?;
    if part.len() != len as usize {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    Ok(part)
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}
