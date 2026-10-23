//! launchd agents in `~/Library/LaunchAgents`, loaded into the user's GUI domain. launchd starts them at login and
//! restarts them whenever they exit.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use super::{Kind, Service, Step, User};
use crate::{Error, Result};

const LABEL_PREFIX: &str = "dev.prime-radiant.";
/// How long bootout may take to let go of a service before a new definition can be loaded under its label.
const BOOTOUT_WAIT: Duration = Duration::from_secs(10);

pub(super) fn folder(user: &User) -> PathBuf {
    user.home.join("Library/LaunchAgents")
}

fn logs(user: &User) -> PathBuf {
    user.home.join("Library/Logs/prime-radiant")
}

fn label(kind: &Kind) -> String {
    match kind {
        Kind::Hub => format!("{LABEL_PREFIX}hub"),
        Kind::Collector(origin) => format!("{LABEL_PREFIX}collect.{origin}"),
    }
}

pub(super) fn file_name(kind: &Kind) -> String {
    format!("{}.plist", label(kind))
}

pub(super) fn kind(file_name: &str) -> Option<Kind> {
    let name = file_name
        .strip_prefix(LABEL_PREFIX)?
        .strip_suffix(".plist")?;
    match name {
        "hub" => Some(Kind::Hub),
        _ => name
            .strip_prefix("collect.")?
            .parse()
            .ok()
            .map(Kind::Collector),
    }
}

pub(super) fn log(kind: &Kind, user: &User) -> PathBuf {
    logs(user).join(format!("{}.log", kind.name()))
}

/// The service in launchd's domain for the user's login session, as `launchctl` names it.
fn target(kind: &Kind, user: &User) -> String {
    format!("gui/{}/{}", user.uid, label(kind))
}

pub(super) fn render(service: &Service, user: &User) -> String {
    let string = |text: &str| format!("<string>{}</string>", xml(text));
    let log = log(&service.kind, user).to_string_lossy().into_owned();
    let mut out = String::from(concat!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n",
        "<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n",
        "<!-- Written by prime-radiant service install, which replaces this file when it runs again. -->\n",
        "<plist version=\"1.0\">\n",
        "<dict>\n",
    ));
    out += &format!("\t<key>Label</key>\n\t{}\n", string(&label(&service.kind)));
    out += "\t<key>ProgramArguments</key>\n\t<array>\n";
    for word in service.command() {
        out += &format!("\t\t{}\n", string(word));
    }
    out += "\t</array>\n";
    out += "\t<key>RunAtLoad</key>\n\t<true/>\n";
    out += "\t<key>KeepAlive</key>\n\t<true/>\n";
    for key in ["StandardOutPath", "StandardErrorPath"] {
        out += &format!("\t<key>{key}</key>\n\t{}\n", string(&log));
    }
    out += "</dict>\n</plist>\n";
    out
}

fn xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub(super) fn install(
    kind: &Kind,
    user: &User,
    file: PathBuf,
    contents: String,
    replacing: bool,
) -> Vec<Step> {
    let mut steps = vec![Step::CreateFolder(logs(user))];
    if replacing {
        steps.push(Step::Bootout(target(kind, user)));
    }
    let domain = format!("gui/{}", user.uid);
    let plist = file.to_string_lossy().into_owned();
    steps.push(Step::Write {
        path: file,
        contents,
    });
    steps.push(Step::Run(vec![
        "launchctl".into(),
        "bootstrap".into(),
        domain,
        plist,
    ]));
    steps
}

pub(super) fn uninstall(kind: &Kind, user: &User, file: PathBuf) -> Vec<Step> {
    vec![Step::Bootout(target(kind, user)), Step::Remove(file)]
}

pub(super) fn bootout_command(target: &str) -> String {
    format!("launchctl bootout {target}")
}

/// Unloads the service, which ends its process. `bootout` can return before launchd has let go of the label, and
/// can fail because the service was not loaded, so success is judged by `launchctl print` no longer finding it.
pub(super) fn bootout(target: &str) -> Result<()> {
    let output = Command::new("launchctl")
        .args(["bootout", target])
        .output()
        .map_err(|error| Error::Usage(format!("cannot run launchctl: {error}")))?;
    let deadline = Instant::now() + BOOTOUT_WAIT;
    while print(target).is_some() {
        if Instant::now() >= deadline {
            return Err(Error::Usage(format!(
                "launchd still had {target} {} s after `{}`: {}",
                BOOTOUT_WAIT.as_secs(),
                bootout_command(target),
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

/// What `launchctl print` says about a loaded service, or `None` if launchd does not have it.
fn print(target: &str) -> Option<String> {
    Command::new("launchctl")
        .args(["print", target])
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
}

pub(super) fn status(kind: &Kind, user: &User) -> String {
    state(print(&target(kind, user)).as_deref())
}

/// The job's state from `launchctl print`, whose first `state = ` line is the service's own.
fn state(print: Option<&str>) -> String {
    let Some(print) = print else {
        return "not loaded".to_owned();
    };
    print
        .lines()
        .find_map(|line| line.trim().strip_prefix("state = "))
        .unwrap_or("unknown")
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn print_output_becomes_a_list_state() {
        let running = "gui/501/dev.prime-radiant.hub = {\n\tactive count = 1\n\tstate = running\n\tpid = 4242\n}\n";
        assert_eq!(state(Some(running)), "running");
        let stopped =
            "gui/501/dev.prime-radiant.hub = {\n\tstate = not running\n\tlast exit code = 1\n}\n";
        assert_eq!(state(Some(stopped)), "not running");
        assert_eq!(state(None), "not loaded");
    }
}
