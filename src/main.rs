use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use clap::builder::{PossibleValuesParser, TypedValueParser};
use clap::{ArgGroup, Args, Parser, Subcommand};
use prime_radiant::client::{self, HubAddress};
use prime_radiant::collect::live;
use prime_radiant::collect::progress::{self, Found, Phase, Snapshot};
use prime_radiant::collect::{import_claude_code, import_codex, import_grok, import_t3_code};
use prime_radiant::config::{self, Config, Places};
use prime_radiant::doctor::{self, Check};
use prime_radiant::harness::{self, HARNESSES, Harness};
use prime_radiant::hub;
use prime_radiant::index::{Indexer, Timestamp};
use prime_radiant::protocol::{OriginStats, Request};
use prime_radiant::query::{Answer, Ask, Brief, Filters, Find, Hit, ParentKind, Query, Stats};
use prime_radiant::raw::{Source, Store};
use prime_radiant::skill;
use prime_radiant::transcript::{Matches, Page};
use serde::Serialize;

const WIDTH: usize = 100;
const DAY_MS: i64 = 24 * 60 * 60 * 1000;
/// How long `status` waits for the hub, and how often `status --watch` asks it again.
const HUB_TIMEOUT: Duration = Duration::from_secs(10);
const HUB_EVERY: Duration = Duration::from_secs(5);

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
    /// Find sessions by ID, alias, ID prefix, words about the topic, or exact phrases.
    #[command(group = ArgGroup::new("search").required(true).multiple(true).args(["query", "phrases"]))]
    Find {
        /// Words about the topic, or a session key, an alias, or a unique ID prefix of 8 or more characters.
        #[arg(num_args = 1..)]
        query: Vec<String>,
        /// Only sessions with this text in one message or the title, its words adjacent and in order. Repeat to require several.
        #[arg(long = "phrase", value_name = "TEXT")]
        phrases: Vec<String>,
        /// Only sessions from this harness. Repeat for any of several.
        #[arg(long = "source", value_name = "SOURCE", value_parser = source())]
        sources: Vec<Source>,
        /// Only sessions that ran on this machine or account. Repeat for any of several.
        #[arg(long = "origin", value_name = "NAME")]
        origins: Vec<String>,
        /// Only sessions whose project path contains this text, ignoring case.
        #[arg(long, value_name = "TEXT")]
        project: Option<String>,
        /// Only sessions last active on or after this day (YYYY-MM-DD, in UTC) or RFC 3339 time.
        #[arg(long, value_name = "DATE", value_parser = since)]
        since: Option<Timestamp>,
        /// Only sessions last active on or before this day (YYYY-MM-DD, in UTC) or RFC 3339 time.
        #[arg(long, value_name = "DATE", value_parser = until)]
        until: Option<Timestamp>,
        #[command(flatten)]
        target: Target,
        /// Print the hits as JSON.
        #[arg(long)]
        json: bool,
        /// The most sessions to print.
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
    ///
    /// With no source flag, it reads every harness this machine has, and says which. With no hub flag, it reaches the
    /// hub a query command would: PRIME_RADIANT_HUB, then the config file, then the hub socket in the default data
    /// folder.
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
        /// Where to keep what this collector is doing, for `prime-radiant status`. It is rewritten about once a second.
        /// Defaults to collect-<origin>.json in $XDG_STATE_HOME/prime-radiant, or in ~/.local/state/prime-radiant.
        #[arg(long, value_name = "PATH")]
        state_file: Option<PathBuf>,
    },
    /// Show what each collector on this machine is doing, and what the hub holds from each origin.
    ///
    /// It asks the hub a query command would, and --data names the hub that serves that folder. Without a hub, it
    /// shows only the collectors, and why the hub part is missing.
    Status {
        /// The folder of the collectors' state files. Defaults to $XDG_STATE_HOME/prime-radiant, or
        /// ~/.local/state/prime-radiant.
        #[arg(long, value_name = "DIR")]
        state_dir: Option<PathBuf>,
        #[command(flatten)]
        target: Target,
        /// Redraw about once a second until Ctrl-C.
        #[arg(long)]
        watch: bool,
        /// Print one JSON object, or one line of JSON a second with --watch.
        #[arg(long)]
        json: bool,
    },
    /// Keep the hub or a collector running as this user's service: a systemd user unit on Linux, a launchd agent on macOS.
    Service {
        #[command(subcommand)]
        command: ServiceCommand,
    },
    /// Save the hub that commands reach when no flag names one, in a file that every shell reads.
    ///
    /// The file is $XDG_CONFIG_HOME/prime-radiant/config.toml, or ~/.config/prime-radiant/config.toml. A command takes
    /// a hub flag first, then PRIME_RADIANT_HUB, then this file, then the hub socket in ~/.local/share/prime-radiant
    /// when a hub serves that folder on this machine. A variable exported in ~/.bashrc never reaches a non-interactive
    /// shell, such as an agent's tool shell, and this file does.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Install the Prime Radiant skill for each harness, so that its agents know how to find past sessions.
    Skill {
        #[command(subcommand)]
        command: SkillCommand,
    },
    /// Check this machine's setup: the binary, the hub and the route to it, the harnesses, collectors, services, and
    /// skill. It changes nothing.
    ///
    /// Each line starts with ok, warn, or fail, and the line after a warning or failure gives the next step. It exits
    /// nonzero if a check fails.
    Doctor,
}

