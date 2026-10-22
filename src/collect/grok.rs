use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::LocalFile;
use crate::raw::WriteMode;

/// The files of a Grok session folder that hold the session. Grok's temporary, lock, prompt, and
/// tool definition files are left out, along with everything else it keeps there.
const SESSION_FILES: [(&str, WriteMode); 5] = [
    ("chat_history.jsonl", WriteMode::Append),
    ("events.jsonl", WriteMode::Append),
    ("updates.jsonl", WriteMode::Append),
    ("summary.json", WriteMode::Replace),
    ("usage.json", WriteMode::Replace),
];

/// Finds the session files under a Grok home folder, `sessions/<working folder>/<session>/<file>`, sorted by locator.
pub fn scan(root: &Path) -> io::Result<Vec<LocalFile>> {
    let mut files = Vec::new();
    for (folder, folder_path) in folders(&root.join("sessions"))? {
        for (session, session_path) in folders(&folder_path)? {
            for (name, write_mode) in SESSION_FILES {
                let path = session_path.join(name);
                if path.is_file() {
                    files.push(LocalFile {
                        locator: format!("sessions/{folder}/{session}/{name}"),
                        path,
                        write_mode,
                    });
                }
            }
        }
    }
    files.sort_by(|a, b| a.locator.cmp(&b.locator));
    Ok(files)
}

/// The folders in `dir` with UTF-8 names, or none when `dir` is missing.
fn folders(dir: &Path) -> io::Result<Vec<(String, PathBuf)>> {
    let entries = match fs::read_dir(dir) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        entries => entries?,
    };
    let mut folders = Vec::new();
    for entry in entries {
        let entry = entry?;
        let is_dir = match entry.file_type() {
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            kind => kind?.is_dir(),
        };
        if let (true, Ok(name)) = (is_dir, entry.file_name().into_string()) {
            folders.push((name, entry.path()));
        }
    }
    Ok(folders)
}
