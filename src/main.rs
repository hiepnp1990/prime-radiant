use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::{Args, Parser, Subcommand};
use prime_radiant::client::{self, HubAddress};
use prime_radiant::collect::live;
use prime_radiant::collect::{import_claude_code, import_codex, import_grok, import_t3_code};
use prime_radiant::hub;
use prime_radiant::index::{Indexer, Timestamp};
use prime_radiant::protocol::Request;
use prime_radiant::query::{Answer, Ask, Brief, Hit, ParentKind, Query, Stats};
use prime_radiant::raw::{Source, Store};
use prime_radiant::transcript::{Matches, Page};
use serde::Serialize;

const WIDTH: usize = 100;
/// A hub command for query commands given no `--data`, `--hub-socket`, or `--hub-command`.
const HUB_ENV: &str = "PRIME_RADIANT_HUB";

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
    /// Import Claude Code, Codex, Grok, and T3 Code sessions into the raw layer, then index them.
    Import {
        #[arg(long)]
        data: PathBuf,
        /// The machine or account the files come from.
        #[arg(long)]
        origin: String,
        #[command(flatten)]
        sources: Sources,
    },
    /// Count what the store holds, and list every stream and session.
    Stats {
        #[command(flatten)]
        target: Target,
        #[arg(long)]
        json: bool,
    },
    /// Find sessions by ID, alias, ID prefix, or words about the topic.
    Find {
        #[arg(required = true, num_args = 1..)]
        query: Vec<String>,
        #[command(flatten)]
        target: Target,
        #[arg(long)]
        json: bool,
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// Summarize one session: its first request, last answer, artifacts, and subagents.
    Brief {
        /// A session key, an alias, or a unique ID prefix of 8 or more characters.
        session: String,
        #[command(flatten)]
        target: Target,
        #[arg(long)]
        json: bool,
    },
    /// Print one page of a session's messages, about 12,000 characters, with secrets removed.
    Read {
        /// A session key, an alias, or a unique ID prefix of 8 or more characters.
        session: String,
        #[arg(long, default_value_t = 1)]
        page: usize,
        #[command(flatten)]
        target: Target,
        #[arg(long)]
        json: bool,
    },
    /// Search one session's messages for text, ignoring case, and name the page of each match.
    Grep {
        /// A session key, an alias, or a unique ID prefix of 8 or more characters.
        session: String,
        /// The text to find. Several words are searched as one phrase.
        #[arg(required = true, num_args = 1..)]
        pattern: Vec<String>,
        #[command(flatten)]
        target: Target,
        #[arg(long)]
        json: bool,
    },
    /// Run the hub, or reach it from another machine.
    Hub {
        #[command(subcommand)]
        command: HubCommand,
    },
    /// Send Claude Code, Codex, Grok, and T3 Code sessions to a hub as they change, and keep running.
    Collect {
        /// The machine or account the files come from.
        #[arg(long)]
        origin: String,
        #[command(flatten)]
        sources: Sources,
        #[command(flatten)]
        hub: HubArgs,
        /// How often to rescan every source for changes the file watcher missed.
        #[arg(long, default_value_t = 300)]
        sweep_seconds: u64,
    },
}

/// Where the sessions are. Each flag repeats, and at least one is required.
#[derive(Args)]
#[group(required = true, multiple = true)]
struct Sources {
    /// A Claude Code projects folder, such as ~/.claude/projects.
    #[arg(long = "claude-code")]
    claude_code: Vec<PathBuf>,
    /// A Codex home folder, such as ~/.codex.
    #[arg(long)]
    codex: Vec<PathBuf>,
    /// A Grok home folder, such as ~/.grok.
    #[arg(long)]
    grok: Vec<PathBuf>,
    /// A T3 Code database, such as ~/.t3/userdata/statev2.sqlite. It is only read.
    #[arg(long = "t3-code", value_name = "DATABASE")]
    t3_code: Vec<PathBuf>,
}

