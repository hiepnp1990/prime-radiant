use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use prime_radiant::client::HubAddress;
use prime_radiant::raw::Source;
use prime_radiant::service::{Kind, Manager, MemoryMax, Origin, Service, Spec, User};

const BIN: &str = env!("CARGO_BIN_EXE_prime-radiant");

fn linux() -> User {
    User {
        home: "/home/dev".into(),
        uid: 1000,
    }
}

fn mac() -> User {
    User {
        home: "/Users/dev".into(),
        uid: 501,
    }
}

fn origin(name: &str) -> Origin {
    name.parse().expect("the origin is valid")
}

fn hub(user: &User) -> Service {
    Service::new(
        Spec::Hub {
            data: "~/prime radiant".into(),
        },
        Path::new("~/.local/bin/prime-radiant"),
        Some("2G".parse().expect("the size is valid")),
        &user.home,
    )
    .expect("the hub service is built")
}

fn collector(user: &User, sources: Vec<(Source, PathBuf)>, hub: HubAddress) -> Service {
    let spec = Spec::Collector {
        origin: origin("example"),
        sources,
        hub,
    };
    Service::new(
        spec,
        Path::new("~/.local/bin/prime-radiant"),
        None,
        &user.home,
    )
    .expect("the collector service is built")
}

#[test]
fn the_hub_unit_quotes_a_path_with_a_space_and_caps_memory() {
    let expected = r#"# Written by prime-radiant service install, which replaces this file when it runs again.
[Unit]
Description=Prime Radiant hub

[Service]
ExecStart=/home/dev/.local/bin/prime-radiant hub serve --data "/home/dev/prime radiant"
Restart=on-failure
RestartSec=5
MemoryMax=2G

[Install]
WantedBy=default.target
"#;
    assert_eq!(Manager::Systemd.render(&hub(&linux()), &linux()), expected);
}

#[test]
fn the_collector_unit_starts_after_the_hub_and_escapes_its_hub_command() {
    let service = collector(
        &linux(),
        vec![
            (Source::ClaudeCode, "~/.claude/projects".into()),
            (Source::Codex, "/home/dev/.codex".into()),
        ],
        HubAddress::Command(
            r#"ssh -o "ProxyCommand=nc %h %p" hub-host prime-radiant hub connect --data $HOME/data"#.into(),
        ),
    );
    let expected = r#"# Written by prime-radiant service install, which replaces this file when it runs again.
[Unit]
Description=Prime Radiant collector for example
After=prime-radiant-hub.service

[Service]
ExecStart=/home/dev/.local/bin/prime-radiant collect --origin example --claude-code /home/dev/.claude/projects --codex /home/dev/.codex --hub-command "ssh -o \"ProxyCommand=nc %%h %%p\" hub-host prime-radiant hub connect --data $$HOME/data"
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
"#;
    assert_eq!(Manager::Systemd.render(&service, &linux()), expected);
}

#[test]
fn the_hub_plist_runs_at_load_keeps_alive_and_logs_to_one_file() {
    let expected = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<!-- Written by prime-radiant service install, which replaces this file when it runs again. -->
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>dev.prime-radiant.hub</string>
	<key>ProgramArguments</key>
	<array>
		<string>/Users/dev/.local/bin/prime-radiant</string>
		<string>hub</string>
		<string>serve</string>
		<string>--data</string>
		<string>/Users/dev/prime radiant</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<true/>
	<key>StandardOutPath</key>
	<string>/Users/dev/Library/Logs/prime-radiant/hub.log</string>
	<key>StandardErrorPath</key>
	<string>/Users/dev/Library/Logs/prime-radiant/hub.log</string>
</dict>
</plist>
"#;
    assert_eq!(Manager::Launchd.render(&hub(&mac()), &mac()), expected);
}

