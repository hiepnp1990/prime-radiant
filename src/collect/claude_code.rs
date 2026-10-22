use std::fs;
use std::io;
use std::path::Path;

use super::LocalFile;
use crate::raw::WriteMode;

/// Finds every file under a Claude Code projects root that the raw layer keeps, sorted by locator.
pub fn scan(root: &Path) -> io::Result<Vec<LocalFile>> {
    let mut files = Vec::new();
    visit(root, root, &mut files)?;
    files.sort_by(|a, b| a.locator.cmp(&b.locator));
    Ok(files)
}

fn visit(root: &Path, dir: &Path, files: &mut Vec<LocalFile>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let kind = entry.file_type()?;
        if kind.is_dir() {
            visit(root, &path, files)?;
            continue;
        }
        if !kind.is_file() {
            continue;
        }
        let Some(locator) = locator(root, &path) else {
            continue;
        };
        if let Some(write_mode) = write_mode(&locator) {
            files.push(LocalFile {
                locator,
                path,
                write_mode,
            });
        }
    }
    Ok(())
}

fn locator(root: &Path, path: &Path) -> Option<String> {
    let parts = path
        .strip_prefix(root)
        .ok()?
        .components()
        .map(|part| part.as_os_str().to_str())
        .collect::<Option<Vec<_>>>()?;
    Some(parts.join("/"))
}

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
