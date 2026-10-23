//! Keeps the hub and collectors running as the user's own services: systemd user units on Linux and launchd agents on
//! macOS, with no root access.
//!
//! A [`Service`] says what runs, the same on every platform. Each [`Manager`] renders it as its own file and plans the
//! [`Step`]s that install or remove it. Rendering and planning are pure, so both platforms can be tested on either.
//! Only [`install`], [`uninstall`], and [`list`] look at the machine or change it.

mod launchd;
mod systemd;

use std::fmt;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::str::FromStr;

use crate::client::HubAddress;
use crate::raw::Source;
use crate::{Error, Result};

unsafe extern "C" {
    safe fn getuid() -> u32;
}

/// The user whose services these are.
pub struct User {
    pub home: PathBuf,
    pub uid: u32,
}

impl User {
    pub fn current() -> Result<User> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|home| home.is_absolute())
            .ok_or_else(|| {
                Error::Usage(
                    "HOME is not an absolute path, so there is no place for services".into(),
                )
            })?;
        Ok(User {
            home,
            uid: getuid(),
        })
    }
}

/// Which Prime Radiant process a service runs. A machine has at most one hub and one collector per origin.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Hub,
    Collector(Origin),
}

impl Kind {
    /// The part of the service's file and log names that tells it apart from the others.
    fn name(&self) -> String {
        match self {
            Kind::Hub => "hub".to_owned(),
            Kind::Collector(origin) => format!("collect-{origin}"),
        }
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Kind::Hub => f.write_str("hub"),
            Kind::Collector(origin) => write!(f, "collector for {origin}"),
        }
    }
}

/// A collector's origin. It names the service's files, so it is limited to letters, digits, `.`, `_`, and `-`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Origin(String);

impl FromStr for Origin {
    type Err = String;

    fn from_str(text: &str) -> std::result::Result<Self, String> {
        if !text.is_empty()
            && text
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
        {
            Ok(Origin(text.to_owned()))
        } else {
            Err("a service's origin names its files, so it may use only letters, digits, '.', '_', and '-'".into())
        }
    }
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A memory limit in systemd's notation: a whole number of bytes, or of K, M, G, or T.
#[derive(Clone, Debug)]
pub struct MemoryMax(String);

impl FromStr for MemoryMax {
    type Err = String;

    fn from_str(text: &str) -> std::result::Result<Self, String> {
        let digits = text.strip_suffix(['K', 'M', 'G', 'T']).unwrap_or(text);
        if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
            Ok(MemoryMax(text.to_owned()))
        } else {
            Err("expected a size such as 512M or 2G".into())
        }
    }
}

impl fmt::Display for MemoryMax {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a service runs, as the user gave it. Paths may start with `~` or be relative.
pub enum Spec {
    Hub {
        data: PathBuf,
    },
    Collector {
        origin: Origin,
        sources: Vec<(Source, PathBuf)>,
        hub: HubAddress,
    },
}

/// One service definition. Every path in it is absolute, because a service has no shell to expand `~` and no
/// working directory of the user's choosing.
pub struct Service {
    pub kind: Kind,
    pub program: PathBuf,
    /// The arguments after the program, as `hub serve` or `collect` takes them.
    pub args: Vec<String>,
    pub memory_max: Option<MemoryMax>,
}

impl Service {
    pub fn new(
        spec: Spec,
        program: &Path,
        memory_max: Option<MemoryMax>,
        home: &Path,
    ) -> Result<Service> {
        let path = |path: &Path| absolute(path, home).and_then(|path| text(&path));
        let (kind, args) = match spec {
            Spec::Hub { data } => (
                Kind::Hub,
                vec!["hub".into(), "serve".into(), "--data".into(), path(&data)?],
            ),
            Spec::Collector {
                origin,
                sources,
                hub,
            } => {
                let mut args = vec!["collect".into(), "--origin".into(), origin.to_string()];
                for (source, root) in &sources {
                    args.push(format!("--{}", source.as_str()));
                    args.push(path(root)?);
                }
                match &hub {
                    HubAddress::Socket(socket) => {
                        args.extend(["--hub-socket".into(), path(socket)?])
                    }
                    // The collector runs the command with `sh -c`, which expands its `~` itself.
                    HubAddress::Command(command) => {
                        args.extend(["--hub-command".into(), plain(command)?.to_owned()])
                    }
                }
                (Kind::Collector(origin), args)
            }
        };
        Ok(Service {
            kind,
            program: PathBuf::from(path(program)?),
            args,
            memory_max,
        })
    }

