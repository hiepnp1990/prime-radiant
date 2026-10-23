//! The harnesses Prime Radiant reads, where each keeps its sessions and its skills under the home folder, and which
//! of them this machine has.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::Result;
use crate::collect::t3_code;
use crate::raw::Source;

pub struct Harness {
    pub source: Source,
    /// The folder, or T3 Code's database, that `collect` reads, relative to the home folder.
    sessions: &'static str,
    /// The folder the harness reads skills from, relative to the home folder, if it reads skills.
    skills: Option<&'static str>,
    /// Counts the sessions at `sessions`, cheaply enough to run on every `collect` start and `doctor`.
    count: fn(&Path) -> Result<u64>,
}

pub const HARNESSES: [Harness; 4] = [
    Harness {
        source: Source::ClaudeCode,
        sessions: ".claude/projects",
        skills: Some(".claude/skills"),
        count: claude_code_sessions,
    },
    Harness {
        source: Source::Codex,
        sessions: ".codex",
        skills: Some(".codex/skills"),
        count: codex_sessions,
    },
    Harness {
        source: Source::Grok,
        sessions: ".grok",
        skills: Some(".grok/skills"),
        count: grok_sessions,
    },
    // T3 Code runs Codex or Claude Code, which read their own skills folders.
    Harness {
        source: Source::T3Code,
        sessions: ".t3/userdata/statev2.sqlite",
        skills: None,
        count: t3_code_threads,
    },
];

impl Harness {
    pub fn of(source: Source) -> &'static Harness {
        HARNESSES
            .iter()
            .find(|harness| harness.source == source)
            .expect("every source is a harness")
    }

    pub fn sessions(&self, home: &Path) -> PathBuf {
        home.join(self.sessions)
    }

    pub fn skills(&self, home: &Path) -> Option<PathBuf> {
        self.skills.map(|folder| home.join(folder))
    }

    pub fn reads_skills(&self) -> bool {
        self.skills.is_some()
    }
}

/// A harness this machine has.
pub struct Detected {
    pub source: Source,
    pub path: PathBuf,
    /// Its sessions, or why they could not be counted.
    pub sessions: Result<u64>,
}

/// Every harness whose sessions are where it keeps them under `home`, in the order of [`HARNESSES`].
pub fn detect(home: &Path) -> Vec<Detected> {
    HARNESSES
        .iter()
        .filter_map(|harness| {
            let path = harness.sessions(home);
            path.exists().then(|| Detected {
                source: harness.source,
                sessions: (harness.count)(&path),
                path,
            })
        })
        .collect()
}

/// The places [`detect`] looks, for a message that says where nothing was found.
pub fn looked_in(home: &Path) -> String {
    let paths: Vec<String> = HARNESSES
        .iter()
        .map(|harness| harness.sessions(home).display().to_string())
        .collect();
    paths.join(", ")
}

/// Each `<session>.jsonl` directly in a project folder. Subagents are in folders below and are not counted.
fn claude_code_sessions(projects: &Path) -> Result<u64> {
    let mut sessions = 0;
    for project in fs::read_dir(projects)? {
        let project = project?.path();
        if project.is_dir() {
            sessions += files(&project, &|name| name.ends_with(".jsonl"), false)?;
        }
    }
    Ok(sessions)
}

/// Each rollout file, active or archived.
fn codex_sessions(home: &Path) -> Result<u64> {
    let rollout = |name: &str| name.starts_with("rollout-");
    Ok(files(&home.join("sessions"), &rollout, true)?
        + files(&home.join("archived_sessions"), &rollout, true)?)
}

/// Each session folder, `sessions/<project>/<id>`.
fn grok_sessions(home: &Path) -> Result<u64> {
    let mut sessions = 0;
    for project in entries(&home.join("sessions"))? {
        if project.is_dir() {
            sessions += entries(&project)?.iter().filter(|id| id.is_dir()).count() as u64;
        }
    }
    Ok(sessions)
}

fn t3_code_threads(db: &Path) -> Result<u64> {
    t3_code::thread_count(db)
}

/// The files in `dir` whose names pass `keep`, and with `deep`, in every folder below it. A missing folder has none.
fn files(dir: &Path, keep: &dyn Fn(&str) -> bool, deep: bool) -> io::Result<u64> {
    let mut found = 0;
    for path in entries(dir)? {
        if path.is_dir() {
            if deep {
                found += files(&path, keep, deep)?;
            }
        } else if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(keep)
        {
            found += 1;
        }
    }
    Ok(found)
}

fn entries(dir: &Path) -> io::Result<Vec<PathBuf>> {
    match fs::read_dir(dir) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        entries => entries?.map(|entry| Ok(entry?.path())).collect(),
    }
}
