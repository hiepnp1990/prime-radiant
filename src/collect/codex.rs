use super::Layout;
use crate::raw::{Source, WriteMode};

/// A Codex home folder keeps rollouts under `sessions` and `archived_sessions`, and appends thread names to
/// `session_index.jsonl`.
///
/// A running thread grows `rollout-*.jsonl`. When it finishes, Codex writes `rollout-*.jsonl.zst` once and removes the live file.
pub const LAYOUT: Layout = Layout {
    source: Source::Codex,
    folders: &["sessions", "archived_sessions"],
    files: &[("session_index.jsonl", WriteMode::Append)],
    write_mode,
};

fn write_mode(locator: &str) -> Option<WriteMode> {
    let name = locator.rsplit_once('/').map_or(locator, |(_, name)| name);
    if !name.starts_with("rollout-") {
        None
    } else if name.ends_with(".jsonl") {
        Some(WriteMode::Append)
    } else if name.ends_with(".jsonl.zst") {
        Some(WriteMode::Replace)
    } else {
        None
    }
}
