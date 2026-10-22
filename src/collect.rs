pub mod claude_code;
pub mod codex;
pub mod grok;
pub mod live;
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

/// Where a source that keeps its sessions in files keeps them under its root, and which of them the raw layer keeps.
#[derive(Debug)]
pub struct Layout {
    pub source: Source,
    /// The folders under the root that hold the files. An empty name is the root itself.
    folders: &'static [&'static str],
    /// How the raw layer keeps the file at a locator, or `None` when it leaves the file out.
    write_mode: fn(&str) -> Option<WriteMode>,
}

/// The layout of a source, or `None` for T3 Code, which keeps its sessions in a database.
pub fn layout(source: Source) -> Option<&'static Layout> {
    match source {
        Source::ClaudeCode => Some(&claude_code::LAYOUT),
        Source::Codex => Some(&codex::LAYOUT),
        Source::Grok => Some(&grok::LAYOUT),
        Source::T3Code => None,
    }
}

impl Layout {
    /// Every file under `root` that the raw layer keeps, sorted by locator.
    pub fn scan(&self, root: &Path) -> io::Result<Vec<LocalFile>> {
        let mut files = Vec::new();
        for folder in self.folders(root) {
            self.visit(root, &folder, &mut files)?;
        }
        files.sort_by(|a, b| a.locator.cmp(&b.locator));
        Ok(files)
    }

    /// Like `scan`, but only the files in `dir`, a folder under `root`. A folder that vanished has none.
    pub fn scan_under(&self, root: &Path, dir: &Path) -> io::Result<Vec<LocalFile>> {
        let mut files = Vec::new();
        self.visit(root, dir, &mut files)?;
        files.sort_by(|a, b| a.locator.cmp(&b.locator));
        Ok(files)
    }

    /// The file at `path` as a scan of `root` would list it, or `None` if a scan would skip it.
    pub fn classify(&self, root: &Path, path: &Path) -> Option<LocalFile> {
        if !fs::symlink_metadata(path).ok()?.is_file() {
            return None;
        }
        self.local_file(root, path)
    }

    /// The folders under `root` that hold the source's files.
    pub fn folders(&self, root: &Path) -> Vec<PathBuf> {
        self.folders
            .iter()
            .map(|folder| match *folder {
                "" => root.to_owned(),
                folder => root.join(folder),
            })
            .collect()
    }

    /// Whether `path` is one of the folders that hold the source's files, or inside one.
    pub fn holds(&self, root: &Path, path: &Path) -> bool {
        locator(root, path).is_some_and(|locator| self.within(&locator))
    }

    fn within(&self, locator: &str) -> bool {
        self.folders.iter().any(|folder| {
            folder.is_empty()
                || locator
                    .strip_prefix(folder)
                    .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
        })
    }

    fn visit(&self, root: &Path, dir: &Path, files: &mut Vec<LocalFile>) -> io::Result<()> {
        let entries = match fs::read_dir(dir) {
            Err(error) if error.kind() == io::ErrorKind::NotFound && dir != root => return Ok(()),
            entries => entries?,
        };
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            let kind = match entry.file_type() {
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                kind => kind?,
            };
            if kind.is_dir() {
                self.visit(root, &path, files)?;
            } else if kind.is_file()
                && let Some(file) = self.local_file(root, &path)
            {
                files.push(file);
            }
        }
        Ok(())
    }

    fn local_file(&self, root: &Path, path: &Path) -> Option<LocalFile> {
        let locator = locator(root, path).filter(|locator| self.within(locator))?;
        let write_mode = (self.write_mode)(&locator)?;
        Some(LocalFile {
            locator,
            path: path.to_owned(),
            write_mode,
        })
    }
}

/// The path relative to `root` with `/` separators, or `None` if a part of it is not UTF-8.
fn locator(root: &Path, path: &Path) -> Option<String> {
    let parts = path
        .strip_prefix(root)
        .ok()?
        .components()
        .map(|part| part.as_os_str().to_str())
        .collect::<Option<Vec<_>>>()?;
    Some(parts.join("/"))
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
    let key = StreamKey {
        origin: origin.to_owned(),
        source,
        locator: file.locator.clone(),
    };
    let state = states.get(&key);
    if file.write_mode == WriteMode::Replace {
        return Ok(replace_write(key, &file.path, state)?.into_iter().collect());
    }
    let mut writes = Vec::new();
    let mut handle = File::open(&file.path)?;
    let (generation, mut start) = match resume(&mut handle, state)? {
        Resume::At { generation, offset } => (generation, offset),
        Resume::NewGeneration { generation } => {
            writes.push(Write::NewGeneration {
                key: key.clone(),
                generation,
            });
            (generation, 0)
        }
    };
    while let Some(write) = segment(&key, &mut handle, generation, start)? {
        if let Write::Segment { bytes, .. } = &write {
            start += bytes.len() as u64;
        }
        writes.push(write);
    }
    Ok(writes)
}

