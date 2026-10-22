use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::{Parser, Subcommand};
use prime_radiant::collect::{import_claude_code, import_codex, import_t3_code};
use prime_radiant::index::{Indexer, Timestamp};
use prime_radiant::query::{Brief, Hit, Query, Stats};
use prime_radiant::raw::{Source, Store};
use serde::Serialize;

const WIDTH: usize = 100;

#[derive(Parser)]
#[command(
    name = "prime-radiant",
    version,
    about = "Find and read any past AI session from one always-on hub.",
    arg_required_else_help = true
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Import Claude Code, Codex, and T3 Code sessions into the raw layer, then index them.
    #[command(group(clap::ArgGroup::new("sources").required(true).multiple(true)))]
    Import {
        #[arg(long)]
        data: PathBuf,
        /// The machine or account the files come from.
        #[arg(long)]
        origin: String,
        /// A Claude Code projects folder, such as ~/.claude/projects.
        #[arg(long = "claude-code", group = "sources")]
        claude_code: Vec<PathBuf>,
        /// A Codex home folder, such as ~/.codex.
        #[arg(long, group = "sources")]
        codex: Vec<PathBuf>,
        /// A T3 Code database, such as ~/.t3/userdata/statev2.sqlite. It is only read.
        #[arg(long = "t3-code", value_name = "DATABASE", group = "sources")]
        t3_code: Vec<PathBuf>,
    },
    /// Count what the store holds, and list every stream and session.
    Stats {
        #[arg(long)]
        data: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Find sessions by ID, alias, ID prefix, or words about the topic.
    Find {
        #[arg(required = true, num_args = 1..)]
        query: Vec<String>,
        #[arg(long)]
        data: PathBuf,
        #[arg(long)]
        json: bool,
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// Summarize one session: its first request, last answer, artifacts, and subagents.
    Brief {
        /// A session key, an alias, or a unique ID prefix of 8 or more characters.
        session: String,
        #[arg(long)]
        data: PathBuf,
        #[arg(long)]
        json: bool,
    },
}

fn main() -> ExitCode {
    match run(Cli::parse().command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(command: Command) -> prime_radiant::Result<()> {
    match command {
        Command::Import {
            data,
            origin,
            claude_code,
            codex,
            t3_code,
        } => {
            let sources = claude_code
                .into_iter()
                .map(|path| (Source::ClaudeCode, path))
                .chain(codex.into_iter().map(|path| (Source::Codex, path)))
                .chain(t3_code.into_iter().map(|path| (Source::T3Code, path)));
            import(&data, &origin, sources)
        }
        Command::Stats { data, json } => {
            let stats = Query::open(&data, now())?.stats()?;
            if json {
                print_json(&stats);
            } else {
                print!("{}", stats_text(&stats));
            }
            Ok(())
        }
        Command::Find {
            query,
            data,
            json,
            limit,
        } => {
            let hits = Query::open(&data, now())?.find(&query.join(" "), limit)?;
            if json {
                print_json(&hits);
            } else {
                print!("{}", hits_text(&hits));
            }
            Ok(())
        }
        Command::Brief {
            session,
            data,
            json,
        } => {
            let brief = Query::open(&data, now())?.brief(&session)?;
            if json {
                print_json(&brief);
            } else {
                print!("{}", brief_text(&brief));
            }
            Ok(())
        }
    }
}

fn import(
    data: &Path,
    origin: &str,
    sources: impl Iterator<Item = (Source, PathBuf)>,
) -> prime_radiant::Result<()> {
    let mut store = Store::open(data)?;
    for (source, path) in sources {
        let (summary, unit) = match source {
            Source::ClaudeCode => (import_claude_code(&path, origin, &mut store)?, "files"),
            Source::Codex => (import_codex(&path, origin, &mut store)?, "files"),
            Source::T3Code => (import_t3_code(&path, origin, &mut store)?, "threads"),
        };
        println!(
            "{}: scanned {} {unit}: {} vanished before they were read, {} bytes appended, {} versions stored, {} generations started, {} writes rejected",
            source.as_str(),
            summary.files_scanned,
            summary.files_vanished,
            summary.bytes_appended,
            summary.versions_stored,
            summary.generations_started,
            summary.rejected.len(),
        );
        for (key, rejection) in summary.rejected.iter().take(5) {
            eprintln!("rejected {}: {rejection:?}", key.locator);
        }
    }
    let synced = Indexer::open(data)?.sync()?;
    println!(
        "indexed {} messages from {} streams",
        synced.messages_indexed, synced.streams_synced
    );
    Ok(())
}

fn now() -> Timestamp {
    Timestamp(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_millis() as i64),
    )
}

fn print_json(value: &impl Serialize) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).expect("output serializes")
    );
}

fn stats_text(stats: &Stats) -> String {
    let c = &stats.counts;
    format!(
        "raw: {} streams, {} segments, {} versions\nindex: {} sessions, {} messages, {} aliases, {} media, {} artifacts\n",
        c.streams, c.segments, c.versions, c.sessions, c.messages, c.aliases, c.media, c.artifacts
    )
}

