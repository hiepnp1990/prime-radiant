//! The user's config file, and the route to the hub that a command takes when no flag names one.
//!
//! A flag comes first. Without one, a command takes `PRIME_RADIANT_HUB`, then the hub in the config file, then the hub
//! socket in the default data folder when that socket exists. The config file is the route that does not depend on how
//! a shell starts: a non-interactive shell, such as an agent's tool shell, often skips `~/.bashrc` and every variable
//! it exports.
//!
//! The file holds one line, `hub_command = "<command>"` or `hub_socket = "<path>"`, in TOML. It is read by a small
//! parser that takes TOML's basic and literal strings and comments, and refuses anything else with the line it
//! stopped at, so a file it cannot read is never misread.

use std::fmt;
use std::fs;
use std::io;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::str::Chars;

use crate::client::HubAddress;
use crate::{Error, Result, hub};

/// A command that reaches the hub, for every command given no flag that names one.
pub const HUB_ENV: &str = "PRIME_RADIANT_HUB";

const HEADER: &str =
    "# Written by prime-radiant config set, which replaces this file when it runs again.\n";

/// Where a route can come from, found from `HOME` and `XDG_CONFIG_HOME`.
pub struct Places {
    pub home: PathBuf,
    /// `$XDG_CONFIG_HOME/prime-radiant/config.toml`, or `~/.config/prime-radiant/config.toml`.
    pub config_file: PathBuf,
}

impl Places {
    /// `None` when `HOME` is not an absolute path.
    pub fn from_env() -> Option<Places> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|home| home.is_absolute())?;
        let config = match std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from) {
            Some(dir) if dir.is_absolute() => dir,
            _ => home.join(".config"),
        };
        Some(Places {
            config_file: config.join("prime-radiant").join("config.toml"),
            home,
        })
    }

    /// The folder `service install hub` serves unless told otherwise.
    pub fn default_data(&self) -> PathBuf {
        self.home.join(".local/share/prime-radiant")
    }

    pub fn default_socket(&self) -> PathBuf {
        hub::socket_path(&self.default_data())
    }
}

/// Where the route in effect came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RouteFrom {
    Environment,
    ConfigFile(PathBuf),
    /// The socket of a hub serving the default data folder on this machine.
    DefaultSocket,
}

impl fmt::Display for RouteFrom {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RouteFrom::Environment => write!(f, "{HUB_ENV}"),
            RouteFrom::ConfigFile(path) => write!(f, "the config file {}", path.display()),
            RouteFrom::DefaultSocket => {
                f.write_str("the default data folder, which has a hub socket")
            }
        }
    }
}

/// The hub a command reaches when no flag names one, and where that came from.
pub struct Route {
    pub hub: HubAddress,
    pub from: RouteFrom,
}

/// The hub a command reaches when no flag names one, or `None` when nothing names one. The config file is read only
/// when the variable is unset.
pub fn hub_route(places: Option<&Places>) -> Result<Option<Route>> {
    if let Some(command) = environment() {
        return Ok(Some(Route {
            hub: HubAddress::Command(command),
            from: RouteFrom::Environment,
        }));
    }
    let Some(places) = places else {
        return Ok(None);
    };
    if let Some(hub) = read(places)?.hub {
        return Ok(Some(Route {
            hub,
            from: RouteFrom::ConfigFile(places.config_file.clone()),
        }));
    }
    let socket = places.default_socket();
    let listening = fs::metadata(&socket).is_ok_and(|meta| meta.file_type().is_socket());
    Ok(listening.then_some(Route {
        hub: HubAddress::Socket(socket),
        from: RouteFrom::DefaultSocket,
    }))
}

/// The hub command in `PRIME_RADIANT_HUB`, unless it is unset or blank.
pub fn environment() -> Option<String> {
    std::env::var(HUB_ENV)
        .ok()
        .filter(|command| !command.trim().is_empty())
}

