use serde::Deserialize;
use serde_json::value::RawValue;

use super::{
    Alias, AliasKind, Artifact, ArtifactKind, Facts, Identity, Lifecycle, Line, Lineage, Message,
    Parser, Role, Timestamp, Version,
};
use crate::redact;

/// T3 Code's driver names for harnesses that Prime Radiant indexes under their own source.
const DRIVERS: [(&str, &str); 2] = [("claudeAgent", "claude-code"), ("codex", "codex")];

pub struct T3Code;

impl Parser for T3Code {
    fn session(&self, locator: &str, _first_line: Option<&[u8]>) -> Option<Identity> {
        let thread = locator.strip_prefix("threads/")?.strip_suffix(".json")?;
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

    fn line(&self, _session: &Identity, _line: &[u8]) -> Line {
        Line::default()
    }

    fn version(&self, _session: &Identity, bytes: &[u8]) -> Version {
        let Ok(snapshot) = serde_json::from_slice::<Snapshot>(bytes) else {
            return Version::default();
        };
        let thread = &snapshot.thread;
        let base = bytes.as_ptr() as usize;
        let messages = snapshot
            .messages
            .iter()
            .filter_map(|raw| {
                let row: MessageRow = serde_json::from_str(raw.get()).ok()?;
                let start = (raw.get().as_ptr() as usize - base) as u64;
                Some((message(row), start..start + raw.get().len() as u64))
            })
            .collect();
        Version {
            facts: Facts {
                title: thread.title.clone().filter(|title| !title.is_empty()),
                project: snapshot.project.and_then(|project| project.workspace_root),
                started: timestamp(&thread.created_at),
                ..Facts::default()
            },
            lifecycle: Some(Lifecycle {
                archived_at: timestamp(&thread.archived_at),
                deleted_at: timestamp(&thread.deleted_at),
            }),
            messages,
            artifacts: snapshot
                .provider_threads
                .iter()
                .filter_map(underlying_session)
                .collect(),
        }
    }
}

/// The shape `collect::t3_code` writes. Each row keeps T3 Code's column names.
#[derive(Deserialize)]
struct Snapshot<'a> {
    thread: ThreadRow,
    project: Option<ProjectRow>,
    provider_threads: Vec<ProviderThreadRow>,
    #[serde(borrow)]
    messages: Vec<&'a RawValue>,
}

#[derive(Deserialize)]
struct ThreadRow {
    title: Option<String>,
    created_at: Option<String>,
    archived_at: Option<String>,
    deleted_at: Option<String>,
}

#[derive(Deserialize)]
struct ProjectRow {
    workspace_root: Option<String>,
}

#[derive(Deserialize)]
struct ProviderThreadRow {
    provider: Option<String>,
    payload_json: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProviderThreadPayload {
    native_thread_ref: Option<NativeThreadRef>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NativeThreadRef {
    driver: Option<String>,
    native_id: Option<String>,
}

#[derive(Deserialize)]
struct MessageRow {
    message_id: String,
    role: String,
    created_at: Option<String>,
    payload_json: String,
}

#[derive(Deserialize)]
struct MessagePayload {
    text: Option<String>,
}

fn message(row: MessageRow) -> Message {
    let text = serde_json::from_str::<MessagePayload>(&row.payload_json)
        .ok()
        .and_then(|payload| payload.text)
        .unwrap_or_default();
    Message {
        id: row.message_id,
        parent_id: None,
        role: match row.role.as_str() {
            "user" => Role::User,
            "assistant" => Role::Assistant,
            _ => Role::System,
        },
        at: timestamp(&row.created_at),
        searchable_text: redact::secrets(&text).into_string(),
        artifacts: Vec::new(),
        media: Vec::new(),
    }
}

/// The session key of the harness session that ran a provider thread, such as `claude-code/<session-id>`.
fn underlying_session(row: &ProviderThreadRow) -> Option<Artifact> {
    let payload: ProviderThreadPayload = serde_json::from_str(row.payload_json.as_deref()?).ok()?;
    let native = payload.native_thread_ref?;
    let driver = native.driver.or_else(|| row.provider.clone())?;
    let source = DRIVERS
        .iter()
        .find(|(name, _)| *name == driver)
        .map_or(driver.as_str(), |(_, source)| source);
    Some(Artifact {
        kind: ArtifactKind::UnderlyingSession,
        value: format!("{source}/{}", native.native_id?),
    })
}

fn timestamp(text: &Option<String>) -> Option<Timestamp> {
    text.as_deref().and_then(Timestamp::parse_utc)
}
