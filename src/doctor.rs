//! `prime-radiant doctor`: read-only checks of this machine's setup. Each check is a small function that returns
//! [`Check`]s, and a warning or a failure carries the next step.

use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::client::{self, HubAddress, NoOrigins};
use crate::collect::progress::{self, Found, Phase};
use crate::config::{self, Config, Places, Route, RouteFrom};
use crate::harness::Detected;
use crate::index::Timestamp;
use crate::service::{self, shell_word};
use crate::skill::{self, Present};
use crate::{Result, harness};

/// How long the hub may take to answer.
const HUB_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, PartialEq, Eq)]
pub enum Check {
    Ok(String),
    Warn { what: String, next: String },
    Fail { what: String, next: String },
}

fn warn(what: impl Into<String>, next: impl Into<String>) -> Check {
    Check::Warn {
        what: what.into(),
        next: next.into(),
    }
}

fn fail(what: impl Into<String>, next: impl Into<String>) -> Check {
    Check::Fail {
        what: what.into(),
        next: next.into(),
    }
}

/// Runs every check on this machine, in the order a new user sets things up, and hands each to `report` as soon as it
/// is done, so that what the hub command prints on stderr comes just before the check of the hub.
pub fn run(report: &mut dyn FnMut(Check)) {
    let mut out = Out(report);
    let exe = std::env::current_exe().ok();
    out.all([binary(exe.as_deref())]);
    out.all(
        exe.as_deref()
            .map(|exe| on_path(exe, std::env::var_os("PATH").as_deref())),
    );
    let Some(places) = Places::from_env() else {
        return out.all([fail(
            "HOME is not an absolute path, so there is no config file or harness to check",
            "run doctor from a shell that sets HOME",
        )]);
    };
    match config::hub_route(Some(&places)) {
        Ok(Some(route)) => {
            out.all([Check::Ok(format!(
                "hub: {}, from {}",
                route.hub, route.from
            ))]);
            out.all(environment_only(
                &route,
                config::read(&places).ok().as_ref(),
            ));
            let reached = reach(&route.hub, HUB_TIMEOUT);
            let unreachable = matches!(reached, Check::Fail { .. });
            out.all([reached]);
            if unreachable {
                out.all(by_name_over_ssh(&route.hub));
            }
            out.all(ssh_sharing(&route.hub));
        }
        Ok(None) => out.all([no_hub(&places)]),
        Err(error) => out.all([fail(
            error.to_string(),
            "fix the file, or replace it with `prime-radiant config set hub-command \"<command>\"`",
        )]),
    }
    let detected = harness::detect(&places.home);
    out.all(harnesses(&detected, &places.home));
    match progress::default_dir().map(|dir| progress::read_dir(&dir)) {
        Some(Ok(found)) => out.all(collectors(&found, !detected.is_empty(), Timestamp::now())),
        Some(Err(error)) => out.all([warn(
            format!("cannot read the collectors' state files: {error}"),
            "check that the state folder is readable",
        )]),
        None => {}
    }
    out.all(services(service::installed()));
    out.all(skills(&detected, &places.home));
}

struct Out<'a>(&'a mut dyn FnMut(Check));

impl Out<'_> {
    fn all(&mut self, checks: impl IntoIterator<Item = Check>) {
        checks.into_iter().for_each(&mut *self.0);
    }
}

fn binary(exe: Option<&Path>) -> Check {
    let version = env!("CARGO_PKG_VERSION");
    match exe {
        Some(exe) => Check::Ok(format!("prime-radiant {version} at {}", exe.display())),
        None => Check::Ok(format!("prime-radiant {version}")),
    }
}

