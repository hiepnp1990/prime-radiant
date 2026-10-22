use std::sync::LazyLock;

use base64::Engine as _;
use regex::Regex;
use serde::Deserialize;
use serde_json::Value;

use super::{
    Alias, AliasKind, Artifact, ArtifactKind, Contents, Facts, Identity, Line, Lineage, Media,
    Message, Parser, Role, Timestamp,
};
use crate::raw::Sha256;
use crate::redact;

const TOOL_USE_CHARS: usize = 500;
const TOOL_RESULT_CHARS: usize = 2000;
const ATTACHMENT_CHARS: usize = 500;

const PERSISTED_OUTPUT: &str = "<persisted-output>";
const FILE_EDITING_TOOLS: [&str; 3] = ["Edit", "Write", "MultiEdit"];

static PULL_REQUEST: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"https://github\.com/[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+/pull/[0-9]+")
        .expect("pull request pattern compiles")
});

pub struct ClaudeCode;

impl Parser for ClaudeCode {
    fn session(&self, locator: &str, first_line: Option<&[u8]>) -> Option<Identity> {
        let parts: Vec<&str> = locator.split('/').collect();
        match parts.as_slice() {
            [_, file] => {
                let stem = file.strip_suffix(".jsonl")?;
                match stem.strip_prefix("agent-") {
                    // A subagent transcript beside the main ones names its parent session only on its lines.
                    Some(agent) => {
                        let line: RawLine = serde_json::from_slice(first_line?).ok()?;
                        Some(subagent(&line.session_id?, agent))
                    }
                    None => Some(main(stem)),
                }
            }
            [_, session, "custom-title.json"] => Some(main(session)),
            [_, session, "subagents", file] => {
                let agent = file
                    .strip_suffix(".jsonl")
                    .or_else(|| file.strip_suffix(".meta.json"))?
                    .strip_prefix("agent-")?;
                Some(subagent(session, agent))
            }
            _ => None,
        }
    }

    fn line(&self, session: &Identity, line: &[u8]) -> Line {
        let Ok(line) = serde_json::from_slice::<RawLine>(line) else {
            return Line::default();
        };
        // A fork's transcript can start with lines copied from the session it forked, which keep that session's ID.
        let copied = session.lineage == Lineage::Main
            && line
                .session_id
                .as_deref()
                .is_some_and(|id| id != session.native_id);
        let facts = if copied {
            Facts::default()
        } else {
            Facts {
                title: line.custom_title.clone().or_else(|| line.ai_title.clone()),
                project: line.cwd.clone(),
                started: line.timestamp.as_deref().and_then(Timestamp::parse_utc),
                ..Facts::default()
            }
        };
        let message = line.uuid.clone().map(|id| message(id, line));
        Line { message, facts }
    }

    fn version(&self, bytes: &[u8]) -> Contents {
        let file: MetadataFile = serde_json::from_slice(bytes).unwrap_or_default();
        Contents::Facts(Facts {
            title: file.custom_title.or(file.description),
            ..Facts::default()
        })
    }
}

fn main(session: &str) -> Identity {
    Identity {
        native_id: session.to_owned(),
        lineage: Lineage::Main,
        aliases: vec![Alias {
            kind: AliasKind::NativeId,
            value: session.to_owned(),
        }],
    }
}