    /// The program and its arguments, the words a service manager runs.
    fn command(&self) -> impl Iterator<Item = &str> {
        let program = self
            .program
            .to_str()
            .expect("Service::new keeps only UTF-8 paths");
        std::iter::once(program).chain(self.args.iter().map(String::as_str))
    }
}

/// `path` with a leading `~` replaced by `home`, then made absolute against the current directory.
fn absolute(path: &Path, home: &Path) -> Result<PathBuf> {
    let mut components = path.components();
    let expanded = match components.next() {
        Some(Component::Normal(first)) if first == "~" => {
            let rest = components.as_path();
            if rest.as_os_str().is_empty() {
                home.to_path_buf()
            } else {
                home.join(rest)
            }
        }
        Some(Component::Normal(first)) if first.to_string_lossy().starts_with('~') => {
            return Err(Error::Usage(format!(
                "{} starts with ~name, which a service cannot expand; give the full path",
                path.display()
            )));
        }
        _ => path.to_path_buf(),
    };
    Ok(std::path::absolute(expanded)?)
}

fn text(path: &Path) -> Result<String> {
    let text = path
        .to_str()
        .ok_or_else(|| Error::Usage(format!("{} is not UTF-8", path.display())))?;
    Ok(plain(text)?.to_owned())
}

fn plain(text: &str) -> Result<&str> {
    if text.chars().any(char::is_control) {
        Err(Error::Usage(format!(
            "{text:?} has a control character, which a service definition cannot hold"
        )))
    } else {
        Ok(text)
    }
}

/// The service manager of the user's platform.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Manager {
    Systemd,
    Launchd,
}

impl Manager {
    pub fn current() -> Result<Manager> {
        if cfg!(target_os = "macos") {
            Ok(Manager::Launchd)
        } else if cfg!(target_os = "linux") {
            Ok(Manager::Systemd)
        } else {
            Err(Error::Usage(
                "services need systemd on Linux or launchd on macOS".into(),
            ))
        }
    }

    /// The folder that holds the user's service definitions.
    fn folder(self, user: &User) -> PathBuf {
        match self {
            Manager::Systemd => systemd::folder(user),
            Manager::Launchd => launchd::folder(user),
        }
    }

    fn file_name(self, kind: &Kind) -> String {
        match self {
            Manager::Systemd => systemd::unit(kind),
            Manager::Launchd => launchd::file_name(kind),
        }
    }

    /// The service a definition file name belongs to, if it is a Prime Radiant service.
    fn kind(self, file_name: &str) -> Option<Kind> {
        match self {
            Manager::Systemd => systemd::kind(file_name),
            Manager::Launchd => launchd::kind(file_name),
        }
    }

    pub fn definition(self, kind: &Kind, user: &User) -> PathBuf {
        self.folder(user).join(self.file_name(kind))
    }

    /// The service's definition file, as its manager reads it.
    pub fn render(self, service: &Service, user: &User) -> String {
        match self {
            Manager::Systemd => systemd::render(service),
            Manager::Launchd => launchd::render(service, user),
        }
    }

    /// Where the service's output goes: a file, or the command that shows it.
    pub fn log(self, kind: &Kind, user: &User) -> String {
        match self {
            Manager::Systemd => systemd::log(kind),
            Manager::Launchd => launchd::log(kind, user).display().to_string(),
        }
    }