/// Whether `prime-radiant` on `path` is this binary, the one agents run by name.
fn on_path(exe: &Path, path: Option<&OsStr>) -> Check {
    let found = path
        .into_iter()
        .flat_map(std::env::split_paths)
        .map(|dir| dir.join("prime-radiant"))
        .find(|candidate| {
            fs::metadata(candidate)
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        });
    let folder = exe.parent().unwrap_or(exe).display();
    match found {
        None => warn(
            "prime-radiant is not on PATH, so an agent cannot run it by name",
            format!(
                "add {folder} to PATH where your login shell sets it, such as ~/.profile or ~/.zprofile"
            ),
        ),
        Some(found) if same_file(&found, exe) => {
            Check::Ok("prime-radiant on PATH is this binary".to_owned())
        }
        Some(found) => warn(
            format!(
                "prime-radiant on PATH is {}, not this binary",
                found.display()
            ),
            format!(
                "agents run the one on PATH, so replace it with this one, or put {folder} first on PATH"
            ),
        ),
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    matches!((fs::canonicalize(a), fs::canonicalize(b)), (Ok(a), Ok(b)) if a == b)
}

fn no_hub(places: &Places) -> Check {
    fail(
        format!(
            "no hub is set: {} is unset, {} names none, and there is no hub socket at {}",
            config::HUB_ENV,
            places.config_file.display(),
            places.default_socket().display()
        ),
        "save the hub with `prime-radiant config set hub-command \"ssh <hub-host> .local/bin/prime-radiant hub \
         connect\"`, or with `prime-radiant config set hub-socket <dir>/hub.sock` on the hub's machine",
    )
}

/// A warning when `PRIME_RADIANT_HUB` names the hub and the config file does not, because a shell that skips
/// `~/.bashrc` never sees the variable.
fn environment_only(route: &Route, config: Option<&Config>) -> Option<Check> {
    let HubAddress::Command(command) = &route.hub else {
        return None;
    };
    let saved = config.and_then(|config| config.hub.as_ref());
    if route.from != RouteFrom::Environment || saved == Some(&route.hub) {
        return None;
    }
    let what = match saved {
        None => format!(
            "only {} names the hub, and non-interactive shells, such as an agent's tool shell, often skip ~/.bashrc and \
             never see it",
            config::HUB_ENV
        ),
        Some(other) => format!(
            "{} names a different hub than the config file, so a shell without the variable reaches {other}",
            config::HUB_ENV
        ),
    };
    Some(warn(
        what,
        format!(
            "prime-radiant config set hub-command {}",
            shell_word(command)
        ),
    ))
}

/// Asks the hub what it holds, giving up after `timeout`.
fn reach(hub: &HubAddress, timeout: Duration) -> Check {
    let (send, answer) = mpsc::channel();
    let address = hub.clone();
    let started = Instant::now();
    thread::spawn(move || {
        let _ = send.send(client::origins(&address));
    });
    let answer = answer.recv_timeout(timeout);
    let took = format!("{:.1} ms", started.elapsed().as_secs_f64() * 1000.0);
    match answer {
        Ok(Ok(origins)) => {
            let streams: u64 = origins.iter().map(|origin| origin.streams).sum();
            Check::Ok(format!(
                "the hub answered in {took}: {} streams from {} {}",
                progress::count(streams),
                origins.len(),
                if origins.len() == 1 {
                    "origin"
                } else {
                    "origins"
                }
            ))
        }
        Ok(Err(NoOrigins::Older(message))) => warn(
            format!(
                "the hub answered in {took}, but it is older than this client and does not know the origins question: {message}"
            ),
            "install this version of prime-radiant on the hub's machine, then restart the hub",
        ),
        Ok(Err(error)) => fail(format!("{error} (after {took})"), unreachable_next(hub)),
        Err(_) => fail(
            format!("the hub did not answer within {} s", timeout.as_secs()),
            unreachable_next(hub),
        ),
    }
}

fn unreachable_next(hub: &HubAddress) -> String {
    match hub {
        HubAddress::Socket(socket) => {
            let data = socket.parent().unwrap_or(socket).display();
            format!(
                "start the hub that serves {data}: `prime-radiant service install hub --data {data}`, or \
                 `prime-radiant hub serve --data {data}`"
            )
        }
        HubAddress::Command(command) if by_path(command).is_some() => {
            format!("run `{command} </dev/null` to see its error")
        }
        HubAddress::Command(command) => format!(
            "run `{command} </dev/null` to see its error. Over SSH, the hub machine's non-interactive shell may not \
             have prime-radiant on PATH, so give its full path in the command"
        ),
    }
}

/// For an SSH hub command that runs `prime-radiant` by name, a warning with the same command naming the binary by the
/// path that `install.sh` and the README use.
fn by_name_over_ssh(hub: &HubAddress) -> Option<Check> {
    let HubAddress::Command(command) = hub else {
        return None;
    };
    let (host, fixed) = by_path(command)?;
    Some(warn(
        format!(
            "the hub command runs prime-radiant by name on {host}, and the non-interactive shell that SSH runs it in \
             may not have ~/.local/bin on PATH"
        ),
        format!(
            "if it printed that prime-radiant was not found, name the binary by its path: prime-radiant config set \
             hub-command {}",
            shell_word(&fixed)
        ),
    ))
}

/// The host and the command with `.local/bin/` put before the `prime-radiant` that an SSH command runs on the host, or
/// `None` when the command is not `ssh` or names the binary some other way. The rest of the text stays as the user
/// wrote it, so that its quotes, `~`, and `$HOME` still expand where they did.
fn by_path(command: &str) -> Option<(String, String)> {
    let remote_program = |command: &str| {
        let (_, host, remote) = ssh_destination(&words(command)?)?;
        // ssh joins the remote words with spaces, and the shell on the hub's machine splits them again.
        let program = remote.join(" ").split_whitespace().next()?.to_owned();
        Some((host, program))
    };
    let (host, program) = remote_program(command)?;
    if program != "prime-radiant" {
        return None;
    }
    command
        .match_indices("prime-radiant")
        .map(|(at, _)| format!("{}.local/bin/{}", &command[..at], &command[at..]))
        .find(|fixed| {
            remote_program(fixed).is_some_and(|(_, program)| program == ".local/bin/prime-radiant")
        })
        .map(|fixed| (host, fixed))
}

/// For a hub reached over SSH, a hint when SSH connection sharing is off, so that each query opens a new connection.
fn ssh_sharing(hub: &HubAddress) -> Option<Check> {
    let HubAddress::Command(command) = hub else {
        return None;
    };
    let (options, host, _) = ssh_destination(&words(command)?)?;
    let output = Command::new("ssh")
        .args(&options)
        .arg("-G")
        .arg(&host)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|output| output.status.success())?;
    let settings = String::from_utf8_lossy(&output.stdout);
    let setting = |name: &str| {
        settings
            .lines()
            .find_map(|line| line.strip_prefix(name)?.strip_prefix(' '))
            .map(str::to_owned)
    };
    let master = setting("controlmaster");
    let path = setting("controlpath").filter(|path| path != "none");
    Some(match (master.as_deref(), path) {
        (Some("false") | None, _) | (_, None) => warn(
            format!(
                "SSH connection sharing is off for {host}, so every query opens a new SSH connection"
            ),
            format!(
                "to reuse one connection, add to ~/.ssh/config: Host {host} / ControlMaster auto / ControlPath \
                 ~/.ssh/control-%C / ControlPersist 10m"
            ),
        ),
        _ => Check::Ok(format!("SSH connection sharing is on for {host}")),
    })
}

