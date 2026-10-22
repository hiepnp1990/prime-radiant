use serde::Deserialize;
use serde_json::Value;

use super::{
    Alias, AliasKind, Cuts, Facts, Identity, Line, Lineage, Parser, Role, Timestamp, Version,
    heading, message,
};
use crate::raw::Sha256;
use crate::redact::{self, Redacted};

const SESSION_ID_LEN: usize = 36;
const CHAT_HISTORY: &str = "chat_history.jsonl";
const EVENTS: &str = "events.jsonl";
const UPDATES: &str = "updates.jsonl";
const SUMMARY: &str = "summary.json";
const USER_QUERY: (&str, &str) = ("<user_query>", "</user_query>");

pub struct Grok;

impl Parser for Grok {
    /// A session folder is `sessions/<URL-encoded working folder>/<session id>`.
    fn session(&self, locator: &str, _first_line: Option<&[u8]>) -> Option<Identity> {
        let ["sessions", folder, session, file] = locator.split('/').collect::<Vec<_>>()[..] else {
            return None;
        };
        let is_id = session.len() == SESSION_ID_LEN
            && session.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-');
        if !is_id || ![CHAT_HISTORY, EVENTS, UPDATES, SUMMARY].contains(&file) {
            return None;
        }
        Some(Identity {
            native_id: session.to_owned(),
            lineage: Lineage::Main,
            aliases: vec![Alias {
                kind: AliasKind::NativeId,
                value: session.to_owned(),
            }],
            project: percent_decoded(folder),
        })
    }

    /// Reads a line of any of the three logs: an update has `params`, an event has `ts`, and a chat history line has neither.
    fn line(&self, session: &Identity, line: &[u8], cuts: Cuts) -> Line {
        // Chat history lines carry no IDs of their own, so a line's hash names it within its session.
        let line_id = || format!("{}:{}", session.native_id, &Sha256::of(line).to_hex()[..16]);
        match serde_json::from_slice::<RawLine>(line) {
            Ok(RawLine::Update(update)) => update.line(line_id, cuts),
            Ok(RawLine::Event { ts }) => Line {
                facts: Facts {
                    started: Timestamp::parse_utc(&ts),
                    ..Facts::default()
                },
                ..Line::default()
            },
            Ok(RawLine::Chat(chat)) => chat.line(&session.native_id, line_id, cuts),
            Err(_) => Line::default(),
        }
    }

    fn version(&self, _session: &Identity, bytes: &[u8], _cuts: Cuts) -> Version {
        let Ok(summary) = serde_json::from_slice::<Summary>(bytes) else {
            return Version::default();
        };
        Version {
            facts: Facts {
                title: [summary.generated_title, summary.session_summary]
                    .into_iter()
                    .flatten()
                    .find_map(|title| heading(&title)),
                started: summary.created_at.as_deref().and_then(Timestamp::parse_utc),
                ..Facts::default()
            },
            ..Version::default()
        }
    }

    /// A folder with both transcripts holds the same turns twice. The updates win, because only
    /// they carry timestamps and the prompts as the user typed them.
    fn superseded_by(&self, locator: &str) -> Option<String> {
        let (folder, file) = locator.rsplit_once('/')?;
        (file == CHAT_HISTORY).then(|| format!("{folder}/{UPDATES}"))
    }
}

