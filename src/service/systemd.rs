//! systemd user units in `~/.config/systemd/user`. The user's own systemd instance runs them, so no root access is
//! needed, but without lingering it stops them at logout.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use super::{Kind, Service, Step, User};

const HEADER: &str =
    "# Written by prime-radiant service install, which replaces this file when it runs again.\n";

pub(super) fn folder(user: &User) -> PathBuf {
    user.home.join(".config/systemd/user")
}

pub(super) fn unit(kind: &Kind) -> String {
    format!("prime-radiant-{}.service", kind.name())
}

pub(super) fn kind(file_name: &str) -> Option<Kind> {
    let name = file_name
        .strip_prefix("prime-radiant-")?
        .strip_suffix(".service")?;
    match name {
        "hub" => Some(Kind::Hub),
        _ => name
            .strip_prefix("collect-")?
            .parse()
            .ok()
            .map(Kind::Collector),
    }
}

pub(super) fn render(service: &Service) -> String {
    let mut out = String::from(HEADER);
    out += "[Unit]\n";
    out += &format!("Description=Prime Radiant {}\n", service.kind.describe());
    if let Kind::Collector(_) = service.kind {
        // Ordering only. Without a hub unit on this machine the line does nothing, and a collector retries a hub that is down.
        out += &format!("After={}\n", unit(&Kind::Hub));
    }
    out += "\n[Service]\n";
    let words: Vec<String> = service.command().map(exec_word).collect();
    out += &format!("ExecStart={}\n", words.join(" "));
    out += "Restart=on-failure\n";
    // Five seconds between restarts keeps a service that fails at start under systemd's limit of 5 starts in 10 s,
    // so systemd keeps retrying it instead of giving up.
    out += "RestartSec=5\n";
    if let Some(max) = &service.memory_max {
        out += &format!("MemoryMax={max}\n");
    }
    out += "\n[Install]\nWantedBy=default.target\n";
    out
}

/// One word of an `ExecStart` line. systemd expands `%` specifiers and `$` variables even inside quotes, so both are
/// doubled. A word with anything but plain characters is double-quoted.
fn exec_word(word: &str) -> String {
    let escaped = word.replace('%', "%%").replace('$', "$$");
    let plain = !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=@+,%$".contains(c));
    if plain {
        escaped
    } else {
        format!("\"{}\"", escaped.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

pub(super) fn install(kind: &Kind, file: PathBuf, contents: String) -> Vec<Step> {
    let unit = unit(kind);
    vec![
        Step::Write {
            path: file,
            contents,
        },
        systemctl(&["daemon-reload"]),
        systemctl(&["enable", &unit]),
        // `enable --now` would leave a running service on its old definition. A restart starts it either way.
        systemctl(&["restart", &unit]),
    ]
}

pub(super) fn uninstall(kind: &Kind, file: PathBuf) -> Vec<Step> {
    vec![
        systemctl(&["disable", "--now", &unit(kind)]),
        Step::Remove(file),
        systemctl(&["daemon-reload"]),
    ]
}

fn systemctl(args: &[&str]) -> Step {
    Step::Run(
        ["systemctl", "--user"]
            .iter()
            .chain(args)
            .map(|word| (*word).to_owned())
            .collect(),
    )
}

pub(super) fn log(kind: &Kind) -> String {
    format!("journalctl --user -u {}", unit(kind))
}

pub(super) fn status(kind: &Kind) -> String {
    let output = Command::new("systemctl")
        .args(["--user", "is-active", &unit(kind)])
        .stderr(Stdio::null())
        .output();
    match output {
        Ok(output) => state(&String::from_utf8_lossy(&output.stdout)),
        Err(error) => format!("unknown, because systemctl did not run: {error}"),
    }
}

/// The state `systemctl is-active` prints, in the words `service list` uses.
fn state(is_active: &str) -> String {
    match is_active.trim() {
        "active" | "reloading" => "running",
        "inactive" => "stopped",
        "" => "unknown",
        other => other,
    }
    .to_owned()
}

pub(super) fn linger_warning(user: &User) -> Option<String> {
    let output = Command::new("loginctl")
        .args([
            "show-user",
            &user.uid.to_string(),
            "--property=Linger",
            "--value",
        ])
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|output| output.status.success());
    linger_note(
        output
            .as_ref()
            .map(|output| String::from_utf8_lossy(&output.stdout))
            .as_deref(),
    )
}

/// The warning for what `loginctl show-user --property=Linger --value` printed, or `None` if it failed.
fn linger_note(linger: Option<&str>) -> Option<String> {
    let found = match linger.map(str::trim) {
        Some("yes") => return None,
        Some("no") => "lingering is off for this user",
        _ => "loginctl could not say whether lingering is on for this user",
    };
    Some(format!(
        "{found}, so systemd stops user services at logout and starts them again at the next login. \
         To keep them running, run: loginctl enable-linger $USER"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_active_words_become_list_states() {
        assert_eq!(state("active\n"), "running");
        assert_eq!(state("inactive\n"), "stopped");
        assert_eq!(state("failed\n"), "failed");
        assert_eq!(state("activating\n"), "activating");
        assert_eq!(
            state(""),
            "unknown",
            "systemctl printed nothing, as without a user bus"
        );
    }

    #[test]
    fn only_lingering_on_silences_the_warning() {
        assert_eq!(linger_note(Some("yes\n")), None);
        let off = linger_note(Some("no\n")).expect("lingering off warns");
        assert!(off.starts_with("lingering is off for this user,"), "{off}");
        assert!(off.ends_with("run: loginctl enable-linger $USER"), "{off}");
        let unknown = linger_note(None).expect("a failed loginctl warns");
        assert!(unknown.starts_with("loginctl could not say"), "{unknown}");
    }
}