/// The options before the destination, the destination, and the remote command's words of an `ssh` command, or `None`
/// for another command.
fn ssh_destination(words: &[String]) -> Option<(Vec<String>, String, Vec<String>)> {
    // Options of ssh that take a value.
    const VALUED: &str = "BbcDEeFIiJLlmOoPpQRSWw";
    let mut words = words.iter().skip_while(|word| is_assignment(word));
    let program = words.next()?;
    if Path::new(program).file_name()? != "ssh" {
        return None;
    }
    let mut options = Vec::new();
    while let Some(word) = words.next() {
        match word.strip_prefix('-') {
            Some(flags) if !flags.is_empty() => {
                options.push(word.clone());
                let valued = flags
                    .char_indices()
                    .find(|(_, flag)| VALUED.contains(*flag));
                if valued.is_some_and(|(at, _)| at + 1 == flags.len()) {
                    options.push(words.next()?.clone());
                }
            }
            _ => return Some((options, word.clone(), words.cloned().collect())),
        }
    }
    None
}

fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty() && name.chars().all(|c| c == '_' || c.is_ascii_alphanumeric())
    })
}

/// The words of a simple shell command, with quotes and backslashes removed. `None` if a quote is not closed.
fn words(command: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut word: Option<String> = None;
    let mut chars = command.chars();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => words.extend(word.take()),
            '\'' => {
                let word = word.get_or_insert_default();
                loop {
                    match chars.next()? {
                        '\'' => break,
                        c => word.push(c),
                    }
                }
            }
            '"' => {
                let word = word.get_or_insert_default();
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' => match chars.next()? {
                            c @ ('"' | '\\' | '$' | '`') => word.push(c),
                            c => {
                                word.push('\\');
                                word.push(c);
                            }
                        },
                        c => word.push(c),
                    }
                }
            }
            '\\' => word.get_or_insert_default().push(chars.next()?),
            c => word.get_or_insert_default().push(c),
        }
    }
    words.extend(word);
    Some(words)
}