fn hits_text(hits: &[Hit]) -> String {
    if hits.is_empty() {
        return "no sessions found\n".to_owned();
    }
    let mut out = String::new();
    for (i, hit) in hits.iter().enumerate() {
        let title = hit.title.as_deref().unwrap_or("(untitled)");
        out += &wrap(&format!("{}. {title} ({})", i + 1, hit.state.as_str()), "");
        out += &wrap(&hit.key, "   ");
        if let Some(project) = &hit.project {
            out += &wrap(project, "   ");
        }
        out += &wrap(
            &format!(
                "ran on {}, last activity {}",
                hit.ran_on,
                optional_time(hit.last_activity)
            ),
            "   ",
        );
        if let Some(snippet) = &hit.snippet {
            out += &wrap(snippet, "   ");
        }
    }
    out
}

fn brief_text(brief: &Brief) -> String {
    let mut out = String::new();
    let mut line = |text: &str| out += &wrap(text, "");
    line(brief.title.as_deref().unwrap_or("(untitled)"));
    line(&format!("key: {}", brief.key));
    if let Some(parent) = &brief.parent {
        line(&format!("subagent of: {parent}"));
    }
    line(&format!(
        "project: {}",
        brief.project.as_deref().unwrap_or("none")
    ));
    line(&format!("origin: {}", brief.origin));
    line(&format!(
        "started {}, last activity {}, {}",
        optional_time(brief.started),
        optional_time(brief.last_activity),
        brief.state.as_str()
    ));
    for (label, at) in [
        ("archived", brief.archived_at),
        ("deleted", brief.deleted_at),
    ] {
        if let Some(at) = at {
            line(&format!("{label} {at}"));
        }
    }
    let roles: Vec<String> = brief
        .messages
        .iter()
        .filter(|(role, _)| *role != "total")
        .map(|(role, n)| format!("{role} {n}"))
        .collect();
    line(&format!(
        "messages: {} ({})",
        brief.messages.get("total").copied().unwrap_or(0),
        roles.join(", ")
    ));
    let f = &brief.freshness;
    line(&format!(
        "brief: covers messages through position {}, built {}{}",
        f.built_through.map_or("none".to_owned(), |p| p.to_string()),
        f.built_at,
        if f.rebuilt {
            " (rebuilt now)"
        } else {
            " (no newer messages since)"
        }
    ));
    let body = &brief.body;
    for (label, text) in [
        ("first request", &body.first_request),
        ("last answer", &body.last_answer),
    ] {
        out += &format!("\n{label}:\n");
        out += &wrap(text.as_deref().unwrap_or("(none)"), "  ");
    }
    if !body.pull_requests.is_empty() {
        out += "\npull requests:\n";
        for url in &body.pull_requests {
            out += &wrap(url, "  ");
        }
    }
    if body.changed_files_total > 0 {
        out += &format!("\nchanged files ({}):\n", body.changed_files_total);
        for path in &body.changed_files {
            out += &wrap(relative(path, brief.project.as_deref()), "  ");
        }
        if body.changed_files_total > body.changed_files.len() {
            out += &format!(
                "  and {} more\n",
                body.changed_files_total - body.changed_files.len()
            );
        }
    }
    for (label, keys) in [
        ("underlying sessions", &brief.underlying_sessions),
        ("front-end sessions", &brief.front_end_sessions),
    ] {
        if !keys.is_empty() {
            out += &format!("\n{label}:\n");
            for key in keys {
                out += &wrap(key, "  ");
            }
        }
    }
    if !body.subagents.is_empty() {
        out += &format!("\nsubagents ({}):\n", body.subagents.len());
        for agent in &body.subagents {
            out += &wrap(
                &format!(
                    "{} ({} messages)",
                    agent.title.as_deref().unwrap_or("(untitled)"),
                    agent.message_count
                ),
                "  ",
            );
            out += &wrap(&agent.key, "    ");
        }
    }
    out
}

/// A path inside the project, relative to it.
fn relative<'a>(path: &'a str, project: Option<&str>) -> &'a str {
    project
        .and_then(|project| path.strip_prefix(project))
        .and_then(|rest| rest.strip_prefix('/'))
        .unwrap_or(path)
}

fn optional_time(at: Option<Timestamp>) -> String {
    at.map_or("unknown".to_owned(), |at| at.to_string())
}

/// Wraps text at word boundaries to the width, prefixing each line with `indent`.
fn wrap(text: &str, indent: &str) -> String {
    let room = WIDTH - indent.len();
    let mut out = String::new();
    for paragraph in text.lines() {
        let mut line = String::new();
        for word in paragraph.split_whitespace() {
            for piece in chunks(word, room) {
                if !line.is_empty() && line.chars().count() + 1 + piece.chars().count() > room {
                    out += &format!("{indent}{line}\n");
                    line.clear();
                }
                if !line.is_empty() {
                    line.push(' ');
                }
                line += piece;
            }
        }
        if !line.is_empty() {
            out += &format!("{indent}{line}\n");
        }
    }
    out
}

fn chunks(word: &str, size: usize) -> Vec<&str> {
    let mut pieces = Vec::new();
    let mut rest = word;
    while let Some((end, _)) = rest.char_indices().nth(size) {
        pieces.push(&rest[..end]);
        rest = &rest[end..];
    }
    pieces.push(rest);
    pieces
}
