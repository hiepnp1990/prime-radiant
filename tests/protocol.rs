use std::io::{self, Cursor};

use prime_radiant::index::Timestamp;
use prime_radiant::protocol::{
    self, MAX_PAYLOAD_BYTES, Request, Response, Role, read_request, read_response, write_request,
    write_response,
};
use prime_radiant::query::{Ask, Filters, Find};
use prime_radiant::raw::{
    Applied, HeadCheck, Progress, Rejection, Sha256, Source, StreamKey, StreamState, Write,
};
use serde_json::json;

fn key() -> StreamKey {
    StreamKey {
        origin: "example-origin".to_owned(),
        source: Source::ClaudeCode,
        locator: "-home-dev-example-project/session.jsonl".to_owned(),
    }
}

fn encode_request(request: &Request) -> Vec<u8> {
    let mut frame = Vec::new();
    write_request(&mut frame, request).expect("request encodes");
    frame
}

fn decode_requests(frames: &[u8]) -> Vec<Request> {
    let mut input = Cursor::new(frames);
    let mut requests = Vec::new();
    while let Some(request) = read_request(&mut input).expect("frame decodes") {
        requests.push(request);
    }
    requests
}

/// A frame with an arbitrary header and payload, as a buggy or hostile peer could send it.
fn raw_frame(header: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::new();
    frame.extend((header.len() as u32).to_be_bytes());
    frame.extend(header);
    frame.extend((payload.len() as u32).to_be_bytes());
    frame.extend(payload);
    frame
}

#[test]
fn every_request_survives_a_round_trip() {
    let bytes = b"{\"type\":\"user\"}\n\x00\xff binary-safe".to_vec();
    let requests = vec![
        Request::Hello {
            protocol: protocol::VERSION,
            origin: "example-origin".to_owned(),
            role: Role::Collector,
        },
        Request::State {
            origin: "example-origin".to_owned(),
            source: Source::ClaudeCode,
        },
        Request::Write(Write::Segment {
            key: key(),
            generation: 2,
            start: 4096,
            sha256: Sha256::of(&bytes),
            bytes: bytes.clone(),
        }),
        Request::Write(Write::Version {
            key: key(),
            sha256: Sha256::of(&bytes),
            bytes: bytes.clone(),
        }),
        Request::Write(Write::NewGeneration {
            key: key(),
            generation: 3,
        }),
        Request::Ask(Ask::Find(Find {
            query: "json flag".to_owned(),
            limit: 10,
            ..Find::default()
        })),
        Request::Ask(Ask::Find(filtered_find())),
        Request::Ask(Ask::Brief {
            session: "33831ab7".to_owned(),
        }),
        Request::Ask(Ask::Read {
            session: "33831ab7".to_owned(),
            page: 2,
        }),
        Request::Ask(Ask::Grep {
            session: "33831ab7".to_owned(),
            pattern: "json flag".to_owned(),
        }),
        Request::Ask(Ask::Stats),
    ];
    let frames: Vec<u8> = requests.iter().flat_map(encode_request).collect();
    assert_eq!(
        decode_requests(&frames),
        requests,
        "back-to-back frames decode to the same requests"
    );
}

/// A find with every newer field set, and times with milliseconds.
fn filtered_find() -> Find {
    Find {
        query: String::new(),
        phrases: vec!["drop off".to_owned(), "the \"north\" gate".to_owned()],
        filters: Filters {
            sources: vec![Source::Codex, Source::T3Code],
            origins: vec!["origin-a".to_owned(), "origin-b".to_owned()],
            project: Some("Garden".to_owned()),
            since: Some(Timestamp(1_772_323_200_000)),
            until: Some(Timestamp(1_772_409_599_999)),
        },
        limit: 3,
    }
}

fn header_of(frame: &[u8]) -> serde_json::Value {
    let header_len = u32::from_be_bytes(frame[..4].try_into().unwrap()) as usize;
    serde_json::from_slice(&frame[4..4 + header_len]).expect("the header is JSON")
}

#[test]
fn a_find_without_filters_or_phrases_keeps_the_header_an_older_hub_reads() {
    let plain = Request::Ask(Ask::Find(Find {
        query: "json flag".to_owned(),
        limit: 10,
        ..Find::default()
    }));
    assert_eq!(
        header_of(&encode_request(&plain)),
        json!({"type": "find", "query": "json flag", "limit": 10}),
        "an unused filter or phrase adds nothing to the header"
    );
    assert_eq!(
        decode_requests(&raw_frame(
            br#"{"type":"find","query":"json flag","limit":10}"#,
            b""
        )),
        [plain],
        "an older client's header is a find with no filters or phrases"
    );

    let filtered = Request::Ask(Ask::Find(filtered_find()));
    assert_eq!(
        header_of(&encode_request(&filtered)),
        json!({
            "type": "find",
            "query": "",
            "limit": 3,
            "phrases": ["drop off", "the \"north\" gate"],
            "sources": ["codex", "t3-code"],
            "origins": ["origin-a", "origin-b"],
            "project": "Garden",
            "since_ms": 1_772_323_200_000_i64,
            "until_ms": 1_772_409_599_999_i64,
        }),
        "times travel as milliseconds, so the last millisecond of a day survives"
    );
}