    /// The steps that write the service's definition and start it. `replacing` says a definition is already there.
    pub fn install(self, service: &Service, user: &User, replacing: bool) -> Plan {
        let file = self.definition(&service.kind, user);
        let contents = self.render(service, user);
        let (steps, warnings) = match self {
            Manager::Systemd => (systemd::install(&service.kind, file, contents), Vec::new()),
            Manager::Launchd => (
                launchd::install(&service.kind, user, file, contents, replacing),
                service
                    .memory_max
                    .iter()
                    .map(|max| {
                        format!("launchd has no memory cap, so --memory-max {max} is not applied")
                    })
                    .collect(),
            ),
        };
        Plan {
            steps,
            notes: vec![format!("log: {}", self.log(&service.kind, user))],
            warnings,
        }
    }

    /// The steps that stop an installed service and remove its definition, leaving its data and log.
    pub fn uninstall(self, kind: &Kind, user: &User) -> Plan {
        let file = self.definition(kind, user);
        Plan {
            steps: match self {
                Manager::Systemd => systemd::uninstall(kind, file),
                Manager::Launchd => launchd::uninstall(kind, user, file),
            },
            notes: Vec::new(),
            warnings: Vec::new(),
        }
    }

    /// The Prime Radiant services that have a definition in the user's folder, the hub first.
    pub fn installed(self, user: &User) -> Result<Vec<Kind>> {
        let entries = match fs::read_dir(self.folder(user)) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut kinds = Vec::new();
        for entry in entries {
            if let Some(kind) = entry?.file_name().to_str().and_then(|name| self.kind(name)) {
                kinds.push(kind);
            }
        }
        kinds.sort();
        Ok(kinds)
    }

    /// Asks the manager whether the service is running.
    fn status(self, kind: &Kind, user: &User) -> String {
        match self {
            Manager::Systemd => systemd::status(kind),
            Manager::Launchd => launchd::status(kind, user),
        }
    }

    /// A warning that the user's services stop at logout, when the manager can tell.
    fn logout_warning(self, user: &User) -> Option<String> {
        match self {
            Manager::Systemd => systemd::linger_warning(user),
            Manager::Launchd => None,
        }
    }
}

/// What install or uninstall does, in order, and what it tells the user.
pub struct Plan {
    steps: Vec<Step>,
    /// Lines printed after the steps.
    notes: Vec<String>,
    pub warnings: Vec<String>,
}

enum Step {
    CreateFolder(PathBuf),
    /// Write a file in place of any earlier one, through a temporary file in the same folder.
    Write {
        path: PathBuf,
        contents: String,
    },
    Remove(PathBuf),
    /// Run a command, and stop if it fails.
    Run(Vec<String>),
    /// Unload a launchd service, then wait until launchd has let go of it.
    Bootout(String),
}

impl Plan {
    /// The steps as `would ...` lines, the notes, then each file the plan writes.
    pub fn dry_run(&self) -> String {
        let mut out = String::new();
        for step in &self.steps {
            out += &step.line(false);
            out += "\n";
        }
        for note in &self.notes {
            out += note;
            out += "\n";
        }
        for step in &self.steps {
            if let Step::Write { path, contents } = step {
                out += &format!("\n{}:\n{contents}", path.display());
            }
        }
        out
    }

    fn carry_out(&self, dry_run: bool) -> Result<()> {
        if dry_run {
            print!("{}", self.dry_run());
        } else {
            for step in &self.steps {
                step.apply()?;
                println!("{}", step.line(true));
            }
            for note in &self.notes {
                println!("{note}");
            }
        }
        for warning in &self.warnings {
            eprintln!("warning: {warning}");
        }
        Ok(())
    }
}

impl Step {
    fn line(&self, done: bool) -> String {
        let (verb, past, object) = match self {
            Step::CreateFolder(path) => ("create", "created", shell_word(&path.to_string_lossy())),
            Step::Write { path, .. } => ("write", "wrote", shell_word(&path.to_string_lossy())),
            Step::Remove(path) => ("remove", "removed", shell_word(&path.to_string_lossy())),
            Step::Run(command) => ("run", "ran", shell_line(command)),
            Step::Bootout(target) => ("run", "ran", launchd::bootout_command(target)),
        };
        if done {
            format!("{past} {object}")
        } else {
            format!("would {verb} {object}")
        }
    }

