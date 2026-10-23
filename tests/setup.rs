use std::fs::{self, File};
use std::io::BufReader;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use prime_radiant::protocol::{self, Response, read_request, write_response};
use rusqlite::Connection;
use serde_json::Value;
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_prime-radiant");
const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
const SKILL: &str = include_str!("../skills/prime-radiant/SKILL.md");
const MARKER: &str =
    "<!-- Written by prime-radiant skill install, which replaces this file when it runs again. -->";
const KEY: &str = "claude-code/33831ab7-5c2a-48a4-a8d7-0b5125040f4d";
const CONFIG: &str = ".config/prime-radiant/config.toml";

/// A made-up home folder. Every command runs with an empty environment except `HOME` and a `PATH` that holds this
/// build, so nothing reads the real home, XDG folders, or `PRIME_RADIANT_HUB`.
struct Home {
    _temp: TempDir,
    home: PathBuf,
    /// Folders put before this build's on `PATH`.
    path_first: Vec<PathBuf>,
}

impl Home {
    fn new() -> Self {
        // A Unix socket path must fit in about 100 bytes, and a hub may serve the default data folder in here.
        let temp = tempfile::Builder::new()
            .prefix("pr")
            .tempdir_in("/tmp")
            .expect("temp directory is created");
        let home = temp.path().to_owned();
        Self {
            _temp: temp,
            home,
            path_first: Vec::new(),
        }
    }

    fn join(&self, path: &str) -> PathBuf {
        self.home.join(path)
    }

    fn path_var(&self) -> String {
        let build = Path::new(BIN).parent().expect("the binary is in a folder");
        let mut folders: Vec<String> = self
            .path_first
            .iter()
            .map(|dir| text(dir).to_owned())
            .collect();
        folders.extend([text(build).to_owned(), "/usr/bin".into(), "/bin".into()]);
        folders.join(":")
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(BIN);
        command
            .args(args)
            .env_clear()
            .env("HOME", &self.home)
            .env("PATH", self.path_var())
            .current_dir(&self.home)
            .stdin(Stdio::null());
        command
    }

    fn output(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut command = self.command(args);
        command.envs(env.iter().copied());
        command.output().expect("prime-radiant runs")
    }

    /// The stdout of a command that succeeds.
    fn ok(&self, args: &[&str]) -> String {
        self.ok_with(args, &[])
    }

    fn ok_with(&self, args: &[&str], env: &[(&str, &str)]) -> String {
        let output = self.output(args, env);
        assert!(
            output.status.success(),
            "{args:?} exits 0, not {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("stdout is UTF-8")
    }

    /// The stdout and stderr of a command that fails.
    fn fails(&self, args: &[&str]) -> (String, String) {
        self.fails_with(args, &[])
    }

    fn fails_with(&self, args: &[&str], env: &[(&str, &str)]) -> (String, String) {
        let output = self.output(args, env);
        assert!(!output.status.success(), "{args:?} fails");
        (
            String::from_utf8(output.stdout).expect("stdout is UTF-8"),
            String::from_utf8(output.stderr).expect("stderr is UTF-8"),
        )
    }

    /// The key of the first hit for the fixture session's ID prefix.
    fn first_hit(&self, args: &[&str], env: &[(&str, &str)]) -> String {
        let args = [&["find", "33831ab7", "--json"], args].concat();
        let hits: Value =
            serde_json::from_str(&self.ok_with(&args, env)).expect("find prints JSON");
        hits[0]["key"].as_str().expect("a hit has a key").to_owned()
    }

    /// Copies a fixture folder to `to` under this home, as if the harness had written it.
    fn harness(&self, fixture: &str, to: &str) {
        copy_dir(&Path::new(FIXTURES).join(fixture), &self.join(to));
    }
}

/// A hub serving `data`, filled from the Claude Code fixture.
struct Hub {
    child: Child,
    data: PathBuf,
}

impl Hub {
    fn start(home: &Home, data: PathBuf, import: bool) -> Self {
        if import {
            let fixture = format!("{FIXTURES}/claude-code");
            home.ok(&[
                "import",
                "--data",
                text(&data),
                "--origin",
                "example",
                "--claude-code",
                &fixture,
            ]);
        }
        let child = home
            .command(&["hub", "serve", "--data", text(&data)])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("the hub starts");
        let socket = data.join("hub.sock");
        let deadline = Instant::now() + Duration::from_secs(30);
        while UnixStream::connect(&socket).is_err() {
            assert!(Instant::now() < deadline, "the hub listens within 30 s");
            thread::sleep(Duration::from_millis(20));
        }
        Hub { child, data }
    }

