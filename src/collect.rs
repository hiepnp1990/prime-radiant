pub mod claude_code;
pub mod codex;
pub mod t3_code;

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::Result;
use crate::raw::{
    Applied, HeadCheck, Progress, Rejection, Sha256, Source, Store, StreamKey, StreamState, Write,
    WriteMode,
};

pub const MAX_SEGMENT_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalFile {
    pub locator: String,
    pub path: PathBuf,
    pub write_mode: WriteMode,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ImportSummary {
    pub files_scanned: usize,
    /// Files deleted between the scan and the read. Claude Code deletes old transcripts.
    pub files_vanished: usize,
    pub writes_planned: usize,
    pub bytes_appended: u64,
    pub versions_stored: usize,
    pub generations_started: usize,
    pub rejected: Vec<(StreamKey, Rejection)>,
}

pub fn plan(
    origin: &str,
    source: Source,
    file: &LocalFile,
    states: &HashMap<StreamKey, StreamState>,
) -> io::Result<Vec<Write>> {
    let mut writes = Vec::new();
    let key = StreamKey {
        origin: origin.to_owned(),
        source,
        locator: file.locator.clone(),
    };
    let state = states.get(&key);
    match file.write_mode {
        WriteMode::Append => plan_append(key, &file.path, state, &mut writes)?,
        WriteMode::Replace => plan_replace(key, &file.path, state, &mut writes)?,
    }
    Ok(writes)
}

fn plan_append(
    key: StreamKey,
    path: &Path,
    state: Option<&StreamState>,
    writes: &mut Vec<Write>,
) -> io::Result<()> {
    let mut file = File::open(path)?;
    let (generation, mut start) = match state {
        Some(StreamState {
            generation,
            progress:
                Progress::Append {
                    confirmed_offset,
                    head,
                },
        }) => {
            if continues(&mut file, *confirmed_offset, head)? {
                (*generation, *confirmed_offset)
            } else {
                writes.push(Write::NewGeneration {
                    key: key.clone(),
                    generation: generation + 1,
                });
                (generation + 1, 0)
            }
        }
        _ => (0, 0),
    };
    file.seek(SeekFrom::Start(start))?;
    loop {
        let mut bytes = Vec::new();
        (&mut file)
            .take(MAX_SEGMENT_BYTES)
            .read_to_end(&mut bytes)?;
        if bytes.is_empty() {
            return Ok(());
        }
        let len = bytes.len() as u64;
        writes.push(Write::Segment {
            key: key.clone(),
            generation,
            start,
            sha256: Sha256::of(&bytes),
            bytes,
        });
        start += len;
    }
}

fn continues(file: &mut File, confirmed_offset: u64, head: &HeadCheck) -> io::Result<bool> {
    if file.metadata()?.len() < confirmed_offset {
        return Ok(false);
    }
    let mut prefix = Vec::new();
    file.take(head.len).read_to_end(&mut prefix)?;
    Ok(HeadCheck::of(&prefix) == *head)
}

fn plan_replace(
    key: StreamKey,
    path: &Path,
    state: Option<&StreamState>,
    writes: &mut Vec<Write>,
) -> io::Result<()> {
    writes.extend(plan_version(key, fs::read(path)?, state));
    Ok(())
}

/// A version of a replace stream, unless the raw layer already has these bytes as its latest.
fn plan_version(key: StreamKey, bytes: Vec<u8>, state: Option<&StreamState>) -> Option<Write> {
    let sha256 = Sha256::of(&bytes);
    let unchanged = matches!(
        state,
        Some(StreamState { progress: Progress::Replace { latest_checksum }, .. }) if *latest_checksum == sha256
    );
    (!unchanged).then_some(Write::Version { key, sha256, bytes })
}

pub fn import_claude_code(root: &Path, origin: &str, store: &mut Store) -> Result<ImportSummary> {
    import(&claude_code::scan(root)?, origin, Source::ClaudeCode, store)
}

pub fn import_codex(root: &Path, origin: &str, store: &mut Store) -> Result<ImportSummary> {
    import(&codex::scan(root)?, origin, Source::Codex, store)
}

/// Imports each thread of a T3 Code database as one replace stream. `files_scanned` counts threads.
pub fn import_t3_code(db_path: &Path, origin: &str, store: &mut Store) -> Result<ImportSummary> {
    let threads = t3_code::snapshots(db_path)?;
    let states = store.state(origin)?;
    let mut summary = ImportSummary {
        files_scanned: threads.len(),
        ..ImportSummary::default()
    };
    for thread in threads {
        let key = StreamKey {
            origin: origin.to_owned(),
            source: Source::T3Code,
            locator: thread.locator,
        };
        let state = states.get(&key);
        if let Some(write) = plan_version(key, thread.bytes, state) {
            apply(store, &write, &mut summary)?;
        }
    }
    Ok(summary)
}

/// Imports the files a scan found. A file deleted since the scan is skipped and counted.
pub fn import(
    files: &[LocalFile],
    origin: &str,
    source: Source,
    store: &mut Store,
) -> Result<ImportSummary> {
    let states = store.state(origin)?;
    let mut summary = ImportSummary {
        files_scanned: files.len(),
        ..ImportSummary::default()
    };
    for file in files {
        let writes = match plan(origin, source, file, &states) {
            Ok(writes) => writes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                summary.files_vanished += 1;
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        for write in writes {
            apply(store, &write, &mut summary)?;
        }
    }
    Ok(summary)
}

fn apply(store: &mut Store, write: &Write, summary: &mut ImportSummary) -> Result<()> {
    summary.writes_planned += 1;
    match store.apply(write)? {
        Applied::Appended { .. } => {
            if let Write::Segment { bytes, .. } = write {
                summary.bytes_appended += bytes.len() as u64;
            }
        }
        Applied::VersionStored => summary.versions_stored += 1,
        Applied::NewGenerationStarted { .. } => summary.generations_started += 1,
        Applied::Rejected(rejection) => summary.rejected.push((write.key().clone(), rejection)),
        Applied::AlreadyHad | Applied::VersionUnchanged => {}
    }
    Ok(())
}
