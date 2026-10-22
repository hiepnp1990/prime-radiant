use std::io::Read as _;

use serde::Deserialize;
use serde::de::{DeserializeOwned, IgnoredAny};
use serde_json::Value;

use super::{
    Alias, AliasKind, Facts, Identity, Line, Lineage, Parser, Role, Timestamp, Version, heading,
    message, read_lines,
};
use crate::redact;

const TOOL_CALL_CHARS: usize = 500;
const TOOL_OUTPUT_CHARS: usize = 2000;
const THREAD_ID_LEN: usize = 36;
/// The IDE extensions put the open files first and the typed request after this heading.
const IDE_REQUEST: &str = "## My request";
const AGENTS_MD: &str = "# AGENTS.md instructions";
const DELEGATION: &str = "<codex_delegation>";
const NEW_TASK: &str = "Message Type: NEW_TASK";
const PAYLOAD: &str = "Payload:";
const TASK_NAME: &str = "Task name:";

pub struct Codex;

impl Parser for Codex {
    /// A rollout is `rollout-<time>-<thread id>[_<branch id>].jsonl[.zst]`, and every rollout of a thread feeds one session.
    fn session(&self, locator: &str, _first_line: Option<&[u8]>) -> Option<Identity> {
        let name = locator.rsplit('/').next()?.strip_prefix("rollout-")?;
        let stem = name
            .strip_suffix(".jsonl.zst")
            .or_else(|| name.strip_suffix(".jsonl"))?;
        let stem = stem.split('_').next()?;
        let thread = stem.get(stem.len().checked_sub(THREAD_ID_LEN)?..)?;
        if !thread.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-') {
            return None;
        }
        Some(Identity {
            native_id: thread.to_owned(),
            lineage: Lineage::Main,
            project: None,
            aliases: vec![Alias {
                kind: AliasKind::NativeId,
                value: thread.to_owned(),
            }],
        })
    }

    fn line(&self, session: &Identity, line: &[u8]) -> Line {
        let Ok(line) = serde_json::from_slice::<RawLine>(line) else {
            return Line::default();
        };
        let at = line.timestamp.as_deref().and_then(Timestamp::parse_utc);
        // Every thread counts ordinals from zero, so only the thread makes an ordinal name one message.
        let fallback_id = line
            .ordinal
            .map(|ordinal| format!("{}:ordinal:{ordinal}", session.native_id));
        match line.kind.as_str() {
            // A subagent's rollout repeats its parent's `session_meta`, which says nothing about this thread.
            "session_meta" => payload::<Meta>(line.payload)
                .filter(|meta| meta.id == session.native_id)
                .map_or_else(Line::default, |meta| Line {
                    messages: Vec::new(),
                    facts: Facts {
                        started: at,
                        ..meta.facts()
                    },
                }),
            "response_item" => payload::<Item>(line.payload)
                .map(|item| item.line(fallback_id, at))
                .unwrap_or_default(),
            "compacted" => {
                let summary = payload::<Compacted>(line.payload).and_then(|c| c.message);
                Line {
                    messages: message(
                        fallback_id,
                        Role::System,
                        at,
                        summary.as_deref().map(redact::secrets),
                    )
                    .into_iter()
                    .collect(),
                    facts: Facts::default(),
                }
            }
            _ => Line::default(),
        }
    }

    fn version(&self, session: &Identity, bytes: &[u8]) -> Version {
        let (facts, messages) = read_lines(self, session, &decoded_prefix(bytes));
        Version {
            facts,
            messages,
            ..Version::default()
        }
    }
}

/// Decompresses `bytes` as far as they decode, so a truncated `.zst` still yields its leading lines.
fn decoded_prefix(bytes: &[u8]) -> Vec<u8> {
    let mut decoded = Vec::new();
    let _ = zstd::stream::read::Decoder::new(bytes).and_then(|mut d| d.read_to_end(&mut decoded));
    decoded
}

fn payload<T: DeserializeOwned>(payload: Value) -> Option<T> {
    serde_json::from_value(payload).ok()
}

#[derive(Deserialize)]
struct RawLine {
    #[serde(rename = "type", default)]
    kind: String,
    timestamp: Option<String>,
    ordinal: Option<u64>,
    #[serde(default)]
    payload: Value,
}

#[derive(Deserialize)]
struct Meta {
    id: String,
    session_id: Option<String>,
    parent_thread_id: Option<String>,
    cwd: Option<String>,
    #[serde(default)]
    source: Value,
}

impl Meta {
    fn facts(self) -> Facts {
        let spawned_by = self.source["subagent"]["thread_spawn"]["parent_thread_id"]
            .as_str()
            .map(str::to_owned);
        let parent = self
            .parent_thread_id
            .or(spawned_by)
            .or(self.session_id)
            .filter(|parent| *parent != self.id);
        Facts {
            project: self.cwd,
            lineage: Some(match parent {
                Some(parent_native_id) => Lineage::Subagent { parent_native_id },
                None => Lineage::Main,
            }),
            ..Facts::default()
        }
    }
}