/// Where the sessions are. Each flag repeats. With none, `collect` reads every harness this machine has, and `import`
/// asks for one.
#[derive(Args)]
#[group(multiple = true)]
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

const SOURCE_FLAGS: &str = "--claude-code, --codex, --grok, or --t3-code";

impl Sources {
    fn named(self) -> Vec<(Source, PathBuf)> {
        let tag = |source, paths: Vec<PathBuf>| paths.into_iter().map(move |path| (source, path));
        tag(Source::ClaudeCode, self.claude_code)
            .chain(tag(Source::Codex, self.codex))
            .chain(tag(Source::Grok, self.grok))
            .chain(tag(Source::T3Code, self.t3_code))
            .collect()
    }

    /// The sources the flags name, or with none, every harness this machine has, which it passes to `say`.
    fn chosen(self, say: impl Fn(&str)) -> prime_radiant::Result<Vec<(Source, PathBuf)>> {
        let named = self.named();
        if !named.is_empty() {
            return Ok(named);
        }
        let home = home()?;
        let detected = harness::detect(&home);
        if detected.is_empty() {
            return Err(prime_radiant::Error::Usage(format!(
                "give {SOURCE_FLAGS}: this machine has no harness at {}",
                harness::looked_in(&home)
            )));
        }
        let found: Vec<String> = detected
            .iter()
            .map(|found| {
                format!(
                    "{} at {} ({})",
                    found.source.as_str(),
                    found.path.display(),
                    sessions(&found.sessions)
                )
            })
            .collect();
        say(&format!(
            "no source flag, so reading every harness on this machine: {}",
            found.join(", ")
        ));
        Ok(detected
            .into_iter()
            .map(|found| (found.source, found.path))
            .collect())
    }
}

fn sessions(count: &prime_radiant::Result<u64>) -> String {
    match count {
        Ok(1) => "1 session".to_owned(),
        Ok(n) => format!("{} sessions", progress::count(*n)),
        Err(error) => format!("sessions not counted: {error}"),
    }
}

fn home() -> prime_radiant::Result<PathBuf> {
    Places::from_env()
        .map(|places| places.home)
        .ok_or_else(|| prime_radiant::Error::Usage("HOME is not an absolute path".to_owned()))
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

/// Where a query is answered: a local data folder, or a hub. With no flag, the route in `config`.
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
            // The route comes from the environment or a file, so the next command finds it with no flag either.
            (None, None, None) => (
                At::Hub(route("--data, --hub-socket, or --hub-command")?.0),
                String::new(),
            ),
        };
        Ok(Answerer { at, flags })
    }
}