#[test]
fn a_state_request_names_each_source_as_the_raw_layer_stores_it() {
    for (source, name) in [
        (Source::ClaudeCode, "claude-code"),
        (Source::Codex, "codex"),
        (Source::Grok, "grok"),
        (Source::T3Code, "t3-code"),
    ] {
        assert_eq!(source.as_str(), name);
        let request = Request::State {
            origin: "example-origin".to_owned(),
            source,
        };
        let frame = encode_request(&request);
        let header_len = u32::from_be_bytes(frame[..4].try_into().unwrap()) as usize;
        let header: serde_json::Value =
            serde_json::from_slice(&frame[4..4 + header_len]).expect("the header is JSON");
        assert_eq!(
            header,
            json!({"type": "state", "origin": "example-origin", "source": name})
        );
        assert_eq!(decode_requests(&frame), [request]);
    }
    let unknown = raw_frame(
        br#"{"type":"state","origin":"example-origin","source":"made-up"}"#,
        b"",
    );
    assert!(
        read_request(&mut unknown.as_slice()).is_err(),
        "an unknown source is an error"
    );
}

#[test]
fn segment_bytes_travel_as_the_raw_payload_after_the_json_header() {
    let bytes = vec![0xC3, 0x28, 0x00, b'\n'];
    let frame = encode_request(&Request::Write(Write::Segment {
        key: key(),
        generation: 0,
        start: 0,
        sha256: Sha256::of(&bytes),
        bytes: bytes.clone(),
    }));
    let header_len = u32::from_be_bytes(frame[..4].try_into().unwrap()) as usize;
    let header: serde_json::Value =
        serde_json::from_slice(&frame[4..4 + header_len]).expect("the header is JSON");
    assert_eq!(
        header,
        json!({
            "type": "write",
            "write": "segment",
            "key": {
                "origin": "example-origin",
                "source": "claude-code",
                "locator": "-home-dev-example-project/session.jsonl",
            },
            "generation": 0,
            "start": 0,
            "sha256": Sha256::of(&bytes).to_hex(),
        }),
        "the header names the write and holds no bytes"
    );
    let rest = &frame[4 + header_len..];
    assert_eq!(&rest[..4], (bytes.len() as u32).to_be_bytes());
    assert_eq!(
        &rest[4..],
        bytes.as_slice(),
        "the payload is the bytes, unencoded"
    );
}

#[test]
fn every_response_survives_a_round_trip() {
    let state = StreamState {
        generation: 1,
        progress: Progress::Append {
            confirmed_offset: 42,
            head: HeadCheck::of(b"{\"type\":\"user\"}\n"),
        },
    };
    let responses = vec![
        Response::Hello {
            protocol: protocol::VERSION,
        },
        Response::State {
            streams: vec![(key(), state.clone())],
        },
        Response::Applied {
            outcome: Applied::Appended {
                confirmed_offset: 42,
            },
            state: Some(state),
        },
        Response::Applied {
            outcome: Applied::Rejected(Rejection::Gap {
                confirmed_offset: 7,
            }),
            state: None,
        },
        Response::Found {
            hits: json!([{"key": "claude-code/x"}]),
        },
        Response::Error {
            message: "the first request must be hello".to_owned(),
        },
    ];
    let mut frames = Vec::new();
    for response in &responses {
        write_response(&mut frames, response).expect("response encodes");
    }
    let mut input = Cursor::new(frames);
    let mut decoded = Vec::new();
    while let Some(response) = read_response(&mut input).expect("frame decodes") {
        decoded.push(response);
    }
    assert_eq!(decoded, responses);
}

#[test]
fn malformed_frames_are_errors_and_a_clean_end_is_none() {
    let cases: Vec<(&str, Vec<u8>, io::ErrorKind)> = vec![
        (
            "unknown request type",
            raw_frame(br#"{"type":"delete_everything"}"#, b""),
            io::ErrorKind::InvalidData,
        ),
        (
            "header that is not JSON",
            raw_frame(b"\x00\x01not json", b""),
            io::ErrorKind::InvalidData,
        ),
        (
            "payload on a request that takes none",
            raw_frame(br#"{"type":"stats"}"#, b"stray"),
            io::ErrorKind::InvalidData,
        ),
        (
            "payload length over the limit",
            [
                raw_frame(br#"{"type":"stats"}"#, b"")[..20].to_vec(),
                (MAX_PAYLOAD_BYTES + 1).to_be_bytes().to_vec(),
            ]
            .concat(),
            io::ErrorKind::InvalidData,
        ),
        (
            "frame cut off inside the payload",
            raw_frame(br#"{"type":"stats"}"#, b"12345")[..26].to_vec(),
            io::ErrorKind::UnexpectedEof,
        ),
        (
            "frame cut off inside a length",
            vec![0, 0],
            io::ErrorKind::UnexpectedEof,
        ),
    ];
    for (name, frame, kind) in cases {
        let error = read_request(&mut Cursor::new(frame)).expect_err(name);
        assert_eq!(error.kind(), kind, "{name}: {error}");
    }
    assert!(
        read_request(&mut Cursor::new(Vec::new()))
            .expect("an empty stream is not an error")
            .is_none(),
        "the end between frames is a clean close"
    );
}