    fn apply(&self) -> Result<()> {
        match self {
            Step::CreateFolder(path) => fs::create_dir_all(path)?,
            Step::Write { path, contents } => write_replacing(path, contents)?,
            Step::Remove(path) => match fs::remove_file(path) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error.into()),
                _ => {}
            },
            Step::Run(command) => {
                let (program, args) = command.split_first().expect("a command names its program");
                let status = Command::new(program)
                    .args(args)
                    .status()
                    .map_err(|error| Error::Usage(format!("cannot run {program}: {error}")))?;
                if !status.success() {
                    return Err(Error::Usage(format!(
                        "`{}` failed with {status}",
                        shell_line(command)
                    )));
                }
            }
            Step::Bootout(target) => launchd::bootout(target)?,
        }
        Ok(())
    }
}

/// Writes `contents` to `path` through a temporary file in the same folder, so a reader sees the old file or the new.
pub(crate) fn write_replacing(path: &Path, contents: &str) -> io::Result<()> {
    let folder = path.parent().expect("the file is inside a folder");
    fs::create_dir_all(folder)?;
    let name = path.file_name().expect("the file has a name");
    let temporary = folder.join(format!(".{}.tmp", name.to_string_lossy()));
    fs::write(&temporary, contents)?;
    fs::rename(&temporary, path)
}

/// `text` as one shell word.
pub(crate) fn shell_word(text: &str) -> String {
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

fn shell_line(words: &[String]) -> String {
    words
        .iter()
        .map(|word| shell_word(word))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Everything `service install` takes.
pub struct Install {
    pub spec: Spec,
    /// The binary the service runs instead of this one.
    pub binary: Option<PathBuf>,
    pub memory_max: Option<MemoryMax>,
    pub dry_run: bool,
}

/// Writes the service's definition and starts it, replacing an earlier definition of the same service. A dry run
/// prints the plan and changes nothing.
pub fn install(request: Install) -> Result<()> {
    let manager = Manager::current()?;
    let user = User::current()?;
    let program = match request.binary {
        Some(binary) => binary,
        None => std::env::current_exe()?,
    };
    let service = Service::new(request.spec, &program, request.memory_max, &user.home)?;
    if !service.program.is_file() {
        return Err(Error::Usage(format!(
            "there is no file at {}; give --binary the prime-radiant binary the service should run",
            service.program.display()
        )));
    }
    let replacing = manager.definition(&service.kind, &user).exists();
    let mut plan = manager.install(&service, &user, replacing);
    plan.warnings.extend(manager.logout_warning(&user));
    plan.carry_out(request.dry_run)
}

/// Stops the service and removes its definition. Its data and log stay. A dry run prints the plan and changes nothing.
pub fn uninstall(kind: Kind, dry_run: bool) -> Result<()> {
    let manager = Manager::current()?;
    let user = User::current()?;
    if !manager.definition(&kind, &user).exists() {
        println!("no {kind} is installed as a service");
        return Ok(());
    }
    manager.uninstall(&kind, &user).carry_out(dry_run)
}

/// A Prime Radiant service of this user, as its manager reports it.
pub struct Installed {
    pub kind: Kind,
    /// Such as `running`, `stopped`, or `not loaded`.
    pub state: String,
    pub definition: PathBuf,
    pub log: String,
}

/// Every Prime Radiant service of this user, the hub first.
pub fn installed() -> Result<Vec<Installed>> {
    let manager = Manager::current()?;
    let user = User::current()?;
    Ok(manager
        .installed(&user)?
        .into_iter()
        .map(|kind| Installed {
            state: manager.status(&kind, &user),
            definition: manager.definition(&kind, &user),
            log: manager.log(&kind, &user),
            kind,
        })
        .collect())
}

/// Prints each Prime Radiant service of this user, whether it is running, and where its log is.
pub fn list() -> Result<()> {
    let services = installed()?;
    if services.is_empty() {
        println!("no Prime Radiant services are installed");
    }
    for service in services {
        println!("{}: {}", service.kind, service.state);
        println!("  definition: {}", service.definition.display());
        println!("  log: {}", service.log);
    }
    Ok(())
}