#[derive(Deserialize)]
struct Compacted {
    message: Option<String>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Item {
    Message {
        id: Option<String>,
        role: String,
        #[serde(default)]
        content: Vec<Part>,
    },
    AgentMessage {
        id: Option<String>,
        #[serde(default)]
        content: Vec<Part>,
    },
    FunctionCall {
        id: Option<String>,
        name: String,
        #[serde(default)]
        arguments: String,
    },
    CustomToolCall {
        id: Option<String>,
        name: String,
        #[serde(default)]
        input: String,
    },
    FunctionCallOutput {
        id: Option<String>,
        output: Output,
    },
    CustomToolCallOutput {
        id: Option<String>,
        output: Output,
    },
    /// Only the summary is readable. The reasoning itself is in `encrypted_content`.
    Reasoning {
        id: Option<String>,
        #[serde(default)]
        summary: Vec<Part>,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct Part {
    text: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Output {
    Text(String),
    Parts(Vec<Part>),
    Other(IgnoredAny),
}

impl Item {
    fn line(self, fallback_id: Option<String>, at: Option<Timestamp>) -> Line {
        let mut facts = Facts::default();
        let (id, role, text) = match self {
            Item::Message { id, role, content } => {
                let role = match role.as_str() {
                    "user" => Role::User,
                    "assistant" => Role::Assistant,
                    _ => return Line::default(),
                };
                if role == Role::User {
                    facts.fallback_title = typed_title(&content);
                }
                (id, role, texts(&content).as_deref().map(redact::secrets))
            }
            Item::AgentMessage { id, content } => {
                facts.fallback_title = task_title(&content);
                (
                    id,
                    Role::Assistant,
                    texts(&content).as_deref().map(redact::secrets),
                )
            }
            Item::FunctionCall {
                id,
                name,
                arguments,
            } => {
                let input = serde_json::from_str(&arguments).unwrap_or(Value::String(arguments));
                let call = redact::tool_call(&name, &input);
                (id, Role::ToolCall, Some(call.cut(TOOL_CALL_CHARS)))
            }
            Item::CustomToolCall { id, name, input } => {
                let call = redact::secrets(&format!("{name} {input}"));
                (id, Role::ToolCall, Some(call.cut(TOOL_CALL_CHARS)))
            }
            Item::FunctionCallOutput { id, output } | Item::CustomToolCallOutput { id, output } => {
                let text = match output {
                    Output::Text(text) => Some(text),
                    Output::Parts(parts) => texts(&parts),
                    Output::Other(_) => None,
                };
                let text = text.map(|text| redact::secrets(&text).cut(TOOL_OUTPUT_CHARS));
                (id, Role::ToolResult, text)
            }
            Item::Reasoning { id, summary } => (
                id,
                Role::Assistant,
                texts(&summary).as_deref().map(redact::secrets),
            ),
            Item::Other => return Line::default(),
        };
        Line {
            messages: message(id.or(fallback_id), role, at, text)
                .into_iter()
                .collect(),
            facts,
        }
    }
}

fn texts(parts: &[Part]) -> Option<String> {
    let texts: Vec<&str> = parts
        .iter()
        .filter_map(|part| part.text.as_deref())
        .collect();
    (!texts.is_empty()).then(|| texts.join("\n"))
}

/// The first request the user typed, skipping the context that Codex injects into user messages.
fn typed_title(parts: &[Part]) -> Option<String> {
    parts
        .iter()
        .filter_map(|part| part.text.as_deref())
        .find_map(typed)
        .and_then(heading)
}

fn typed(text: &str) -> Option<&str> {
    let text = text.trim_start();
    if let Some(delegation) = text.strip_prefix(DELEGATION) {
        return delegation.split_once("<input>")?.1.split("</input>").next();
    }
    if text.starts_with('<') || text.starts_with(AGENTS_MD) {
        return None;
    }
    Some(match text.find(IDE_REQUEST) {
        Some(at) => text[at..]
            .split_once('\n')
            .map_or("", |(_, request)| request),
        None => text,
    })
}

/// The task in a message that starts a subagent, or the task's name when the task itself is encrypted.
fn task_title(parts: &[Part]) -> Option<String> {
    let text = parts.iter().find_map(|part| part.text.as_deref())?;
    let (header, task) = text.strip_prefix(NEW_TASK)?.split_once(PAYLOAD)?;
    heading(task).or_else(|| {
        let name = header
            .lines()
            .find_map(|line| line.strip_prefix(TASK_NAME))?;
        heading(name)
    })
}