impl Sources {
    fn into_list(self) -> Vec<(Source, PathBuf)> {
        let tag = |source, paths: Vec<PathBuf>| paths.into_iter().map(move |path| (source, path));
        tag(Source::ClaudeCode, self.claude_code)
            .chain(tag(Source::Codex, self.codex))
            .chain(tag(Source::Grok, self.grok))
            .chain(tag(Source::T3Code, self.t3_code))
            .collect()
    }
}

#[derive(Subcommand)]
enum HubCommand {
    /// Serve a data directory on <data>/hub.sock, as the only writer of its raw layer and index.
    Serve {
        #[arg(long)]
        data: PathBuf,
    },
    /// Copy stdin to the hub's socket and the socket to stdout, for a collector that runs it over SSH.
    Connect {
        #[arg(long)]
        data: PathBuf,
    },
}

/// Where a query is answered: a local data folder, or a hub. With no flag, the hub command in PRIME_RADIANT_HUB.
#[derive(Args)]
#[group(multiple = false)]
struct Target {
    /// A data folder on this machine.
    #[arg(long)]
    data: Option<PathBuf>,
    /// The hub's socket on this machine, <data>/hub.sock.
    #[arg(long)]
    hub_socket: Option<PathBuf>,
    /// A command whose stdin and stdout reach the hub, such as "ssh <host> prime-radiant hub connect --data <dir>".
    #[arg(long)]
    hub_command: Option<String>,
}

/// Who answers a query, and the flags that name it again, for the command that reads the next page.
struct Answerer {
    at: At,
    flags: String,
}

enum At {
    Local(PathBuf),
    Hub(HubAddress),
}

impl Target {
    fn answerer(self) -> prime_radiant::Result<Answerer> {
        let (at, flags) = match (self.data, self.hub_socket, self.hub_command) {
            (Some(dir), _, _) => {
                let flags = format!(" --data {}", quoted(&dir.to_string_lossy()));
                (At::Local(dir), flags)
            }
            (None, Some(socket), _) => {
                let flags = format!(" --hub-socket {}", quoted(&socket.to_string_lossy()));
                (At::Hub(HubAddress::Socket(socket)), flags)
            }
            (None, None, Some(command)) => {
                let flags = format!(" --hub-command {}", quoted(&command));
                (At::Hub(HubAddress::Command(command)), flags)
            }
            (None, None, None) => match std::env::var(HUB_ENV) {
                Ok(command) if !command.trim().is_empty() => {
                    (At::Hub(HubAddress::Command(command)), String::new())
                }
                _ => {
                    return Err(prime_radiant::Error::Usage(format!(
                        "give --data, --hub-socket, or --hub-command, or set {HUB_ENV} to a command that reaches the hub"
                    )));
                }
            },
        };
        Ok(Answerer { at, flags })
    }
}

impl Answerer {
    fn ask(&self, ask: Ask) -> prime_radiant::Result<Answer> {
        match &self.at {
            At::Hub(hub) => Answer::try_from(client::ask(hub, Request::Ask(ask))?),
            At::Local(dir) => {
                let answer = Query::open(dir, now())?.answer(ask)?;
                if let Answer::Brief(brief) = &answer
                    && brief.freshness.rebuilt
                    && let Err(error) =
                        Indexer::open(dir).and_then(|mut index| index.store_brief(brief))
                {
                    eprintln!("warning: the rebuilt brief was not stored: {error}");
                }
                Ok(answer)
            }
        }
    }
}