#[test]
fn the_collector_plist_escapes_xml_and_leaves_the_hub_command_to_its_shell() {
    let service = collector(
        &mac(),
        vec![
            (Source::ClaudeCode, "~/.claude/projects".into()),
            (Source::Grok, "/Users/dev/R&D <old>".into()),
        ],
        HubAddress::Command("ssh hub-host prime-radiant hub connect --data ~/data".into()),
    );
    let expected = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<!-- Written by prime-radiant service install, which replaces this file when it runs again. -->
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>dev.prime-radiant.collect.example</string>
	<key>ProgramArguments</key>
	<array>
		<string>/Users/dev/.local/bin/prime-radiant</string>
		<string>collect</string>
		<string>--origin</string>
		<string>example</string>
		<string>--claude-code</string>
		<string>/Users/dev/.claude/projects</string>
		<string>--grok</string>
		<string>/Users/dev/R&amp;D &lt;old&gt;</string>
		<string>--hub-command</string>
		<string>ssh hub-host prime-radiant hub connect --data ~/data</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<true/>
	<key>StandardOutPath</key>
	<string>/Users/dev/Library/Logs/prime-radiant/collect-example.log</string>
	<key>StandardErrorPath</key>
	<string>/Users/dev/Library/Logs/prime-radiant/collect-example.log</string>
</dict>
</plist>
"#;
    assert_eq!(Manager::Launchd.render(&service, &mac()), expected);
}

#[test]
fn paths_with_a_tilde_or_relative_paths_become_absolute() {
    let service = collector(
        &linux(),
        vec![
            (Source::ClaudeCode, "~".into()),
            (Source::Codex, "relative/codex".into()),
        ],
        HubAddress::Socket("~/data/hub.sock".into()),
    );
    let cwd = std::env::current_dir().expect("the test has a working directory");
    let codex = cwd.join("relative/codex").to_string_lossy().into_owned();
    assert_eq!(
        service.program,
        Path::new("/home/dev/.local/bin/prime-radiant")
    );
    assert_eq!(
        service.args,
        [
            "collect",
            "--origin",
            "example",
            "--claude-code",
            "/home/dev",
            "--codex",
            &codex,
            "--hub-socket",
            "/home/dev/data/hub.sock",
        ]
    );
    let other_user = Service::new(
        Spec::Hub {
            data: "~other/data".into(),
        },
        Path::new("/usr/bin/prime-radiant"),
        None,
        Path::new("/home/dev"),
    );
    assert!(
        other_user.is_err(),
        "~other is refused, not taken as a folder named ~other"
    );
}

