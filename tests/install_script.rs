#![cfg(unix)]

use std::fs::{self, Permissions};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use sha2::{Digest, Sha256};
use tempfile::TempDir;

const INSTALL_SH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/install.sh");

/// `uname -s`, `uname -m`, and the release asset the installer must pick for them.
const SUPPORTED: [(&str, &str, &str); 2] = [
    ("Linux", "x86_64", "prime-radiant-linux-x86_64"),
    ("Darwin", "arm64", "prime-radiant-macos-arm64"),
];

/// A fake release folder, a fake `uname`, and an empty home, all in one temp folder.
struct Sandbox {
    temp: TempDir,
}

impl Sandbox {
    /// A release folder like a real one: every asset, each with its own bytes, and a correct SHA256SUMS.
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("temp folder is created");
        let sandbox = Self { temp };
        for dir in [
            sandbox.release(),
            sandbox.shim(),
            sandbox.home(),
            sandbox.tmp(),
        ] {
            fs::create_dir(dir).expect("sandbox folder is created");
        }
        let mut sums = String::new();
        for (_, _, asset) in SUPPORTED {
            let bytes = binary(asset);
            fs::write(sandbox.release().join(asset), &bytes).expect("asset is written");
            sums.push_str(&format!("{}  {asset}\n", sha256_hex(&bytes)));
        }
        sandbox.write_sums(&sums);
        sandbox
    }

    fn release(&self) -> PathBuf {
        self.temp.path().join("release")
    }

    fn shim(&self) -> PathBuf {
        self.temp.path().join("shim")
    }

    fn home(&self) -> PathBuf {
        self.temp.path().join("home")
    }

    fn tmp(&self) -> PathBuf {
        self.temp.path().join("tmp")
    }

    fn default_bin(&self) -> PathBuf {
        self.home().join(".local/bin")
    }

    fn write_sums(&self, sums: &str) {
        fs::write(self.release().join("SHA256SUMS"), sums).expect("SHA256SUMS is written");
    }

    /// `PATH` with the fake tools first, then `extra`, then the real `PATH` for `curl` and the rest.
    fn path_var(&self, extra: &[&Path]) -> String {
        let mut parts = vec![self.shim().display().to_string()];
        parts.extend(extra.iter().map(|path| path.display().to_string()));
        parts.push(std::env::var("PATH").expect("PATH is set"));
        parts.join(":")
    }

    /// `sh install.sh` on a machine whose `uname -s` and `uname -m` print `os` and `arch`,
    /// with only the fake release folder to download from.
    fn command(&self, os: &str, arch: &str) -> Command {
        let uname = self.shim().join("uname");
        let script = format!(
            "#!/bin/sh\ncase $1 in\n-s) echo '{os}' ;;\n-m) echo '{arch}' ;;\n*) exit 1 ;;\nesac\n"
        );
        write_executable(&uname, &script);
        let mut command = Command::new("sh");
        command
            .arg(INSTALL_SH)
            .env_clear()
            .env("PATH", self.path_var(&[]))
            .env("HOME", self.home())
            .env("TMPDIR", self.tmp())
            .env(
                "PRIME_RADIANT_BASE_URL",
                format!("file://{}", self.release().display()),
            );
        command
    }

    /// Puts a fake `curl` first on `PATH` that fails every download and logs its arguments,
    /// one per line, to the returned file.
    fn fail_downloads(&self) -> PathBuf {
        let log = self.temp.path().join("curl.log");
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" >> '{}'\nexit 22\n",
            log.display()
        );
        write_executable(&self.shim().join("curl"), &script);
        log
    }
}

fn write_executable(path: &Path, script: &str) {
    fs::write(path, script).expect("script is written");
    fs::set_permissions(path, Permissions::from_mode(0o755)).expect("script is executable");
}