/// The error of a command that has no hub to ask, with the command's `flags` and every other way to name a hub.
pub fn no_route(places: Option<&Places>, flags: &str) -> Error {
    let (file, socket) = match places {
        Some(places) => (
            places.config_file.display().to_string(),
            places.default_socket().display().to_string(),
        ),
        None => (
            "~/.config/prime-radiant/config.toml".to_owned(),
            "~/.local/share/prime-radiant/hub.sock".to_owned(),
        ),
    };
    Error::Usage(format!(
        "no hub is set: no flag names one, {HUB_ENV} is unset, {file} names none, and there is no hub socket at \
         {socket}. Give {flags}, or save the hub for every shell with \
         `prime-radiant config set hub-command \"<command>\"` or `prime-radiant config set hub-socket <path>`, which \
         write {file}. Non-interactive shells, such as an agent's tool shell, often skip ~/.bashrc, so a \
         {HUB_ENV} exported there never reaches them. `prime-radiant doctor` checks the setup."
    ))
}

/// What the config file sets.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Config {
    pub hub: Option<HubAddress>,
}

/// The config file's settings. A missing file sets nothing.
pub fn read(places: &Places) -> Result<Config> {
    let file = &places.config_file;
    let text = match fs::read_to_string(file) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(error) => {
            return Err(Error::Usage(format!(
                "cannot read {}: {error}",
                file.display()
            )));
        }
    };
    parse(&text, &places.home).map_err(|why| Error::Usage(format!("{}:{why}", file.display())))
}

/// Writes the config file in place of any earlier one, or removes it when it would set nothing.
pub fn write(places: &Places, config: &Config) -> Result<()> {
    let file = &places.config_file;
    let line = match &config.hub {
        None => {
            return match fs::remove_file(file) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error.into()),
                _ => Ok(()),
            };
        }
        Some(HubAddress::Command(command)) => format!("hub_command = {}\n", quote(command)),
        Some(HubAddress::Socket(socket)) => {
            let text = socket
                .to_str()
                .ok_or_else(|| Error::Usage(format!("{} is not UTF-8", socket.display())))?;
            format!("hub_socket = {}\n", quote(text))
        }
    };
    crate::service::write_replacing(file, &format!("{HEADER}{line}"))?;
    Ok(())
}

/// The file's settings, or the line number and what is wrong with it. `home` expands a socket path that starts with
/// `~/`.
fn parse(text: &str, home: &Path) -> std::result::Result<Config, String> {
    let mut config = Config::default();
    for (i, line) in text.lines().enumerate() {
        let at = |why: &str| {
            format!(
                "{}: {why}. The file holds one line, hub_command = \"<command>\" or hub_socket = \"<path>\"",
                i + 1
            )
        };
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| at("expected a setting = \"value\""))?;
        let value = string(value.trim()).map_err(at)?;
        let hub = match key.trim() {
            "hub_command" if value.trim().is_empty() => return Err(at("hub_command is empty")),
            "hub_command" => HubAddress::Command(value),
            "hub_socket" => HubAddress::Socket(socket_path(&value, home).map_err(at)?),
            other => return Err(at(&format!("{other} is not a setting"))),
        };
        if config.hub.is_some() {
            return Err(at("the file names a hub twice"));
        }
        config.hub = Some(hub);
    }
    Ok(config)
}

fn socket_path(text: &str, home: &Path) -> std::result::Result<PathBuf, &'static str> {
    let path = match text.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None => PathBuf::from(text),
    };
    if path.is_absolute() {
        Ok(path)
    } else {
        Err("hub_socket is not an absolute path")
    }
}