    fn socket(&self) -> PathBuf {
        self.data.join("hub.sock")
    }

    fn bridge(&self) -> String {
        format!("{BIN} hub connect --data {}", text(&self.data))
    }
}

impl Drop for Hub {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A child process that is killed when the test ends.
struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn text(path: &Path) -> &str {
    path.to_str().expect("test paths are UTF-8")
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("folder is created");
    for entry in fs::read_dir(from).expect("fixture folder is readable") {
        let path = entry.expect("entry is readable").path();
        let target = to.join(path.file_name().expect("entry has a name"));
        if path.is_dir() {
            copy_dir(&path, &target);
        } else {
            fs::copy(&path, &target).expect("file is copied");
        }
    }
}

fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !done() {
        assert!(Instant::now() < deadline, "{what} within 30 s");
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn the_route_comes_from_a_flag_then_the_variable_then_the_config_file_then_the_default_socket() {
    let home = Home::new();
    let hub = Hub::start(&home, home.join(".local/share/prime-radiant"), true);
    let config_file = home.join(CONFIG);

    let shown = home.ok(&["config", "show"]);
    assert!(
        shown.starts_with(&format!(
            "hub: the hub socket {}\nfrom: the default data folder",
            hub.socket().display()
        )),
        "{shown}"
    );
    assert_eq!(home.first_hit(&[], &[]), KEY, "the default socket answers");

    let missing = home.join("missing.sock");
    home.ok(&["config", "set", "hub-socket", text(&missing)]);
    let shown = home.ok(&["config", "show"]);
    assert!(
        shown.contains(&format!(
            "from: the config file {}\n",
            config_file.display()
        )),
        "{shown}"
    );
    let (_, stderr) = home.fails(&["find", "33831ab7"]);
    assert!(
        stderr.contains(text(&missing)),
        "the config file comes before the default socket: {stderr}"
    );

    let bridge = hub.bridge();
    let variable = [("PRIME_RADIANT_HUB", bridge.as_str())];
    assert_eq!(
        home.first_hit(&[], &variable),
        KEY,
        "the variable comes before the config file"
    );
    let shown = home.ok_with(&["config", "show"], &variable);
    assert!(
        shown.starts_with(&format!(
            "hub: the hub command {bridge}\nfrom: PRIME_RADIANT_HUB\n"
        )),
        "{shown}"
    );

    let broken = [("PRIME_RADIANT_HUB", "exit 3")];
    home.fails_with(&["find", "33831ab7"], &broken);
    assert_eq!(
        home.first_hit(&["--hub-socket", text(&hub.socket())], &broken),
        KEY,
        "a flag comes before the variable"
    );
}

#[test]
fn the_config_file_reaches_a_non_interactive_shell_with_an_empty_environment() {
    let home = Home::new();
    let hub = Hub::start(&home, home.join("data"), true);
    home.ok(&["config", "set", "hub-command", &hub.bridge()]);
    let agent_shell = || {
        Command::new("/bin/sh")
            .args(["-c", "prime-radiant find 33831ab7 --json"])
            .env_clear()
            .env("HOME", &home.home)
            .env("PATH", home.path_var())
            .stdin(Stdio::null())
            .output()
            .expect("sh runs")
    };

    let output = agent_shell();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let hits: Value = serde_json::from_slice(&output.stdout).expect("find prints JSON");
    assert_eq!(hits[0]["key"], KEY);

    home.ok(&["config", "unset", "hub"]);
    let output = agent_shell();
    assert!(
        !output.status.success(),
        "with no config file, the shell has no hub"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    for words in [
        format!("{}", home.join(CONFIG).display()),
        "prime-radiant config set hub-command".to_owned(),
        "Non-interactive shells, such as an agent's tool shell, often skip ~/.bashrc".to_owned(),
        "prime-radiant doctor".to_owned(),
    ] {
        assert!(
            stderr.contains(&words),
            "the error says {words:?}: {stderr}"
        );
    }
}

#[test]
fn config_set_writes_one_hub_that_show_reads_back_and_unset_removes() {
    let home = Home::new();
    let file = home.join(CONFIG);
    let command =
        r#"ssh -o "ProxyCommand=nc %h %p" hub-host prime-radiant hub connect --data '$HOME/data'"#;

    let out = home.ok(&["config", "set", "hub-command", command]);
    assert!(
        out.starts_with(&format!(
            "wrote {}: the hub command {command}\n",
            file.display()
        )),
        "{out}"
    );
    assert_eq!(
        fs::read_to_string(&file).expect("the file is written"),
        "# Written by prime-radiant config set, which replaces this file when it runs again.\n\
         hub_command = \"ssh -o \\\"ProxyCommand=nc %h %p\\\" hub-host prime-radiant hub connect --data '$HOME/data'\"\n"
    );
    assert_eq!(
        home.ok(&["config", "show"]),
        format!(
            "hub: the hub command {command}\nfrom: the config file {0}\nconfig file: {0}, which names the hub command {command}\n",
            file.display()
        )
    );

    home.ok(&["config", "set", "hub-socket", "relative/hub.sock"]);
    let shown = home.ok(&["config", "show"]);
    let cwd = fs::canonicalize(&home.home).expect("the home folder resolves");
    assert!(
        shown.starts_with(&format!(
            "hub: the hub socket {}\n",
            cwd.join("relative/hub.sock").display()
        )),
        "a relative socket is saved as an absolute one: {shown}"
    );
    home.ok(&["config", "set", "hub-socket", "~/data/hub.sock"]);
    assert!(
        home.ok(&["config", "show"]).starts_with(&format!(
            "hub: the hub socket {}\n",
            home.join("data/hub.sock").display()
        )),
        "a quoted ~ is the home folder"
    );

    assert_eq!(
        home.ok(&["config", "unset", "hub"]),
        format!("removed {}, which held only the hub\n", file.display())
    );
    assert!(!file.exists());
    assert_eq!(
        home.ok(&["config", "show"]),
        format!(
            "hub: none, so a command needs a flag that names one\nconfig file: {}, which does not exist\n",
            file.display()
        )
    );
    assert_eq!(
        home.ok(&["config", "unset", "hub"]),
        format!("no config file at {}\n", file.display())
    );

    let (_, stderr) = home.fails(&["config", "set", "hub-command", " "]);
    assert_eq!(stderr, "error: the hub command is empty\n");
}

#[test]
fn the_config_file_follows_xdg_config_home_and_a_broken_one_names_its_line() {
    let home = Home::new();
    let xdg = home.join("xdg");
    let xdg_file = xdg.join("prime-radiant/config.toml");
    let with_xdg = [("XDG_CONFIG_HOME", text(&xdg))];
    let output = home.output(&["config", "set", "hub-socket", "/srv/hub.sock"], &with_xdg);
    assert!(output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("a shell without it reads ~/.config/prime-radiant/config.toml"),
        "set says where a shell without XDG_CONFIG_HOME looks"
    );
    assert!(xdg_file.is_file());
    assert!(!home.join(CONFIG).exists());
    let shown = home.ok_with(&["config", "show"], &with_xdg);
    assert!(
        shown.contains(&format!("from: the config file {}\n", xdg_file.display())),
        "{shown}"
    );
    assert!(
        home.ok(&["config", "show"]).starts_with("hub: none"),
        "without the variable, ~/.config is read"
    );

    let file = home.join(CONFIG);
    fs::create_dir_all(file.parent().expect("the file has a folder"))
        .expect("the folder is created");
    fs::write(
        &file,
        "# from the issue\nhub = \"ssh hub-host prime-radiant hub connect\"\n",
    )
    .expect("the file is written");
    let (_, stderr) = home.fails(&["find", "33831ab7"]);
    assert!(
        stderr.starts_with(&format!(
            "error: {}:2: hub is not a setting.",
            file.display()
        )),
        "{stderr}"
    );
    assert!(
        home.ok(&["config", "show"])
            .contains("which cannot be read")
    );
    home.ok(&["config", "set", "hub-socket", "/srv/hub.sock"]);
    assert!(
        home.ok(&["config", "show"])
            .starts_with("hub: the hub socket /srv/hub.sock\n"),
        "set replaces it"
    );
}

fn t3_code_database(path: &Path, threads: usize) {
    fs::create_dir_all(path.parent().expect("the database has a folder"))
        .expect("the folder is created");
    let db = Connection::open(path).expect("the database is created");
    db.execute_batch(
        "CREATE TABLE orchestration_v2_projection_threads (thread_id TEXT PRIMARY KEY)",
    )
    .expect("the table is created");
    for n in 0..threads {
        db.execute(
            "INSERT INTO orchestration_v2_projection_threads VALUES (?1)",
            [format!("thread-{n}")],
        )
        .expect("the thread is added");
    }
}

/// Whether a dry run's service definition passes `flag` and `value` to collect: one line of a systemd unit, or two
/// strings of a launchd plist.
fn runs_with(definition: &str, flag: &str, value: &str) -> bool {
    definition.contains(&format!("{flag} {value}"))
        || definition.contains(&format!(
            "<string>{flag}</string>\n\t\t<string>{value}</string>"
        ))
}

#[test]
fn collect_and_its_service_read_every_harness_this_machine_has_when_no_source_is_given() {
    let home = Home::new();
    let none = home.fails(&[
        "service",
        "install",
        "collector",
        "--origin",
        "example",
        "--hub-socket",
        "/srv/hub.sock",
        "--dry-run",
    ]);
    assert!(
        none.1
            .contains("give --claude-code, --codex, --grok, or --t3-code"),
        "with no harness and no flag, it asks for one: {}",
        none.1
    );

    home.harness("claude-code", ".claude/projects");
    home.harness("grok", ".grok");
    t3_code_database(&home.join(".t3/userdata/statev2.sqlite"), 2);
    home.ok(&["config", "set", "hub-socket", "/srv/hub.sock"]);

    let out = home.ok(&[
        "service",
        "install",
        "collector",
        "--origin",
        "example",
        "--dry-run",
    ]);
    let mut lines = out.lines();
    assert_eq!(
        lines.next(),
        Some(
            format!(
                "no source flag, so reading every harness on this machine: claude-code at {} (1 session), grok at {} \
                 (3 sessions), t3-code at {} (2 sessions)",
                home.join(".claude/projects").display(),
                home.join(".grok").display(),
                home.join(".t3/userdata/statev2.sqlite").display()
            )
            .as_str()
        )
    );
    assert_eq!(
        lines.next(),
        Some(
            format!(
                "no hub flag, so the hub is the hub socket /srv/hub.sock, from the config file {}",
                home.join(CONFIG).display()
            )
            .as_str()
        )
    );
    for (flag, value) in [
        ("--claude-code", home.join(".claude/projects")),
        ("--grok", home.join(".grok")),
        ("--t3-code", home.join(".t3/userdata/statev2.sqlite")),
        ("--hub-socket", PathBuf::from("/srv/hub.sock")),
    ] {
        assert!(
            runs_with(&out, flag, text(&value)),
            "the service runs collect {flag} {value:?}: {out}"
        );
    }
    assert!(
        !out.contains("--codex"),
        "codex is not on this machine: {out}"
    );

    let data = home.join("data");
    let (_, stderr) = home.fails(&["import", "--data", text(&data), "--origin", "example"]);
    assert_eq!(
        stderr, "error: give --claude-code, --codex, --grok, or --t3-code\n",
        "import reads only what it is given"
    );
    assert!(!data.exists());

    let codex = home.join("elsewhere/codex");
    let out = home.ok(&[
        "service",
        "install",
        "collector",
        "--origin",
        "example",
        "--codex",
        text(&codex),
        "--hub-socket",
        "/srv/hub.sock",
        "--dry-run",
    ]);
    assert!(
        !out.contains("no source flag") && !out.contains("no hub flag"),
        "{out}"
    );
    assert!(
        out.contains(text(&codex)) && !out.contains("--grok"),
        "the flag alone names the source: {out}"
    );

    let log = home.join("collect.log");
    let _collector = Running(
        home.command(&["collect", "--origin", "example"])
            .stderr(File::create(&log).expect("the log is created"))
            .spawn()
            .expect("collect starts"),
    );
    wait_for("collect names its harnesses and hub", || {
        let log = fs::read_to_string(&log).unwrap_or_default();
        log.contains(
            "collect: no source flag, so reading every harness on this machine: claude-code at ",
        ) && log.contains("collect: no hub flag, so the hub is the hub socket /srv/hub.sock")
    });
}

#[test]
fn skill_install_writes_only_its_own_file_and_uninstall_removes_it() {
    let home = Home::new();
    fs::create_dir_all(home.join(".claude/projects")).expect("Claude Code is set up");
    fs::create_dir_all(home.join(".codex")).expect("Codex is set up");
    let claude = home.join(".claude/skills/prime-radiant/SKILL.md");
    let codex = home.join(".codex/skills/prime-radiant/SKILL.md");
    let grok = home.join(".grok/skills/prime-radiant/SKILL.md");

    assert_eq!(
        home.ok(&["skill", "install", "--dry-run"]),
        format!(
            "claude-code: would write {}\ncodex: would write {}\n",
            claude.display(),
            codex.display()
        ),
        "with no --harness, every harness this machine has"
    );
    assert!(
        !home.join(".claude/skills").exists(),
        "a dry run writes nothing"
    );

    assert_eq!(
        home.ok(&["skill", "install"]),
        format!(
            "claude-code: wrote {}\ncodex: wrote {}\n",
            claude.display(),
            codex.display()
        )
    );
    let installed = fs::read_to_string(&claude).expect("the skill is written");
    assert!(
        installed.starts_with("---\nname: prime-radiant\n"),
        "the frontmatter comes first"
    );
    assert_eq!(
        installed.replace(&format!("{MARKER}\n"), ""),
        SKILL,
        "it is the repository's skill and one marker line"
    );
    assert_eq!(
        home.ok(&["skill", "install", "--harness", "claude-code"]),
        format!("claude-code: {} is up to date\n", claude.display())
    );

    fs::write(
        &claude,
        format!("{installed}An edit of the installed copy.\n"),
    )
    .expect("the copy is edited");
    assert_eq!(
        home.ok(&["skill", "install", "--harness", "claude-code"]),
        format!("claude-code: replaced {}\n", claude.display()),
        "a file it wrote is replaced"
    );
    assert_eq!(
        fs::read_to_string(&claude).expect("the skill reads"),
        installed
    );

    fs::create_dir_all(grok.parent().expect("the skill has a folder"))
        .expect("the folder is created");
    fs::write(&grok, "my own notes\n").expect("the user's file is written");
    let (out, stderr) = home.fails(&["skill", "install", "--harness", "grok"]);
    assert_eq!(
        out,
        format!(
            "grok: left {} alone, because prime-radiant skill install did not write it; --force replaces it\n",
            grok.display()
        )
    );
    assert_eq!(stderr, "error: 1 skill file was left alone\n");
    assert_eq!(
        fs::read_to_string(&grok).expect("the file reads"),
        "my own notes\n"
    );
    assert_eq!(
        home.ok(&["skill", "install", "--harness", "grok", "--force"]),
        format!("grok: replaced {}\n", grok.display())
    );
    assert_eq!(
        fs::read_to_string(&grok).expect("the skill reads"),
        installed
    );

    fs::write(&codex, "my own notes\n").expect("the user's file is written");
    assert_eq!(
        home.ok(&["skill", "uninstall", "--dry-run", "--force"]),
        format!(
            "claude-code: would remove {}\ncodex: would remove {}\ngrok: would remove {}\n",
            claude.display(),
            codex.display(),
            grok.display()
        )
    );
    assert!(
        claude.is_file() && codex.is_file() && grok.is_file(),
        "a dry run removes nothing"
    );
    let (out, _) = home.fails(&["skill", "uninstall"]);
    assert_eq!(
        out,
        format!(
            "claude-code: removed {}\ncodex: left {} alone, because prime-radiant skill install did not write it; \
             --force removes it\ngrok: removed {}\n",
            claude.display(),
            codex.display(),
            grok.display()
        )
    );
    assert!(
        !home.join(".claude/skills/prime-radiant").exists(),
        "the emptied folder goes too"
    );
    assert!(codex.is_file());
    assert_eq!(
        home.ok(&["skill", "uninstall", "--harness", "claude-code"]),
        format!("claude-code: no skill at {}\n", claude.display())
    );

    let managed = home.join("managed/prime-radiant");
    fs::create_dir_all(&managed).expect("the managed folder is created");
    fs::write(managed.join("SKILL.md"), SKILL).expect("the managed copy is written");
    fs::create_dir_all(home.join(".grok/skills")).expect("the folder is created");
    std::os::unix::fs::symlink(&managed, home.join(".grok/skills/prime-radiant"))
        .expect("the link is made");
    let (out, _) = home.fails(&["skill", "install", "--harness", "grok", "--force"]);
    assert_eq!(
        out,
        format!(
            "grok: left {} alone, because {} is a link to {}; remove the link for skill install to write its own copy\n",
            grok.display(),
            home.join(".grok/skills/prime-radiant").display(),
            managed.display()
        ),
        "even --force leaves a linked skill to whatever made the link"
    );
    assert_eq!(
        fs::read_to_string(managed.join("SKILL.md")).expect("the copy reads"),
        SKILL
    );
    let (out, _) = home.fails(&["doctor"]);
    assert!(
        out.contains(&format!(
            "ok    skill: installed for grok, through a link to {}\n",
            managed.display()
        )),
        "doctor finds the linked copy current:\n{out}"
    );

    let output = home.output(&["skill", "install", "--harness", "t3-code"], &[]);
    assert!(
        !output.status.success(),
        "T3 Code reads no skills of its own"
    );
}

fn lines_starting(out: &str, label: &str) -> Vec<String> {
    out.lines()
        .filter(|line| line.starts_with(label))
        .map(str::to_owned)
        .collect()
}

#[test]
fn doctor_passes_against_a_running_hub_with_everything_set_up() {
    let home = Home::new();
    home.harness("claude-code", ".claude/projects");
    let hub = Hub::start(&home, home.join("data"), false);
    home.ok(&["config", "set", "hub-socket", text(&hub.socket())]);
    home.ok(&["skill", "install"]);
    let _collector = Running(
        home.command(&["collect", "--origin", "example"])
            .stderr(Stdio::null())
            .spawn()
            .expect("collect starts"),
    );
    let state = home.join(".local/state/prime-radiant/collect-example.json");
    wait_for("the collector is live", || {
        fs::read(&state)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .is_some_and(|state| state["phase"]["name"] == "live")
    });

    let out = home.ok(&["doctor"]);
    assert_eq!(lines_starting(&out, "warn"), Vec::<String>::new(), "{out}");
    assert_eq!(lines_starting(&out, "fail"), Vec::<String>::new(), "{out}");
    for line in [
        format!("ok    prime-radiant {} at ", env!("CARGO_PKG_VERSION")),
        "ok    prime-radiant on PATH is this binary".to_owned(),
        format!(
            "ok    hub: the hub socket {}, from the config file {}",
            hub.socket().display(),
            home.join(CONFIG).display()
        ),
        "ok    the hub answered in ".to_owned(),
        format!(
            "ok    harness claude-code: {}, 1 session",
            home.join(".claude/projects").display()
        ),
        "ok    collector example: running, live · watching ".to_owned(),
        "ok    services: none installed".to_owned(),
        "ok    skill: installed for claude-code".to_owned(),
    ] {
        assert!(
            out.lines().any(|printed| printed.starts_with(&line)),
            "doctor prints {line:?}:\n{out}"
        );
    }
    assert!(out.contains(" streams from 1 origin\n"), "{out}");
}

#[test]
fn doctor_fails_with_the_next_step_when_no_hub_is_set() {
    let home = Home::new();
    home.harness("claude-code", ".claude/projects");
    let (out, stderr) = home.fails(&["doctor"]);
    let at = out
        .find("fail  no hub is set: PRIME_RADIANT_HUB is unset, ")
        .unwrap_or_else(|| panic!("{out}"));
    let next = out[at..].lines().nth(1).expect("a failure has a next step");
    assert!(
        next.starts_with("      next: save the hub with `prime-radiant config set hub-command \"ssh <hub-host> prime-radiant hub connect --data <dir>\"`"),
        "{next}"
    );
    assert_eq!(stderr, "error: 1 check failed\n");
    assert!(
        out.contains("warn  skill: not installed for claude-code"),
        "{out}"
    );
    assert!(
        out.contains("      next: prime-radiant skill install --harness claude-code\n"),
        "{out}"
    );
    assert!(
        out.contains("warn  no collector has run on this machine"),
        "{out}"
    );
}

#[test]
fn doctor_warns_when_only_the_variable_names_the_hub_and_ssh_shares_no_connection() {
    let mut home = Home::new();
    let tools = home.join("tools");
    fs::create_dir_all(&tools).expect("the folder is created");
    let ssh = tools.join("ssh");
    fs::write(
        &ssh,
        "#!/bin/sh\n\
         # Stands in for ssh: prints ssh-G.txt for -G, and otherwise runs the remote command here.\n\
         for arg in \"$@\"; do [ \"$arg\" = -G ] && exec cat \"$(dirname \"$0\")/ssh-G.txt\"; done\n\
         shift\n\
         exec sh -c \"$*\"\n",
    )
    .expect("the stand-in is written");
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).expect("the stand-in runs");
    fs::write(
        tools.join("ssh-G.txt"),
        "hostname example-hub\ncontrolmaster false\n",
    )
    .expect("settings are written");
    home.path_first.push(tools.clone());
    let hub = Hub::start(&home, home.join("data"), true);
    let command = format!("ssh example-hub {}", hub.bridge());
    let variable = [("PRIME_RADIANT_HUB", command.as_str())];

    let out = home.ok_with(&["doctor"], &variable);
    for line in [
        format!("ok    hub: the hub command {command}, from PRIME_RADIANT_HUB"),
        "warn  only PRIME_RADIANT_HUB names the hub, and non-interactive shells, such as an agent's tool shell, often skip ~/.bashrc".to_owned(),
        format!("      next: prime-radiant config set hub-command '{command}'"),
        "ok    the hub answered in ".to_owned(),
        "warn  SSH connection sharing is off for example-hub, so every query opens a new SSH connection".to_owned(),
        "      next: to reuse one connection, add to ~/.ssh/config: Host example-hub / ControlMaster auto".to_owned(),
    ] {
        assert!(out.lines().any(|printed| printed.starts_with(&line)), "doctor prints {line:?}:\n{out}");
    }

    home.ok(&["config", "set", "hub-command", &command]);
    fs::write(
        tools.join("ssh-G.txt"),
        "hostname example-hub\ncontrolmaster auto\ncontrolpath /tmp/control-%C\n",
    )
    .expect("settings are written");
    let out = home.ok_with(&["doctor"], &variable);
    assert!(
        !out.contains("names the hub, and"),
        "the config file names the same hub:\n{out}"
    );
    assert!(
        out.contains("ok    SSH connection sharing is on for example-hub\n"),
        "{out}"
    );
}

#[test]
fn doctor_tells_a_hub_older_than_this_client_apart() {
    let home = Home::new();
    let socket = home.join("older.sock");
    let listener = UnixListener::bind(&socket).expect("the stand-in hub listens");
    // A hub from before the origins question answers hello, then fails to decode the question.
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut reader = BufReader::new(stream.try_clone().expect("the socket clones"));
            let _ = read_request(&mut reader);
            let hello = Response::Hello {
                protocol: protocol::VERSION,
            };
            let refusal = Response::Error {
                message: "bad frame: unknown variant `origins`".to_owned(),
            };
            let _ = write_response(&mut stream, &hello);
            let _ = write_response(&mut stream, &refusal);
        }
    });
    home.ok(&["config", "set", "hub-socket", text(&socket)]);
    let out = home.ok(&["doctor"]);
    let at = out
        .find("warn  the hub answered in ")
        .unwrap_or_else(|| panic!("doctor warns of an older hub:\n{out}"));
    let mut lines = out[at..].lines();
    let warning = lines.next().expect("the warning");
    assert!(
        warning.contains(
            "but it is older than this client and does not know the origins question: bad frame"
        ),
        "{warning}"
    );
    assert_eq!(
        lines.next(),
        Some(
            "      next: install this version of prime-radiant on the hub's machine, then restart the hub"
        )
    );
}