fn binary(asset: &str) -> Vec<u8> {
    format!("#!/bin/sh\necho {asset}\n").into_bytes()
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn run(command: &mut Command) -> Output {
    command.output().expect("install.sh runs")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .expect("folder is readable")
        .map(|entry| {
            entry
                .expect("entry is readable")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

#[test]
fn installs_the_verified_asset_for_each_supported_platform() {
    for (os, arch, asset) in SUPPORTED {
        let sandbox = Sandbox::new();
        let prefix = sandbox.temp.path().join("prefix");
        let output = run(sandbox.command(os, arch).env("PREFIX", &prefix));
        assert!(
            output.status.success(),
            "{os} {arch} installs: {}",
            stderr(&output)
        );

        let bin = prefix.join("bin");
        let installed = bin.join("prime-radiant");
        assert_eq!(
            fs::read(&installed).expect("binary is installed"),
            binary(asset),
            "{os} {arch} gets {asset}"
        );
        let mode = fs::metadata(&installed)
            .expect("binary has metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o111, 0o111, "installed binary is executable");
        assert_eq!(
            entries(&bin),
            ["prime-radiant"],
            "nothing else is left in the bin folder"
        );
        assert!(
            entries(&sandbox.tmp()).is_empty(),
            "the download folder is removed"
        );
    }
}

#[test]
fn installs_to_local_bin_under_home_and_says_whether_it_is_on_path() {
    let sandbox = Sandbox::new();
    let bin = sandbox.default_bin();

    let off_path = run(&mut sandbox.command("Linux", "x86_64"));
    assert!(off_path.status.success(), "installs: {}", stderr(&off_path));
    assert!(
        bin.join("prime-radiant").is_file(),
        "binary lands in ~/.local/bin"
    );
    assert!(
        stdout(&off_path).contains(&format!("{} is not on your PATH", bin.display())),
        "says the folder is off PATH: {}",
        stdout(&off_path)
    );

    let on_path = run(sandbox
        .command("Linux", "x86_64")
        .env("PATH", sandbox.path_var(&[&bin])));
    assert!(on_path.status.success(), "reinstalls: {}", stderr(&on_path));
    assert!(
        stdout(&on_path).contains(&format!("{} is on your PATH", bin.display())),
        "says the folder is on PATH: {}",
        stdout(&on_path)
    );
}

#[test]
fn refuses_a_download_that_does_not_match_sha256sums() {
    let other_asset = format!(
        "{}  prime-radiant-macos-arm64\n",
        sha256_hex(&binary("prime-radiant-macos-arm64"))
    );
    let wrong_hash = format!(
        "{}  prime-radiant-linux-x86_64\n",
        sha256_hex(b"some other binary")
    );
    for sums in [wrong_hash, other_asset] {
        let sandbox = Sandbox::new();
        sandbox.write_sums(&sums);
        let bin = sandbox.default_bin();
        fs::create_dir_all(&bin).expect("bin folder is created");
        fs::write(bin.join("prime-radiant"), "old binary").expect("old binary is written");

        let output = run(&mut sandbox.command("Linux", "x86_64"));
        assert!(!output.status.success(), "refuses with SHA256SUMS {sums:?}");
        assert!(
            stderr(&output).contains("checksum"),
            "names the checksum: {}",
            stderr(&output)
        );
        assert_eq!(
            fs::read_to_string(bin.join("prime-radiant")).unwrap(),
            "old binary",
            "keeps the old binary"
        );
        assert_eq!(
            entries(&bin),
            ["prime-radiant"],
            "nothing else is left in the bin folder"
        );
    }
}

#[test]
fn refuses_unsupported_platforms() {
    for (os, arch) in [
        ("FreeBSD", "amd64"),
        ("MINGW64_NT-10.0", "x86_64"),
        ("Linux", "aarch64"),
        ("Darwin", "x86_64"),
    ] {
        let sandbox = Sandbox::new();
        let output = run(&mut sandbox.command(os, arch));
        assert!(!output.status.success(), "refuses {os} {arch}");
        let message = stderr(&output);
        assert!(
            message.contains(&format!("{os} {arch}")),
            "names the platform: {message}"
        );
        assert!(
            message.contains("cargo install --git") && message.contains("--root ~/.local"),
            "points to the source install, into the folder this script uses: {message}"
        );
        assert!(
            !sandbox.default_bin().exists(),
            "installs nothing on {os} {arch}"
        );
    }
}

#[test]
fn downloads_the_latest_release_or_the_one_prime_radiant_version_names() {
    let releases = "https://github.com/hiepnp1990/prime-radiant/releases";
    let latest = format!("{releases}/latest/download/prime-radiant-linux-x86_64");
    let tagged = format!("{releases}/download/v0.2.0/prime-radiant-linux-x86_64");
    for (version, url) in [
        (None, &latest),
        (Some("v0.2.0"), &tagged),
        (Some("0.2.0"), &tagged),
    ] {
        let sandbox = Sandbox::new();
        let log = sandbox.fail_downloads();
        let mut command = sandbox.command("Linux", "x86_64");
        command.env_remove("PRIME_RADIANT_BASE_URL");
        if let Some(version) = version {
            command.env("PRIME_RADIANT_VERSION", version);
        }
        let output = run(&mut command);
        assert!(
            !output.status.success(),
            "a failed download stops the install"
        );
        let requested = fs::read_to_string(&log).expect("curl was called");
        assert!(
            requested.lines().any(|line| line == url),
            "PRIME_RADIANT_VERSION={version:?} requests {url}, got {requested:?}"
        );
        assert!(!sandbox.default_bin().exists(), "installs nothing");
    }
}