#[derive(Deserialize)]
struct Summary {
    generated_title: Option<String>,
    session_summary: Option<String>,
    created_at: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawLine {
    Update(UpdateLine),
    /// A line of `events.jsonl`, which times each step of a turn but holds none of its text.
    Event {
        ts: String,
    },
    Chat(ChatLine),
}

/// One Agent Client Protocol notification from `updates.jsonl`.
#[derive(Deserialize)]
struct UpdateLine {
    /// Seconds since the Unix epoch.
    timestamp: Option<i64>,
    params: Params,
}

#[derive(Deserialize)]
struct Params {
    update: SessionUpdate,
    #[serde(rename = "_meta", default)]
    meta: EventMeta,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EventMeta {
    event_id: Option<String>,
    agent_timestamp_ms: Option<i64>,
}

#[derive(Deserialize)]
#[serde(tag = "sessionUpdate", rename_all = "snake_case")]
enum SessionUpdate {
    UserMessageChunk {
        content: Text,
        #[serde(rename = "_meta", default)]
        meta: PromptMeta,
    },
    AgentMessageChunk {
        content: Text,
    },
    AgentThoughtChunk {
        content: Text,
    },
    #[serde(rename_all = "camelCase")]
    ToolCall {
        tool_call_id: String,
        title: Option<String>,
        #[serde(default)]
        raw_input: Value,
    },
    ToolCallUpdate {
        status: Option<String>,
        content: Option<Vec<ToolContent>>,
    },
    #[serde(other)]
    Other,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PromptMeta {
    /// Set on prompts that Grok sends for the user, such as a background task's result.
    #[serde(default)]
    hide_from_scrollback: bool,
}

#[derive(Deserialize)]
struct Text {
    text: Option<String>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ToolContent {
    Content {
        content: Text,
    },
    Diff {
        path: Option<String>,
    },
    #[serde(other)]
    Other,
}

impl UpdateLine {
    fn line(self, line_id: impl Fn() -> String, cuts: Cuts) -> Line {
        let at = self
            .params
            .meta
            .agent_timestamp_ms
            .or(self.timestamp.and_then(|seconds| seconds.checked_mul(1000)))
            .map(Timestamp);
        let id = self.params.meta.event_id.unwrap_or_else(line_id);
        let mut facts = Facts {
            started: at,
            ..Facts::default()
        };
        let (role, text) = match self.params.update {
            SessionUpdate::UserMessageChunk { content, meta } if !meta.hide_from_scrollback => {
                facts.fallback_title = content.text.as_deref().and_then(heading);
                (Role::User, content.text.as_deref().map(redact::secrets))
            }
            SessionUpdate::AgentMessageChunk { content }
            | SessionUpdate::AgentThoughtChunk { content } => (
                Role::Assistant,
                content.text.as_deref().map(redact::secrets),
            ),
            SessionUpdate::ToolCall {
                tool_call_id,
                title,
                raw_input,
            } => (
                Role::ToolCall,
                Some(tool_call(&title.unwrap_or(tool_call_id), &raw_input, cuts)),
            ),
            SessionUpdate::ToolCallUpdate {
                status: Some(status),
                content,
            } if status == "completed" || status == "failed" => {
                let texts: Vec<String> = content
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|part| match part {
                        ToolContent::Content { content } => content.text,
                        ToolContent::Diff { path } => path,
                        ToolContent::Other => None,
                    })
                    .collect();
                (Role::ToolResult, Some(tool_output(&texts.join("\n"), cuts)))
            }
            _ => {
                return Line {
                    facts,
                    ..Line::default()
                };
            }
        };
        Line {
            messages: message(Some(id), role, at, text).into_iter().collect(),
            facts,
        }
    }
}

/// One line of `chat_history.jsonl`, the conversation as Grok sends it to the model.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ChatLine {
    User {
        #[serde(default)]
        content: Vec<Text>,
        /// Set on context that Grok adds, such as system reminders.
        synthetic_reason: Option<String>,
    },
    Assistant {
        content: Option<String>,
        #[serde(default)]
        tool_calls: Vec<ChatToolCall>,
    },
    ToolResult {
        tool_call_id: String,
        content: Option<String>,
    },
    /// Only the summary is readable. The reasoning itself is in `encrypted_content`.
    Reasoning {
        id: Option<String>,
        #[serde(default)]
        summary: Vec<Text>,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct ChatToolCall {
    id: String,
    name: String,
    #[serde(default)]
    arguments: String,
}

impl ChatLine {
    fn line(self, session: &str, line_id: impl Fn() -> String, cuts: Cuts) -> Line {
        let scoped = |id: &str| format!("{session}:{id}");
        let mut facts = Facts::default();
        let messages = match self {
            ChatLine::User {
                content,
                synthetic_reason: None,
            } => {
                let text = texts(&content);
                let typed = text.as_deref().and_then(typed);
                facts.fallback_title = typed.and_then(heading);
                message(
                    Some(line_id()),
                    Role::User,
                    None,
                    typed.map(redact::secrets),
                )
                .into_iter()
                .collect()
            }
            ChatLine::Assistant {
                content,
                tool_calls,
            } => {
                let text = message(
                    Some(line_id()),
                    Role::Assistant,
                    None,
                    content.as_deref().map(redact::secrets),
                );
                let calls = tool_calls.into_iter().filter_map(|call| {
                    let input = serde_json::from_str(&call.arguments)
                        .unwrap_or(Value::String(call.arguments));
                    message(
                        Some(scoped(&call.id)),
                        Role::ToolCall,
                        None,
                        Some(tool_call(&call.name, &input, cuts)),
                    )
                });
                text.into_iter().chain(calls).collect()
            }
            ChatLine::ToolResult {
                tool_call_id,
                content,
            } => message(
                Some(scoped(&format!("{tool_call_id}:result"))),
                Role::ToolResult,
                None,
                content.as_deref().map(|text| tool_output(text, cuts)),
            )
            .into_iter()
            .collect(),
            ChatLine::Reasoning { id, summary } => message(
                Some(id.map_or_else(&line_id, |id| scoped(&id))),
                Role::Assistant,
                None,
                texts(&summary).as_deref().map(redact::secrets),
            )
            .into_iter()
            .collect(),
            ChatLine::User { .. } | ChatLine::Other => Vec::new(),
        };
        Line { messages, facts }
    }
}

/// The text the user typed, without the context that Grok wraps around it.
fn typed(text: &str) -> Option<&str> {
    let (open, close) = USER_QUERY;
    match text.split_once(open) {
        Some((_, query)) => query.split(close).next().map(str::trim),
        None => (!text.trim_start().starts_with('<')).then_some(text),
    }
}

fn texts(parts: &[Text]) -> Option<String> {
    let texts: Vec<&str> = parts
        .iter()
        .filter_map(|part| part.text.as_deref())
        .collect();
    (!texts.is_empty()).then(|| texts.join("\n"))
}

fn tool_call(name: &str, input: &Value, cuts: Cuts) -> Redacted {
    redact::tool_call(name, input).cut(cuts.tool_call)
}

fn tool_output(text: &str, cuts: Cuts) -> Redacted {
    redact::secrets(text).cut(cuts.tool_result)
}

/// Decodes `%XX` escapes, the way Grok names a session's working folder.
fn percent_decoded(text: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(text.len());
    let mut rest = text.as_bytes();
    while let Some((&byte, tail)) = rest.split_first() {
        if byte == b'%' {
            let hex = tail
                .get(..2)
                .filter(|hex| hex.iter().all(u8::is_ascii_hexdigit))?;
            bytes.push(u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?);
            rest = &tail[2..];
        } else {
            bytes.push(byte);
            rest = tail;
        }
    }
    String::from_utf8(bytes).ok()
}