fn subagent(session: &str, agent: &str) -> Identity {
    let native_id = format!("{session}/agent-{agent}");
    Identity {
        aliases: vec![
            Alias {
                kind: AliasKind::NativeId,
                value: native_id.clone(),
            },
            Alias {
                kind: AliasKind::AgentId,
                value: agent.to_owned(),
            },
        ],
        native_id,
        lineage: Lineage::Subagent {
            parent_native_id: session.to_owned(),
        },
    }
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MetadataFile {
    custom_title: Option<String>,
    description: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawLine {
    #[serde(rename = "type", default)]
    kind: String,
    uuid: Option<String>,
    parent_uuid: Option<String>,
    session_id: Option<String>,
    #[serde(default)]
    is_meta: bool,
    timestamp: Option<String>,
    cwd: Option<String>,
    custom_title: Option<String>,
    ai_title: Option<String>,
    message: Option<RawMessage>,
    attachment: Option<RawAttachment>,
    content: Option<Content>,
}

#[derive(Deserialize)]
struct RawMessage {
    content: Option<Content>,
}

#[derive(Deserialize)]
struct RawAttachment {
    filename: Option<String>,
    snippet: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Content {
    Text(String),
    Blocks(Vec<Block>),
    Other(serde::de::IgnoredAny),
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Block {
    Text {
        text: String,
    },
    ToolUse {
        name: String,
        #[serde(default)]
        input: Value,
    },
    ToolResult {
        content: Option<Content>,
    },
    Image {
        source: ImageSource,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct ImageSource {
    media_type: Option<String>,
    data: Option<String>,
}

fn message(id: String, line: RawLine) -> Message {
    let content = line
        .message
        .and_then(|message| message.content)
        .or(line.content);
    let mut found = Found::default();
    if let Some(content) = &content {
        found.content(content);
    }
    if let Some(attachment) = &line.attachment {
        if let Some(filename) = &attachment.filename {
            found.text(filename, None);
        }
        if let Some(snippet) = &attachment.snippet {
            found.text(snippet, Some(ATTACHMENT_CHARS));
        }
    }
    Message {
        id,
        parent_id: line.parent_uuid,
        role: role(&line.kind, line.is_meta, content.as_ref()),
        at: line.timestamp.as_deref().and_then(Timestamp::parse_utc),
        searchable_text: found.pieces.join("\n"),
        artifacts: found.artifacts,
        media: found.media,
    }
}

fn role(kind: &str, is_meta: bool, content: Option<&Content>) -> Role {
    let blocks = match content {
        Some(Content::Blocks(blocks)) => blocks.as_slice(),
        _ => &[],
    };
    let has = |wanted: fn(&Block) -> bool| blocks.iter().any(wanted);
    match kind {
        "user" if has(|block| matches!(block, Block::ToolResult { .. })) => Role::ToolResult,
        // Claude Code marks text it injects into the user's turn, such as skill bodies and command caveats.
        "user" if is_meta => Role::System,
        "user" => Role::User,
        "assistant"
            if !has(|block| matches!(block, Block::Text { .. }))
                && has(|block| matches!(block, Block::ToolUse { .. })) =>
        {
            Role::ToolCall
        }
        "assistant" => Role::Assistant,
        "attachment" => Role::Attachment,
        _ => Role::System,
    }
}

#[derive(Default)]
struct Found {
    pieces: Vec<String>,
    artifacts: Vec<Artifact>,
    media: Vec<Media>,
}

impl Found {
    fn content(&mut self, content: &Content) {
        match content {
            Content::Text(text) => self.text(text, None),
            Content::Blocks(blocks) => blocks.iter().for_each(|block| self.block(block)),
            Content::Other(_) => {}
        }
    }

    fn block(&mut self, block: &Block) {
        match block {
            Block::Text { text } => self.text(text, None),
            Block::ToolUse { name, input } => self.tool_use(name, input),
            Block::ToolResult { content } => self.tool_result(content.as_ref()),
            Block::Image { source } => self.image(source),
            Block::Other => {}
        }
    }

    fn text(&mut self, text: &str, limit: Option<usize>) {
        if text.is_empty() {
            return;
        }
        self.pull_requests(text);
        let clean = redact::secrets(text);
        let clean = match limit {
            Some(limit) => clean.cut(limit),
            None => clean,
        };
        self.pieces.push(clean.into_string());
    }

    fn tool_use(&mut self, name: &str, input: &Value) {
        if FILE_EDITING_TOOLS.contains(&name)
            && let Some(path) = input.get("file_path").and_then(Value::as_str)
        {
            self.artifact(ArtifactKind::ChangedFile, path);
        }
        let text = redact::tool_call(name, input);
        self.pull_requests(text.as_str());
        self.pieces.push(text.cut(TOOL_USE_CHARS).into_string());
    }

    fn tool_result(&mut self, content: Option<&Content>) {
        let mut parts = Vec::new();
        match content {
            Some(Content::Text(text)) => parts.push(text.as_str()),
            Some(Content::Blocks(blocks)) => {
                for block in blocks {
                    match block {
                        Block::Text { text } => parts.push(text),
                        Block::Image { source } => self.image(source),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        let text = parts.join("\n");
        let limit = (!text.starts_with(PERSISTED_OUTPUT)).then_some(TOOL_RESULT_CHARS);
        self.text(&text, limit);
    }

    fn image(&mut self, source: &ImageSource) {
        let Some(decoded) = source
            .data
            .as_deref()
            .and_then(|data| base64::engine::general_purpose::STANDARD.decode(data).ok())
        else {
            return;
        };
        self.media.push(Media {
            hash: Sha256::of(&decoded),
            mime: source
                .media_type
                .clone()
                .unwrap_or_else(|| "application/octet-stream".to_owned()),
            bytes: decoded.len() as u64,
        });
    }

    fn pull_requests(&mut self, text: &str) {
        for url in PULL_REQUEST.find_iter(text) {
            self.artifact(ArtifactKind::PullRequest, url.as_str());
        }
    }

    fn artifact(&mut self, kind: ArtifactKind, value: &str) {
        let artifact = Artifact {
            kind,
            value: value.to_owned(),
        };
        if !self.artifacts.contains(&artifact) {
            self.artifacts.push(artifact);
        }
    }
}
