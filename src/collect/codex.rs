use std::fs;
use std::io;
use std::path::Path;

use super::LocalFile;
use crate::raw::WriteMode;

const ROLLOUT_DIRS: [&str; 2] = ["sessions", "archived_sessions"];

/// Finds every rollout under a Codex home folder, sorted by locator.
///
/// A running thread grows `rollout-*.jsonl`. When it finishes, Codex writes `rollout-*.jsonl.zst` once and removes the live file.
pub fn scan(root: &Path) -> io::Result<Vec<LocalFile>> {
    let mut files = Vec::new();
    for dir in ROLLOUT_DIRS {
        match visit(&root.join(dir), dir, &mut files) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            other => other?,
        }
    }
    files.sort_by(|a, b| a.locator.cmp(&b.locator));
    Ok(files)
}

fn visit(dir: &Path, locator: &str, files: &mut Vec<LocalFile>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let path = entry.path();
        let child = format!("{locator}/{name}");
        let kind = entry.file_type()?;
        if kind.is_dir() {
            visit(&path, &child, files)?;
        } else if kind.is_file()
            && let Some(write_mode) = write_mode(&name)
        {
            files.push(LocalFile {
                locator: child,
                path,
                write_mode,
            });
        }
    }
    Ok(())
}

fn write_mode(name: &str) -> Option<WriteMode> {
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
