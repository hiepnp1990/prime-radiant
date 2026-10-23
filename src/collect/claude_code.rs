use super::Layout;
use crate::raw::{Source, WriteMode};

/// A Claude Code projects folder keeps transcripts anywhere under it, with rewritten files beside them.
pub const LAYOUT: Layout = Layout {
    source: Source::ClaudeCode,
    folders: &[""],
    files: &[],
    write_mode,
};

fn write_mode(locator: &str) -> Option<WriteMode> {
    let (dirs, name) = locator.rsplit_once('/').unwrap_or(("", locator));
    if dirs.split('/').any(|dir| dir == "tool-results") {
        Some(WriteMode::Replace)
    } else if name.ends_with(".jsonl") {
        Some(WriteMode::Append)
    } else if name.ends_with(".meta.json") || name == "custom-title.json" {
        Some(WriteMode::Replace)
    } else {
        None
    }
}