fn harnesses(detected: &[Detected], home: &Path) -> Vec<Check> {
    if detected.is_empty() {
        return vec![warn(
            format!("no harness found: looked for {}", harness::looked_in(home)),
            "if a harness keeps its sessions elsewhere, give collect the folder, such as --claude-code <folder>",
        )];
    }
    detected
        .iter()
        .map(|found| match &found.sessions {
            Ok(sessions) => Check::Ok(format!(
                "harness {}: {}, {} {}",
                found.source.as_str(),
                found.path.display(),
                progress::count(*sessions),
                if *sessions == 1 {
                    "session"
                } else {
                    "sessions"
                }
            )),
            Err(error) => warn(
                format!(
                    "harness {}: {}, whose sessions cannot be counted: {error}",
                    found.source.as_str(),
                    found.path.display()
                ),
                "check that it is readable by this user",
            ),
        })
        .collect()
}

/// Each collector that wrote a state file on this machine. With none, a warning when there are sessions to collect.
fn collectors(found: &[Found], has_sessions: bool, now: Timestamp) -> Vec<Check> {
    if found.is_empty() && has_sessions {
        return vec![warn(
            "no collector has run on this machine, so its sessions don't reach the hub",
            "prime-radiant service install collector --origin <a name for this machine>",
        )];
    }
    found
        .iter()
        .map(|found| match found {
            Found::Collector { running: true, state, .. } => {
                let what = format!("collector {}: running, {}", state.origin, state.summary(now));
                match state.phase {
                    Phase::Reconnecting { .. } => warn(what, "it cannot reach the hub; see its log"),
                    _ => Check::Ok(what),
                }
            }
            Found::Collector { running: false, state, .. } => warn(
                format!(
                    "collector {}: stopped, last seen {}, when it was {}",
                    state.origin,
                    progress::ago(state.updated_at, now),
                    state.summary(state.updated_at)
                ),
                format!(
                    "start it again, such as with `prime-radiant service install collector --origin {}`, or read \
                     its log to see why it stopped",
                    state.origin
                ),
            ),
            Found::Unreadable { file, error } => warn(
                format!("cannot read {}: {error}", file.display()),
                "remove the file if no collector writes it",
            ),
        })
        .collect()
}

fn services(installed: Result<Vec<service::Installed>>) -> Vec<Check> {
    match installed {
        Ok(services) if services.is_empty() => {
            vec![Check::Ok("services: none installed".to_owned())]
        }
        Ok(services) => services
            .into_iter()
            .map(|service| match service.state.as_str() {
                "running" => Check::Ok(format!("service {}: running", service.kind)),
                state => warn(
                    format!("service {}: {state}", service.kind),
                    format!("read its log, {}", service.log),
                ),
            })
            .collect(),
        Err(error) => vec![warn(
            format!("cannot list the services: {error}"),
            "run `prime-radiant service list` to see the error",
        )],
    }
}