fn dry_run_steps(plan: &prime_radiant::service::Plan) -> Vec<String> {
    let text = plan.dry_run();
    text.split("\n\n")
        .next()
        .unwrap_or("")
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
fn systemd_plans_reload_enable_and_restart_and_reverse_on_uninstall() {
    let user = linux();
    let service = hub(&user);
    let plan = Manager::Systemd.install(&service, &user, true);
    let unit = "/home/dev/.config/systemd/user/prime-radiant-hub.service";
    assert_eq!(
        plan.dry_run(),
        format!(
            "would write {unit}\n\
             would run systemctl --user daemon-reload\n\
             would run systemctl --user enable prime-radiant-hub.service\n\
             would run systemctl --user restart prime-radiant-hub.service\n\
             log: journalctl --user -u prime-radiant-hub.service\n\
             \n{unit}:\n{}",
            Manager::Systemd.render(&service, &user)
        )
    );
    assert!(plan.warnings.is_empty(), "systemd applies --memory-max");

    let removal = Manager::Systemd.uninstall(&Kind::Collector(origin("example")), &user);
    assert_eq!(
        dry_run_steps(&removal),
        [
            "would run systemctl --user disable --now prime-radiant-collect-example.service",
            "would remove /home/dev/.config/systemd/user/prime-radiant-collect-example.service",
            "would run systemctl --user daemon-reload",
        ]
    );
}

#[test]
fn launchd_plans_bootstrap_and_boot_out_first_when_replacing() {
    let user = mac();
    let service = hub(&user);
    let plist = "/Users/dev/Library/LaunchAgents/dev.prime-radiant.hub.plist";
    let fresh = Manager::Launchd.install(&service, &user, false);
    assert_eq!(
        dry_run_steps(&fresh),
        [
            "would create /Users/dev/Library/Logs/prime-radiant".to_owned(),
            format!("would write {plist}"),
            format!("would run launchctl bootstrap gui/501 {plist}"),
            "log: /Users/dev/Library/Logs/prime-radiant/hub.log".to_owned(),
        ]
    );
    assert_eq!(
        fresh.warnings,
        ["launchd has no memory cap, so --memory-max 2G is not applied"]
    );

    let replacing = Manager::Launchd.install(&service, &user, true);
    assert_eq!(
        dry_run_steps(&replacing),
        [
            "would create /Users/dev/Library/Logs/prime-radiant".to_owned(),
            "would run launchctl bootout gui/501/dev.prime-radiant.hub".to_owned(),
            format!("would write {plist}"),
            format!("would run launchctl bootstrap gui/501 {plist}"),
            "log: /Users/dev/Library/Logs/prime-radiant/hub.log".to_owned(),
        ]
    );

    let removal = Manager::Launchd.uninstall(&Kind::Collector(origin("example")), &user);
    assert_eq!(
        dry_run_steps(&removal),
        [
            "would run launchctl bootout gui/501/dev.prime-radiant.collect.example",
            "would remove /Users/dev/Library/LaunchAgents/dev.prime-radiant.collect.example.plist",
        ]
    );
}

#[test]
fn each_definition_file_maps_back_to_its_service() {
    for manager in [Manager::Systemd, Manager::Launchd] {
        let temp = tempfile::tempdir().expect("temp directory is created");
        let user = User {
            home: temp.path().to_owned(),
            uid: 1000,
        };
        assert_eq!(
            manager
                .installed(&user)
                .expect("a missing folder lists nothing"),
            []
        );
        let kinds = [
            Kind::Collector(origin("my.laptop-2")),
            Kind::Hub,
            Kind::Collector(origin("example")),
        ];
        for kind in &kinds {
            let file = manager.definition(kind, &user);
            fs::create_dir_all(file.parent().expect("the definition has a folder"))
                .expect("the folder is created");
            fs::write(&file, "").expect("the definition is written");
        }
        let folder = manager.definition(&Kind::Hub, &user);
        let folder = folder.parent().expect("the definition has a folder");
        for other in [
            "other.service",
            "com.example.agent.plist",
            ".prime-radiant-hub.service.tmp",
        ] {
            fs::write(folder.join(other), "").expect("an unrelated file is written");
        }
        assert_eq!(
            manager.installed(&user).expect("the folder is read"),
            [
                Kind::Hub,
                Kind::Collector(origin("example")),
                Kind::Collector(origin("my.laptop-2")),
            ],
            "{manager:?}"
        );
    }
}

#[test]
fn origins_and_memory_sizes_are_checked_when_parsed() {
    for good in ["example", "my.laptop-2", "A_1"] {
        assert!(good.parse::<Origin>().is_ok(), "{good}");
    }
    for bad in ["", "two words", "a/b", "user@host"] {
        assert!(bad.parse::<Origin>().is_err(), "{bad:?}");
    }
    for good in ["512M", "2G", "1T", "1073741824"] {
        assert!(good.parse::<MemoryMax>().is_ok(), "{good}");
    }
    for bad in ["", "G", "2GB", "1.5G", "-1", "2g"] {
        assert!(bad.parse::<MemoryMax>().is_err(), "{bad:?}");
    }
}

/// An empty home folder for the binary, with symlinks resolved so it matches the working directory it reports.
fn empty_home() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp directory is created");
    let home = temp
        .path()
        .canonicalize()
        .expect("the temp directory resolves");
    (temp, home)
}

fn service(home: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .arg("service")
        .args(args)
        .env("HOME", home)
        .current_dir(home)
        .output()
        .expect("prime-radiant runs")
}