#[derive(Args)]
#[group(required = true, multiple = false)]
struct HubArgs {
    /// The hub's socket on this machine, <data>/hub.sock.
    #[arg(long)]
    hub_socket: Option<PathBuf>,
    /// A command whose stdin and stdout reach the hub, such as "ssh <host> prime-radiant hub connect --data <dir>".
    #[arg(long)]
    hub_command: Option<String>,
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
            sources,
        } => import(&data, &origin, sources.into_list()),
        Command::Stats { target, json } => query(target, Ask::Stats, json),
        Command::Find {
            query: words,
            target,
            json,
            limit,
        } => {
            let ask = Ask::Find {
                query: words.join(" "),
                limit,
            };
            query(target, ask, json)
        }
        Command::Brief {
            session,
            target,
            json,
        } => query(target, Ask::Brief { session }, json),
        Command::Read {
            session,
            page,
            target,
            json,
        } => query(target, Ask::Read { session, page }, json),
        Command::Grep {
            session,
            pattern,
            target,
            json,
        } => {
            let ask = Ask::Grep {
                session,
                pattern: pattern.join(" "),
            };
            query(target, ask, json)
        }
        Command::Hub {
            command: HubCommand::Serve { data },
        } => hub::serve(&data),
        Command::Hub {
            command: HubCommand::Connect { data },
        } => Ok(hub::connect(&data)?),
        Command::Collect {
            origin,
            sources,
            hub,
            sweep_seconds,
        } => live::run(live::Options {
            origin,
            sources: sources.into_list(),
            hub: match (hub.hub_socket, hub.hub_command) {
                (Some(socket), _) => HubAddress::Socket(socket),
                (None, Some(command)) => HubAddress::Command(command),
                (None, None) => unreachable!("clap requires one of the hub flags"),
            },
            sweep: Duration::from_secs(sweep_seconds),
        }),
    }
}

fn query(target: Target, ask: Ask, json: bool) -> prime_radiant::Result<()> {
    let answerer = target.answerer()?;
    let answer = answerer.ask(ask)?;
    if json {
        match &answer {
            Answer::Found(hits) => print_json(hits),
            Answer::Brief(brief) => print_json(brief),
            Answer::Stats(stats) => print_json(stats),
            Answer::Page(page) => print_json(page),
            Answer::Matches(matches) => print_json(matches),
        }
    } else {
        print!(
            "{}",
            match &answer {
                Answer::Found(hits) => hits_text(hits),
                Answer::Brief(brief) => brief_text(brief),
                Answer::Stats(stats) => stats_text(stats),
                Answer::Page(page) => page_text(page, &answerer.flags),
                Answer::Matches(matches) => matches_text(matches),
            }
        );
    }
    Ok(())
}

fn import(data: &Path, origin: &str, sources: Vec<(Source, PathBuf)>) -> prime_radiant::Result<()> {
    let mut store = Store::open(data)?;
    for (source, path) in sources {
        let (summary, unit) = match source {
            Source::ClaudeCode => (import_claude_code(&path, origin, &mut store)?, "files"),
            Source::Codex => (import_codex(&path, origin, &mut store)?, "files"),
            Source::Grok => (import_grok(&path, origin, &mut store)?, "files"),
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
        let relation = match parent.kind {
            ParentKind::Fork => "fork of",
            ParentKind::Subagent => "subagent of",
        };
        line(&format!("{relation}: {}", parent.key));
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

fn page_text(page: &Page, flags: &str) -> String {
    let mut out = page.text();
    if page.page < page.pages {
        out += &format!(
            "\npage {} of {}. Next page: prime-radiant read {} --page {}{flags}\n",
            page.page,
            page.pages,
            quoted(&page.key),
            page.page + 1
        );
    } else {
        out += &format!("\npage {} of {}, the last page.\n", page.page, page.pages);
    }
    out
}

fn matches_text(matches: &Matches) -> String {
    let pattern = &matches.pattern;
    if matches.total == 0 {
        return format!("no matches for \"{pattern}\" in {}\n", matches.key);
    }
    let noun = if matches.total == 1 {
        "match"
    } else {
        "matches"
    };
    let mut out = format!(
        "{} {noun} for \"{pattern}\" in {}",
        matches.total, matches.key
    );
    if matches.total > matches.matches.len() {
        out += &format!(", the first {} shown", matches.matches.len());
    }
    out += "\n";
    for found in &matches.matches {
        out += &format!(
            "\n[{}] {}, page {}:\n",
            found.position, found.role, found.page
        );
        out += &wrap(&found.context, "  ");
    }
    out
}

/// `text` as one shell word.
fn quoted(text: &str) -> String {
    let plain = !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=@%+,".contains(c));
    if plain {
        text.to_owned()
    } else {
        format!("'{}'", text.replace('\'', "'\\''"))
    }
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
