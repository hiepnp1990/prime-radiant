use super::Layout;
use crate::raw::{Source, WriteMode};

/// The files of a Grok session folder that hold the session. Grok's temporary, lock, prompt, and
/// tool definition files are left out, along with everything else it keeps there.
const SESSION_FILES: [(&str, WriteMode); 5] = [
    ("chat_history.jsonl", WriteMode::Append),
    ("events.jsonl", WriteMode::Append),
    ("updates.jsonl", WriteMode::Append),
    ("summary.json", WriteMode::Replace),
    ("usage.json", WriteMode::Replace),
];

/// A Grok home folder keeps each session's files at `sessions/<working folder>/<session>/<file>`.
pub const LAYOUT: Layout = Layout {
    source: Source::Grok,
    folders: &["sessions"],
    files: &[],
    write_mode,
};

fn write_mode(locator: &str) -> Option<WriteMode> {
    let ["sessions", _, _, name] = locator.split('/').collect::<Vec<_>>()[..] else {
        return None;
    };
    SESSION_FILES
        .iter()
        .find(|(file, _)| *file == name)
        .map(|(_, write_mode)| *write_mode)
}