/// The one write that moves the stream's copy one step closer to the file, or `None` when the copy matches.
/// `state` is the stream's confirmed state, so each call starts from what the store has, not from what was sent.
pub fn next_write(
    key: &StreamKey,
    file: &LocalFile,
    state: Option<&StreamState>,
) -> io::Result<Option<Write>> {
    if file.write_mode == WriteMode::Replace {
        return replace_write(key.clone(), &file.path, state);
    }
    let mut handle = File::open(&file.path)?;
    match resume(&mut handle, state)? {
        Resume::At { generation, offset } => segment(key, &mut handle, generation, offset),
        Resume::NewGeneration { generation } => Ok(Some(Write::NewGeneration {
            key: key.clone(),
            generation,
        })),
    }
}

/// Where an append stream's next bytes go.
enum Resume {
    At {
        generation: u64,
        offset: u64,
    },
    /// The file no longer starts with the confirmed bytes, so they start over in a new generation.
    NewGeneration {
        generation: u64,
    },
}

fn resume(file: &mut File, state: Option<&StreamState>) -> io::Result<Resume> {
    Ok(match state {
        Some(StreamState {
            generation,
            progress:
                Progress::Append {
                    confirmed_offset,
                    head,
                },
        }) => {
            if continues(file, *confirmed_offset, head)? {
                Resume::At {
                    generation: *generation,
                    offset: *confirmed_offset,
                }
            } else {
                Resume::NewGeneration {
                    generation: generation + 1,
                }
            }
        }
        _ => Resume::At {
            generation: 0,
            offset: 0,
        },
    })
}

/// Up to `MAX_SEGMENT_BYTES` from `start`, or `None` at the end of the file.
fn segment(
    key: &StreamKey,
    file: &mut File,
    generation: u64,
    start: u64,
) -> io::Result<Option<Write>> {
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.take(MAX_SEGMENT_BYTES).read_to_end(&mut bytes)?;
    Ok((!bytes.is_empty()).then(|| Write::Segment {
        key: key.clone(),
        generation,
        start,
        sha256: Sha256::of(&bytes),
        bytes,
    }))
}

fn continues(file: &mut File, confirmed_offset: u64, head: &HeadCheck) -> io::Result<bool> {
    if file.metadata()?.len() < confirmed_offset {
        return Ok(false);
    }
    let mut prefix = Vec::new();
    file.take(head.len).read_to_end(&mut prefix)?;
    Ok(HeadCheck::of(&prefix) == *head)
}

fn replace_write(
    key: StreamKey,
    path: &Path,
    state: Option<&StreamState>,
) -> io::Result<Option<Write>> {
    Ok(plan_version(key, fs::read(path)?, state))
}

/// A version of a replace stream, unless the raw layer already has these bytes as its latest.
pub fn plan_version(key: StreamKey, bytes: Vec<u8>, state: Option<&StreamState>) -> Option<Write> {
    let sha256 = Sha256::of(&bytes);
    let unchanged = matches!(
        state,
        Some(StreamState { progress: Progress::Replace { latest_checksum }, .. }) if *latest_checksum == sha256
    );
    (!unchanged).then_some(Write::Version { key, sha256, bytes })
}

pub fn import_claude_code(root: &Path, origin: &str, store: &mut Store) -> Result<ImportSummary> {
    import(
        &claude_code::LAYOUT.scan(root)?,
        origin,
        Source::ClaudeCode,
        store,
    )
}

pub fn import_codex(root: &Path, origin: &str, store: &mut Store) -> Result<ImportSummary> {
    import(&codex::LAYOUT.scan(root)?, origin, Source::Codex, store)
}

pub fn import_grok(root: &Path, origin: &str, store: &mut Store) -> Result<ImportSummary> {
    import(&grok::LAYOUT.scan(root)?, origin, Source::Grok, store)
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