/// The skill's state for each harness this machine has that reads skills.
fn skills(detected: &[Detected], home: &Path) -> Vec<Check> {
    detected
        .iter()
        .filter_map(|found| Some((found.source, skill::file(found.source, home)?)))
        .map(|(source, file)| {
            let name = source.as_str();
            match skill::look(&file) {
                Ok(Present::Ours { current: true }) => {
                    Check::Ok(format!("skill: installed for {name}"))
                }
                Ok(Present::Foreign { current: true }) => Check::Ok(format!(
                    "skill: installed for {name}, in a copy skill install did not write"
                )),
                Ok(Present::Linked {
                    target,
                    current: true,
                    ..
                }) => Check::Ok(format!(
                    "skill: installed for {name}, through a link to {}",
                    target.display()
                )),
                Ok(Present::Ours { current: false }) => warn(
                    format!("skill: the copy for {name} is not this binary's"),
                    format!("prime-radiant skill install --harness {name}"),
                ),
                Ok(Present::Foreign { current: false }) => warn(
                    format!(
                        "skill: {} was not written by skill install, and it differs from this binary's skill",
                        file.display()
                    ),
                    format!("prime-radiant skill install --harness {name} --force"),
                ),
                Ok(Present::Linked {
                    link,
                    target,
                    current: false,
                }) => warn(
                    format!(
                        "skill: {} is a link to {}, which differs from this binary's skill",
                        link.display(),
                        target.display()
                    ),
                    format!(
                        "update the copy the link points to, or remove the link and run `prime-radiant skill install \
                         --harness {name}`"
                    ),
                ),
                Ok(Present::Missing) => warn(
                    format!("skill: not installed for {name}, so its agents don't know Prime Radiant"),
                    format!("prime-radiant skill install --harness {name}"),
                ),
                Err(error) => warn(
                    format!("skill: cannot read {}: {error}", file.display()),
                    "check that it is readable by this user",
                ),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn destination(command: &str) -> Option<(Vec<String>, String, Vec<String>)> {
        ssh_destination(&words(command).expect("the quotes close"))
    }

    fn strings(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| word.to_string()).collect()
    }

    #[test]
    fn the_ssh_destination_follows_its_options() {
        assert_eq!(
            destination(
                r#"ssh -o "ProxyCommand=nc %h %p" -p 2222 -A hub-host prime-radiant hub connect"#
            ),
            Some((
                strings(&["-o", "ProxyCommand=nc %h %p", "-p", "2222", "-A"]),
                "hub-host".to_owned(),
                strings(&["prime-radiant", "hub", "connect"])
            ))
        );
        assert_eq!(
            destination("LC_ALL=C /usr/bin/ssh -Tp22 dev@hub-host 'prime-radiant hub connect'"),
            Some((
                strings(&["-Tp22"]),
                "dev@hub-host".to_owned(),
                strings(&["prime-radiant hub connect"])
            ))
        );
        assert_eq!(
            destination("ssh -Ti ~/.ssh/key hub-host x"),
            Some((
                strings(&["-Ti", "~/.ssh/key"]),
                "hub-host".to_owned(),
                strings(&["x"])
            ))
        );
        assert_eq!(
            destination("prime-radiant hub connect --data /srv/data"),
            None
        );
        assert_eq!(destination("ssh -o"), None);
    }

    fn fixed(command: &str) -> Option<String> {
        by_path(command).map(|(_, fixed)| fixed)
    }

    #[test]
    fn the_fix_names_the_binary_by_path_and_keeps_the_rest_of_the_command_as_written() {
        assert_eq!(
            fixed("ssh hub-host prime-radiant hub connect").as_deref(),
            Some("ssh hub-host .local/bin/prime-radiant hub connect")
        );
        // The host is named prime-radiant too, and ~ and $HOME stay for the shell that runs the command to expand.
        assert_eq!(
            fixed(
                r#"SSH_AUTH_SOCK=$HOME/agent.sock ssh -i ~/.ssh/key -o "ProxyCommand=nc %h %p" prime-radiant prime-radiant hub connect --data ~/data"#
            )
            .as_deref(),
            Some(
                r#"SSH_AUTH_SOCK=$HOME/agent.sock ssh -i ~/.ssh/key -o "ProxyCommand=nc %h %p" prime-radiant .local/bin/prime-radiant hub connect --data ~/data"#
            )
        );
        assert_eq!(
            fixed("ssh hub-host 'prime-radiant hub connect'").as_deref(),
            Some("ssh hub-host '.local/bin/prime-radiant hub connect'")
        );
        for command in [
            "ssh hub-host /opt/bin/prime-radiant hub connect",
            "ssh hub-host prime-radiant-hub connect",
            "ssh hub-host",
            "prime-radiant hub connect",
        ] {
            assert_eq!(fixed(command), None, "{command}");
        }
    }

    #[test]
    fn shell_words_drop_their_quotes() {
        assert_eq!(
            words(r#"a 'b c' "d \"e\" \n" f\ g"#),
            Some(vec![
                "a".into(),
                "b c".into(),
                "d \"e\" \\n".into(),
                "f g".into()
            ])
        );
        assert_eq!(words("a 'b"), None);
    }
}