/// A TOML basic string, `"..."`, or literal string, `'...'`, then nothing but a comment.
fn string(text: &str) -> std::result::Result<String, &'static str> {
    let mut chars = text.chars();
    let mut value = String::new();
    match chars.next() {
        Some('\'') => loop {
            match chars.next() {
                Some('\'') => break,
                Some(c) => value.push(c),
                None => return Err("the string has no closing quote"),
            }
        },
        Some('"') => loop {
            match chars.next() {
                Some('"') => break,
                Some('\\') => value.push(escape(&mut chars)?),
                Some(c) => value.push(c),
                None => return Err("the string has no closing quote"),
            }
        },
        _ => return Err("the value is not a quoted string"),
    }
    let rest = chars.as_str().trim_start();
    if rest.is_empty() || rest.starts_with('#') {
        Ok(value)
    } else {
        Err("there is more after the string")
    }
}

fn escape(chars: &mut Chars) -> std::result::Result<char, &'static str> {
    let digits = match chars.next() {
        Some('b') => return Ok('\u{8}'),
        Some('t') => return Ok('\t'),
        Some('n') => return Ok('\n'),
        Some('f') => return Ok('\u{c}'),
        Some('r') => return Ok('\r'),
        Some('"') => return Ok('"'),
        Some('\\') => return Ok('\\'),
        Some('u') => 4,
        Some('U') => 8,
        _ => return Err("the string has an escape TOML does not define"),
    };
    let hex: String = chars.by_ref().take(digits).collect();
    u32::from_str_radix(&hex, 16)
        .ok()
        .filter(|_| hex.len() == digits)
        .and_then(char::from_u32)
        .ok_or("the string has a bad \\u escape")
}

/// `text` as a TOML basic string.
fn quote(text: &str) -> String {
    let mut out = String::from('"');
    for c in text.chars() {
        match c {
            '"' => out += "\\\"",
            '\\' => out += "\\\\",
            '\t' => out += "\\t",
            '\n' => out += "\\n",
            c if c.is_control() => out += &format!("\\u{:04X}", c as u32),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(text: &str) -> std::result::Result<Option<HubAddress>, String> {
        parse(text, Path::new("/home/dev")).map(|config| config.hub)
    }

    #[test]
    fn a_written_command_reads_back_the_same() {
        let command = r#"ssh -o "ProxyCommand=nc %h %p" hub-host 'prime-radiant' hub connect --data C:\data	ü"#;
        let line = format!("hub_command = {}", quote(command));
        assert_eq!(parsed(&line), Ok(Some(HubAddress::Command(command.into()))));
    }

    #[test]
    fn literal_strings_comments_and_a_home_socket_read_as_toml_does() {
        let text = "# the hub\n\nhub_socket = '~/data/hub.sock'  # on this machine\n";
        assert_eq!(
            parsed(text),
            Ok(Some(HubAddress::Socket("/home/dev/data/hub.sock".into())))
        );
        assert_eq!(
            parsed(r#"hub_command = "a \"b\" \u00fc""#),
            Ok(Some(HubAddress::Command("a \"b\" ü".into())))
        );
        assert_eq!(parsed("# nothing yet\n"), Ok(None));
    }

    #[test]
    fn a_file_it_cannot_read_exactly_is_refused_with_its_line() {
        for (text, line) in [
            ("hub = \"ssh hub-host\"", "1: hub is not a setting"),
            (
                "\nhub_command = ssh hub-host",
                "2: the value is not a quoted string",
            ),
            (
                "hub_command = \"\"\"ssh\"\"\"",
                "1: there is more after the string",
            ),
            (
                "hub_command = \"ssh\nhub_socket = \"/a\"",
                "1: the string has no closing quote",
            ),
            (
                "hub_socket = \"relative/hub.sock\"",
                "1: hub_socket is not an absolute path",
            ),
            (
                "hub_command = \"a\"\nhub_socket = \"/a\"",
                "2: the file names a hub twice",
            ),
            ("hub_command = \"  \"", "1: hub_command is empty"),
            ("[hub]", "1: expected a setting"),
            (
                "hub_command = \"\\x41\"",
                "1: the string has an escape TOML does not define",
            ),
        ] {
            let error = parsed(text).expect_err(text);
            assert!(error.starts_with(line), "{text:?} gives {error:?}");
        }
    }
}