fn stdout(output: &Output) -> String {
    assert!(
        output.status.success(),
        "exit {:?}, stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout.clone()).expect("stdout is UTF-8")
}

fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in fs::read_dir(dir).expect("the folder is read") {
        let path = entry.expect("the entry is read").path();
        if path.is_dir() {
            found.extend(files_under(&path));
        }
        found.push(path);
    }
    found
}

fn hub_definition(home: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        home.join("Library/LaunchAgents/dev.prime-radiant.hub.plist")
    } else {
        home.join(".config/systemd/user/prime-radiant-hub.service")
    }
}

#[test]
fn a_dry_run_install_prints_the_definition_and_changes_nothing() {
    let (_temp, home) = empty_home();
    let home_text = home.to_string_lossy();

    let out = stdout(&service(
        &home,
        &[
            "install",
            "hub",
            "--data",
            "~/data",
            "--memory-max",
            "512M",
            "--dry-run",
        ],
    ));
    assert!(
        out.contains(&format!(
            "would write {}\n",
            hub_definition(&home).display()
        )),
        "{out}"
    );
    assert!(out.contains(&format!("{home_text}/data")), "{out}");
    assert!(out.contains(BIN), "the service runs this binary: {out}");
    assert!(!out.contains('~'), "no path keeps its ~: {out}");

    let out = stdout(&service(
        &home,
        &[
            "install",
            "collector",
            "--origin",
            "example",
            "--claude-code",
            "projects",
            "--hub-socket",
            "~/data/hub.sock",
            "--dry-run",
        ],
    ));
    assert!(out.contains(&format!("{home_text}/projects")), "{out}");
    assert!(out.contains(&format!("{home_text}/data/hub.sock")), "{out}");

    assert_eq!(
        files_under(&home),
        Vec::<PathBuf>::new(),
        "a dry run writes nothing"
    );
}

#[test]
fn a_dry_run_uninstall_keeps_the_definition_and_a_missing_one_is_already_gone() {
    let (_temp, home) = empty_home();
    let definition = hub_definition(&home);
    fs::create_dir_all(definition.parent().expect("the definition has a folder"))
        .expect("the folder is created");
    fs::write(&definition, "an earlier definition\n").expect("the definition is written");

    let out = stdout(&service(&home, &["uninstall", "hub", "--dry-run"]));
    assert!(
        out.contains(&format!("would remove {}\n", definition.display())),
        "{out}"
    );
    assert_eq!(
        fs::read_to_string(&definition).expect("the definition is still there"),
        "an earlier definition\n"
    );

    let out = stdout(&service(
        &home,
        &["uninstall", "collector", "--origin", "example", "--dry-run"],
    ));
    assert_eq!(out, "no collector for example is installed as a service\n");
}

#[test]
fn list_says_when_there_are_no_services() {
    let (_temp, home) = empty_home();
    let out = stdout(&service(&home, &["list"]));
    assert_eq!(out, "no Prime Radiant services are installed\n");
}

#[test]
fn install_refuses_a_bad_origin_size_or_binary() {
    let (_temp, home) = empty_home();
    let collector = |origin: &str, extra: &[&str]| {
        let mut args = vec![
            "install",
            "collector",
            "--origin",
            origin,
            "--claude-code",
            "projects",
            "--hub-socket",
            "hub.sock",
            "--dry-run",
        ];
        args.extend(extra);
        let output = service(&home, &args);
        assert!(!output.status.success(), "{args:?} is refused");
        String::from_utf8_lossy(&output.stderr).into_owned()
    };
    let stderr = collector("two words", &[]);
    assert!(stderr.contains("may use only letters, digits"), "{stderr}");
    let stderr = collector("example", &["--memory-max", "2GB"]);
    assert!(
        stderr.contains("expected a size such as 512M or 2G"),
        "{stderr}"
    );
    let stderr = collector("example", &["--binary", "~/no-such-binary"]);
    assert!(
        stderr.contains(&format!(
            "there is no file at {}/no-such-binary",
            home.display()
        )),
        "{stderr}"
    );
    assert_eq!(files_under(&home), Vec::<PathBuf>::new());
}