impl Answerer {
    fn ask(&self, ask: Ask) -> prime_radiant::Result<Answer> {
        match &self.at {
            At::Hub(hub) => Answer::try_from(client::ask(hub, Request::Ask(ask))?),
            At::Local(dir) => {
                let answer = Query::open(dir, Timestamp::now())?.answer(ask)?;
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
#[group(multiple = false)]
struct HubArgs {
    /// The hub's socket on this machine, <data>/hub.sock.
    #[arg(long)]
    hub_socket: Option<PathBuf>,
    /// A command whose stdin and stdout reach the hub, such as "ssh <host> prime-radiant hub connect --data <dir>".
    #[arg(long)]
    hub_command: Option<String>,
}

impl HubArgs {
    /// The hub the flags name, or the route a query command takes, which it passes to `say` with where it came from.
    fn address(self, say: impl Fn(&str)) -> prime_radiant::Result<HubAddress> {
        match (self.hub_socket, self.hub_command) {
            (Some(socket), _) => Ok(HubAddress::Socket(socket)),
            (None, Some(command)) => Ok(HubAddress::Command(command)),
            (None, None) => {
                let (hub, from) = route("--hub-socket or --hub-command")?;
                say(&format!("no hub flag, so the hub is {hub}, from {from}"));
                Ok(hub)
            }
        }
    }
}

/// The route a command takes with no hub flag, or the error that lists the `flags` and every other way to name a hub.
fn route(flags: &str) -> prime_radiant::Result<(HubAddress, config::RouteFrom)> {
    let places = Places::from_env();
    match config::hub_route(places.as_ref())? {
        Some(route) => Ok((route.hub, route.from)),
        None => Err(config::no_route(places.as_ref(), flags)),
    }
}

#[derive(Subcommand)]
enum ServiceCommand {
    /// Write the service's definition and start it. Installing the same service again replaces its definition.
    Install {
        #[command(subcommand)]
        service: InstallService,
    },
    /// Stop a service and remove its definition. Its data and log stay.
    Uninstall {
        #[command(subcommand)]
        service: UninstallService,
    },
    /// List this user's Prime Radiant services, whether each is running, and where its log is.
    List,
}

#[derive(Subcommand)]
enum InstallService {
    /// Run `hub serve` as a service.
    Hub {
        /// The data folder to serve. The hub listens on <data>/hub.sock.
        #[arg(long, default_value = "~/.local/share/prime-radiant")]
        data: PathBuf,
        #[command(flatten)]
        options: InstallOptions,
    },
    /// Run `collect` as a service, with the same source and hub flags.
    ///
    /// With no source or hub flag, it finds them as `collect` does and says what it found, and the definition keeps
    /// them. Install again after a new harness or `config set`.
    Collector {
        /// The machine or account the files come from. It names the service, so it may use only letters, digits, '.', '_', and '-'.
        #[arg(long)]
        origin: prime_radiant::service::Origin,
        #[command(flatten)]
        sources: Sources,
        #[command(flatten)]
        hub: HubArgs,
        #[command(flatten)]
        options: InstallOptions,
    },
}

#[derive(Args)]
struct InstallOptions {
    /// The most memory the service may use, such as 512M or 2G. Past it, the kernel ends the service and systemd restarts it. launchd has no memory cap.
    #[arg(long, value_name = "SIZE")]
    memory_max: Option<prime_radiant::service::MemoryMax>,
    /// The prime-radiant binary the service runs, such as a path that stays the same across upgrades. The default is this binary.
    #[arg(long, value_name = "PATH")]
    binary: Option<PathBuf>,
    /// Print the file and the commands, and change nothing.
    #[arg(long)]
    dry_run: bool,
}

impl InstallOptions {
    fn install(self, spec: prime_radiant::service::Spec) -> prime_radiant::Result<()> {
        prime_radiant::service::install(prime_radiant::service::Install {
            spec,
            binary: self.binary,
            memory_max: self.memory_max,
            dry_run: self.dry_run,
        })
    }
}

#[derive(Subcommand)]
enum UninstallService {
    /// Remove the hub's service.
    Hub {
        /// Print the commands, and change nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Remove the service of the collector for one origin.
    Collector {
        /// The origin the collector was installed with.
        #[arg(long)]
        origin: prime_radiant::service::Origin,
        /// Print the commands, and change nothing.
        #[arg(long)]
        dry_run: bool,
    },
}

impl ServiceCommand {
    fn run(self) -> prime_radiant::Result<()> {
        use prime_radiant::service::{self, Kind, Spec};
        match self {
            ServiceCommand::Install {
                service: InstallService::Hub { data, options },
            } => options.install(Spec::Hub { data }),
            ServiceCommand::Install {
                service:
                    InstallService::Collector {
                        origin,
                        sources,
                        hub,
                        options,
                    },
            } => {
                let say = |note: &str| println!("{note}");
                options.install(Spec::Collector {
                    origin,
                    sources: sources.chosen(say)?,
                    hub: hub.address(say)?,
                })
            }
            ServiceCommand::Uninstall {
                service: UninstallService::Hub { dry_run },
            } => service::uninstall(Kind::Hub, dry_run),
            ServiceCommand::Uninstall {
                service: UninstallService::Collector { origin, dry_run },
            } => service::uninstall(Kind::Collector(origin), dry_run),
            ServiceCommand::List => service::list(),
        }
    }
}

#[derive(Subcommand)]
enum ConfigCommand {
    /// Save the hub in the config file, in place of the hub it named before.
    Set {
        #[command(subcommand)]
        setting: Setting,
    },
    /// Remove a setting from the config file.
    Unset {
        #[command(subcommand)]
        setting: Unset,
    },
    /// Print the hub a command reaches with no hub flag, where that comes from, and what the config file names.
    Show,
}

#[derive(Subcommand)]
enum Setting {
    /// A command whose stdin and stdout reach the hub, given as one quoted argument.
    HubCommand {
        /// Such as "ssh <host> prime-radiant hub connect --data <dir>". It runs with `sh -c`, which expands ~ and $HOME.
        command: String,
    },
    /// The hub's socket on this machine, <data>/hub.sock.
    HubSocket {
        /// The socket. A relative path is saved as an absolute one.
        path: PathBuf,
    },
}

#[derive(Subcommand)]
enum Unset {
    /// The hub, a command or a socket.
    Hub,
}

impl ConfigCommand {
    fn run(self) -> prime_radiant::Result<()> {
        let places = Places::from_env().ok_or_else(|| {
            prime_radiant::Error::Usage(
                "HOME is not an absolute path, so there is no place for the config file".to_owned(),
            )
        })?;
        let file = places.config_file.display();
        match self {
            ConfigCommand::Set { setting } => {
                let hub = match setting {
                    Setting::HubCommand { command } if command.trim().is_empty() => {
                        return Err(prime_radiant::Error::Usage(
                            "the hub command is empty".to_owned(),
                        ));
                    }
                    Setting::HubCommand { command } => HubAddress::Command(command),
                    Setting::HubSocket { path } => {
                        HubAddress::Socket(match path.strip_prefix("~") {
                            Ok(rest) => places.home.join(rest),
                            Err(_) => std::path::absolute(&path)?,
                        })
                    }
                };
                config::write(
                    &places,
                    &Config {
                        hub: Some(hub.clone()),
                    },
                )?;
                println!("wrote {file}: {hub}");
                if let HubAddress::Socket(socket) = &hub
                    && !socket.exists()
                {
                    eprintln!("warning: there is no socket at {} yet", socket.display());
                }
                if config::environment().is_some() {
                    eprintln!(
                        "warning: {} is set in this shell, and it comes before the config file",
                        config::HUB_ENV
                    );
                }
                if std::env::var_os("XDG_CONFIG_HOME")
                    .is_some_and(|dir| Path::new(&dir) != places.home.join(".config"))
                {
                    eprintln!(
                        "note: XDG_CONFIG_HOME is set here, and a shell without it reads ~/.config/prime-radiant/config.toml instead"
                    );
                }
                println!("`prime-radiant doctor` checks that the hub answers");
            }
            ConfigCommand::Unset {
                setting: Unset::Hub,
            } => {
                let existed = places.config_file.exists();
                config::write(&places, &Config::default())?;
                if existed {
                    println!("removed {file}, which held only the hub");
                } else {
                    println!("no config file at {file}");
                }
            }
            ConfigCommand::Show => print!("{}", config_text(&places)),
        }
        Ok(())
    }
}

/// The route a command takes with no hub flag, where it comes from, and what the config file names.
fn config_text(places: &Places) -> String {
    let mut out = match config::hub_route(Some(places)) {
        Ok(Some(route)) => format!("hub: {}\nfrom: {}\n", route.hub, route.from),
        Ok(None) => "hub: none, so a command needs a flag that names one\n".to_owned(),
        Err(error) => format!("hub: none, because {error}\n"),
    };
    let file = places.config_file.display();
    out += &match config::read(places) {
        Ok(Config { hub: Some(hub) }) => format!("config file: {file}, which names {hub}\n"),
        Ok(_) if places.config_file.exists() => {
            format!("config file: {file}, which names no hub\n")
        }
        Ok(_) => format!("config file: {file}, which does not exist\n"),
        Err(_) => format!("config file: {file}, which cannot be read\n"),
    };
    out
}

#[derive(Subcommand)]
enum SkillCommand {
    /// Write the skill to <skills folder>/prime-radiant/SKILL.md: ~/.claude/skills, ~/.codex/skills, or ~/.grok/skills.
    ///
    /// Without --harness, it installs for every harness this machine has. It replaces only a file it wrote before,
    /// which it marks with a line after the frontmatter, unless --force is given.
    Install {
        #[command(flatten)]
        options: SkillOptions,
    },
    /// Remove the skill that skill install wrote. Without --harness, from every harness that reads skills.
    Uninstall {
        #[command(flatten)]
        options: SkillOptions,
    },
}

#[derive(Args)]
struct SkillOptions {
    /// A harness to change. Repeat for several.
    #[arg(long = "harness", value_name = "HARNESS", value_parser = skill_harness())]
    harnesses: Vec<Source>,
    /// Replace or remove a skill file that skill install did not write.
    #[arg(long)]
    force: bool,
    /// Print what would change, and change nothing.
    #[arg(long)]
    dry_run: bool,
}

impl SkillCommand {
    fn run(self) -> prime_radiant::Result<()> {
        let home = home()?;
        let (mode, options) = match self {
            SkillCommand::Install { options } => (skill::Mode::Install, options),
            SkillCommand::Uninstall { options } => (skill::Mode::Uninstall, options),
        };
        let reads_skills = |source: &Source| Harness::of(*source).reads_skills();
        let harnesses: Vec<Source> = match (mode, options.harnesses.is_empty()) {
            (_, false) => options.harnesses,
            (skill::Mode::Install, true) => {
                let found: Vec<Source> = harness::detect(&home)
                    .into_iter()
                    .map(|found| found.source)
                    .filter(reads_skills)
                    .collect();
                if found.is_empty() {
                    return Err(prime_radiant::Error::Usage(format!(
                        "give --harness: this machine has no harness that reads skills at {}",
                        harness::looked_in(&home)
                    )));
                }
                found
            }
            (skill::Mode::Uninstall, true) => HARNESSES
                .iter()
                .map(|harness| harness.source)
                .filter(reads_skills)
                .collect(),
        };
        skill::run(mode, &harnesses, &home, options.force, options.dry_run)
    }
}

fn skill_harness() -> impl TypedValueParser<Value = Source> {
    let names = HARNESSES
        .iter()
        .filter(|harness| harness.reads_skills())
        .map(|harness| harness.source.as_str());
    PossibleValuesParser::new(names)
        .map(|name| Source::parse(&name).expect("clap allows only harness names"))
}

fn doctor_command() -> prime_radiant::Result<()> {
    let mut failed = 0;
    doctor::run(&mut |check| match check {
        Check::Ok(what) => println!("ok    {what}"),
        Check::Warn { what, next } => println!("warn  {what}\n      next: {next}"),
        Check::Fail { what, next } => {
            failed += 1;
            println!("fail  {what}\n      next: {next}");
        }
    });
    match failed {
        0 => Ok(()),
        1 => Err(prime_radiant::Error::Usage("1 check failed".to_owned())),
        n => Err(prime_radiant::Error::Usage(format!("{n} checks failed"))),
    }
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
        } => match sources.named() {
            sources if sources.is_empty() => {
                Err(prime_radiant::Error::Usage(format!("give {SOURCE_FLAGS}")))
            }
            sources => import(&data, &origin, sources),
        },
        Command::Stats { target, json } => query(target, Ask::Stats, json),
        Command::Find {
            query: words,
            phrases,
            sources,
            origins,
            project,
            since,
            until,
            target,
            json,
            limit,
        } => {
            let ask = Ask::Find(Find {
                query: words.join(" "),
                phrases,
                filters: Filters {
                    sources,
                    origins,
                    project,
                    since,
                    until,
                },
                limit,
            });
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
            state_file,
        } => {
            let say = |note: &str| eprintln!("collect: {note}");
            let sources = sources.chosen(say)?;
            let hub = hub.address(say)?;
            live::run(live::Options {
                state_file: state_file.or_else(|| {
                    progress::default_dir().map(|dir| dir.join(progress::file_name(&origin)))
                }),
                origin,
                sources,
                hub,
                sweep: Duration::from_secs(sweep_seconds),
            })
        }
        Command::Service { command } => command.run(),
        Command::Config { command } => command.run(),
        Command::Skill { command } => command.run(),
        Command::Doctor => doctor_command(),
        Command::Status {
            state_dir,
            target,
            watch,
            json,
        } => status(state_dir, target, watch, json),
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

/// What `status` shows: every collector's state file, and what the hub holds from each origin.
#[derive(Serialize)]
struct Status<'a> {
    state_dir: &'a Path,
    collectors: Vec<Found>,
    hub: &'a HubPart,
}

/// What the hub holds from each origin, or why status cannot say.
#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum HubPart {
    Origins(Vec<OriginStats>),
    Unavailable(String),
}

/// Asks the hub in a thread of its own, so that a hub that never answers cannot stop `status`.
struct HubPoll {
    /// Where the hub is, as the user named it.
    label: Option<String>,
    answers: Option<Receiver<HubPart>>,
    latest: HubPart,
    answered: Instant,
    /// How long the hub may go without answering before status says so.
    patience: Duration,
}

impl HubPoll {
    fn start(target: Target, repeat: bool) -> Self {
        let (label, address) = match target.answerer() {
            Ok(Answerer {
                at: At::Local(dir), ..
            }) => {
                let socket = hub::socket_path(&dir);
                (socket.display().to_string(), HubAddress::Socket(socket))
            }
            Ok(Answerer {
                at: At::Hub(address),
                ..
            }) => (
                match &address {
                    HubAddress::Socket(socket) => socket.display().to_string(),
                    HubAddress::Command(command) => command.clone(),
                },
                address,
            ),
            Err(error) => {
                return Self {
                    label: None,
                    answers: None,
                    latest: HubPart::Unavailable(format!("no hub to ask: {error}")),
                    answered: Instant::now(),
                    patience: HUB_TIMEOUT,
                };
            }
        };
        let (send, answers) = mpsc::channel();
        thread::spawn(move || {
            loop {
                if send.send(ask_origins(&address)).is_err() || !repeat {
                    return;
                }
                thread::sleep(HUB_EVERY);
            }
        });
        Self {
            label: Some(label),
            answers: Some(answers),
            latest: HubPart::Unavailable("waiting for the hub to answer".to_owned()),
            answered: Instant::now(),
            // Asked again every few seconds, the hub has that much longer before it counts as quiet.
            patience: HUB_TIMEOUT + if repeat { HUB_EVERY } else { Duration::ZERO },
        }
    }

    /// The hub's latest answer, waiting up to `wait` for a newer one.
    fn latest(&mut self, wait: Duration) -> &HubPart {
        if let Some(answers) = &self.answers {
            let mut next = answers.recv_timeout(wait).ok();
            while let Some(answer) = next {
                self.latest = answer;
                self.answered = Instant::now();
                next = answers.try_recv().ok();
            }
            let quiet = self.answered.elapsed();
            if quiet >= self.patience {
                self.latest = HubPart::Unavailable(format!(
                    "the hub has not answered for {}",
                    progress::duration(quiet.as_millis() as i64)
                ));
            }
        }
        &self.latest
    }
}

fn ask_origins(hub: &HubAddress) -> HubPart {
    match client::origins(hub) {
        Ok(origins) => HubPart::Origins(origins),
        Err(error) => HubPart::Unavailable(error.to_string()),
    }
}

fn status(
    state_dir: Option<PathBuf>,
    target: Target,
    watch: bool,
    json: bool,
) -> prime_radiant::Result<()> {
    let dir = state_dir.or_else(progress::default_dir).ok_or_else(|| {
        prime_radiant::Error::Usage(
            "neither XDG_STATE_HOME nor HOME is set; give --state-dir".to_owned(),
        )
    })?;
    let mut hub = HubPoll::start(target, watch);
    let redraw = watch && !json && io::stdout().is_terminal();
    let wait = if watch { Duration::ZERO } else { HUB_TIMEOUT };
    loop {
        let started = Instant::now();
        let label = hub.label.clone();
        let status = Status {
            state_dir: &dir,
            collectors: progress::read_dir(&dir)?,
            hub: hub.latest(wait),
        };
        if json && watch {
            println!(
                "{}",
                serde_json::to_string(&status).expect("output serializes")
            );
        } else if json {
            print_json(&status);
        } else {
            if redraw {
                print!("\x1b[H\x1b[2J");
            }
            print!(
                "{}",
                status_text(&status, label.as_deref(), Timestamp::now())
            );
        }
        if !watch {
            return Ok(());
        }
        thread::sleep(Duration::from_secs(1).saturating_sub(started.elapsed()));
    }
}

fn status_text(status: &Status, hub_label: Option<&str>, now: Timestamp) -> String {
    let mut out = String::new();
    if status.collectors.is_empty() {
        out += &wrap(
            &format!(
                "collectors: none has written its state in {}",
                status.state_dir.display()
            ),
            "",
        );
    } else {
        out += &wrap(&format!("collectors ({}):", status.state_dir.display()), "");
    }
    for found in &status.collectors {
        match found {
            Found::Collector { running, state, .. } => out += &collector_text(state, *running, now),
            Found::Unreadable { file, error } => {
                out += &wrap(&format!("cannot read {}: {error}", file.display()), "  ");
            }
        }
    }
    out += "\n";
    match status.hub {
        HubPart::Origins(origins) => {
            out += &wrap(&format!("hub ({}):", hub_label.unwrap_or("?")), "");
            if origins.is_empty() {
                out += "  no streams yet\n";
            }
            for origin in origins {
                out += &wrap(
                    &format!(
                        "{}: {} streams, {}, {}",
                        origin.origin,
                        progress::count(origin.streams),
                        progress::size(origin.confirmed_bytes),
                        match origin.last_write {
                            Some(at) => format!("last write {}", progress::ago(at, now)),
                            None => "no write since the hub started".to_owned(),
                        }
                    ),
                    "  ",
                );
            }
        }
        HubPart::Unavailable(why) => out += &wrap(&format!("hub: unavailable: {why}"), ""),
    }
    out
}

/// A collector's lines. A stopped one is shown as it was when it last wrote its state.
fn collector_text(state: &Snapshot, running: bool, now: Timestamp) -> String {
    let (mut out, at, was) = if running {
        let header = format!(
            "{}: running, pid {}, started {}",
            state.origin,
            state.pid,
            progress::ago(state.started_at, now)
        );
        (wrap(&header, "  "), now, "")
    } else {
        let header = format!(
            "{}: stopped, last seen {}",
            state.origin,
            progress::ago(state.updated_at, now)
        );
        (wrap(&header, "  "), state.updated_at, "when last seen: ")
    };
    out += &wrap(&format!("{was}{}", state.summary(at)), "    ");
    match &state.phase {
        Phase::FirstSync { progress, .. } => {
            let files = format!(
                "{} of {} files done",
                progress::count(progress.files_done),
                progress::count(progress.files_total)
            );
            out += &wrap(
                &match &progress.current {
                    Some(current) => format!(
                        "sending {}, {} · {files}",
                        current.locator,
                        progress::size_of(current.bytes_done, current.bytes_total)
                    ),
                    None => files,
                },
                "    ",
            );
        }
        Phase::Live { first_sync, .. } => {
            let sent = match first_sync.files_total {
                0 => "first sync: the hub lacked nothing".to_owned(),
                _ => format!(
                    "first sync {}%: {} files, {}",
                    first_sync.percent(),
                    progress::count(first_sync.files_done),
                    progress::size(first_sync.bytes_done)
                ),
            };
            out += &wrap(&sent, "    ");
        }
        Phase::Reconnecting { down_since, .. } => {
            out += &wrap(
                &format!(
                    "no connection to the hub for {}",
                    progress::duration(at.0 - down_since.0)
                ),
                "    ",
            );
        }
        Phase::Connecting | Phase::Scanning { .. } => {}
    }
    let mut totals = format!("sent {} since it started", progress::size(state.sent_bytes));
    totals += &match (&state.last_error, state.errors) {
        (Some(note), errors) => format!(
            " · {} {}, the last {}: {}",
            progress::count(errors),
            if errors == 1 { "error" } else { "errors" },
            progress::ago(note.at, now),
            note.message
        ),
        (None, _) => " · no errors".to_owned(),
    };
    out += &wrap(&totals, "    ");
    out
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

fn source() -> impl TypedValueParser<Value = Source> {
    let names = [
        Source::ClaudeCode,
        Source::Codex,
        Source::Grok,
        Source::T3Code,
    ]
    .map(Source::as_str);
    PossibleValuesParser::new(names)
        .map(|name| Source::parse(&name).expect("clap allows only source names"))
}

fn since(text: &str) -> Result<Timestamp, String> {
    day_or_time(text).map(|(first, _)| first)
}

fn until(text: &str) -> Result<Timestamp, String> {
    day_or_time(text).map(|(_, last)| last)
}

/// The first and last millisecond of a UTC day, `YYYY-MM-DD`, or an RFC 3339 time twice.
fn day_or_time(text: &str) -> Result<(Timestamp, Timestamp), String> {
    if text.len() == 10
        && let Some(start) = Timestamp::parse_rfc3339(&format!("{text}T00:00:00Z"))
    {
        return Ok((start, Timestamp(start.0 + DAY_MS - 1)));
    }
    Timestamp::parse_rfc3339(text)
        .map(|at| (at, at))
        .ok_or_else(|| {
            "expected a day such as 2026-01-15, or an RFC 3339 time such as 2026-01-15T09:30:00Z"
                .to_owned()
        })
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
