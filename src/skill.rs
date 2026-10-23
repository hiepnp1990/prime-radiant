//! The skill that teaches an agent to use Prime Radiant, written into each harness's skills folder by `skill install`.
//!
//! The binary carries the skill, so the installed copy matches the CLI that answers it. A line after the frontmatter
//! marks a file `skill install` wrote, and install and uninstall replace or remove only a file with that line unless
//! told to with `--force`. They never change a skill folder or file that is a link, because the link's target belongs
//! to whatever made the link, such as a repository of managed skills.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::harness::Harness;
use crate::raw::Source;
use crate::{Error, Result};

const SKILL: &str = include_str!("../skills/prime-radiant/SKILL.md");
const MARKER: &str =
    "<!-- Written by prime-radiant skill install, which replaces this file when it runs again. -->";

/// The file `skill install` writes: the skill's frontmatter, the marker line, then the skill's body. Frontmatter has to
/// come first for a harness to read the skill.
pub fn text() -> String {
    let (frontmatter, body) = SKILL
        .strip_prefix("---\n")
        .and_then(|rest| rest.split_once("\n---\n"))
        .expect("the skill starts with frontmatter");
    format!("---\n{frontmatter}\n---\n{MARKER}\n{body}")
}

/// A harness's skill file, or `None` for a harness that reads no skills.
pub fn file(source: Source, home: &Path) -> Option<PathBuf> {
    Harness::of(source)
        .skills(home)
        .map(|folder| folder.join("prime-radiant").join("SKILL.md"))
}

/// What is at a harness's skill file. `current` says whether it holds this binary's skill.
#[derive(Debug, PartialEq, Eq)]
pub enum Present {
    Missing,
    /// A file `skill install` wrote.
    Ours {
        current: bool,
    },
    /// A file `skill install` did not write, such as a copy the user made.
    Foreign {
        current: bool,
    },
    /// The skill's folder or file is a link to `target`.
    Linked {
        link: PathBuf,
        target: PathBuf,
        current: bool,
    },
}

pub fn look(file: &Path) -> io::Result<Present> {
    let folder = file.parent().expect("the skill is in a folder");
    let link = [folder, file]
        .into_iter()
        .find(|path| fs::symlink_metadata(path).is_ok_and(|meta| meta.is_symlink()));
    let text = match fs::read_to_string(file) {
        Err(error) if error.kind() == io::ErrorKind::NotFound && link.is_none() => {
            return Ok(Present::Missing);
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error),
        Ok(text) => text,
    };
    let current = text == self::text() || text == SKILL;
    Ok(match link {
        Some(link) => Present::Linked {
            target: fs::read_link(link)?,
            link: link.to_owned(),
            current,
        },
        None if text.lines().any(|line| line == MARKER) => Present::Ours {
            current: text == self::text(),
        },
        None => Present::Foreign { current },
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Install,
    Uninstall,
}

/// What install or uninstall does with one harness's skill file.
#[derive(Debug, PartialEq, Eq)]
enum Action {
    Write,
    Replace,
    Remove,
    /// The file is this binary's skill already, or there is none to remove.
    Nothing,
    /// The file is not one `skill install` wrote, and `--force` is not given.
    Refuse,
    /// The skill's folder or file is a link, which neither command changes.
    KeepLink,
}

fn action(mode: Mode, present: &Present, force: bool) -> Action {
    match (mode, present) {
        (Mode::Install, Present::Missing) => Action::Write,
        (Mode::Install, Present::Ours { current: true }) => Action::Nothing,
        (Mode::Install, Present::Ours { current: false }) => Action::Replace,
        (Mode::Uninstall, Present::Missing) => Action::Nothing,
        (Mode::Uninstall, Present::Ours { .. }) => Action::Remove,
        (_, Present::Linked { .. }) => Action::KeepLink,
        (_, Present::Foreign { .. }) if !force => Action::Refuse,
        (Mode::Install, Present::Foreign { .. }) => Action::Replace,
        (Mode::Uninstall, Present::Foreign { .. }) => Action::Remove,
    }
}

/// Installs or removes the skill for each harness, printing a line for each. A dry run prints what it would do and
/// changes nothing. It fails after the others are done if it left a file alone that `skill install` did not write.
pub fn run(
    mode: Mode,
    harnesses: &[Source],
    home: &Path,
    force: bool,
    dry_run: bool,
) -> Result<()> {
    let mut refused = 0;
    for &source in harnesses {
        let file = file(source, home).expect("only harnesses that read skills are given");
        let shown = file.display();
        let done = |verb: &str, past: &str| match dry_run {
            true => format!("would {verb} {shown}"),
            false => format!("{past} {shown}"),
        };
        let present = look(&file)?;
        let action = action(mode, &present, force);
        let line = match action {
            Action::Write | Action::Replace => {
                if !dry_run {
                    crate::service::write_replacing(&file, &text())?;
                }
                match action {
                    Action::Write => done("write", "wrote"),
                    _ => done("replace", "replaced"),
                }
            }
            Action::Remove => {
                if !dry_run {
                    fs::remove_file(&file)?;
                    // The folder goes too if the skill was all it held.
                    let _ = fs::remove_dir(file.parent().expect("the skill is in a folder"));
                }
                done("remove", "removed")
            }
            Action::Nothing if mode == Mode::Install => format!("{shown} is up to date"),
            Action::Nothing => format!("no skill at {shown}"),
            Action::Refuse => {
                refused += 1;
                let verb = match mode {
                    Mode::Install => "replaces",
                    Mode::Uninstall => "removes",
                };
                format!(
                    "left {shown} alone, because prime-radiant skill install did not write it; --force {verb} it"
                )
            }
            Action::KeepLink => {
                refused += 1;
                let Present::Linked { link, target, .. } = &present else {
                    unreachable!("only a link is kept as a link")
                };
                format!(
                    "left {shown} alone, because {} is a link to {}; remove the link for skill install to write its own copy",
                    link.display(),
                    target.display()
                )
            }
        };
        println!("{}: {line}", source.as_str());
    }
    match refused {
        0 => Ok(()),
        1 => Err(Error::Usage("1 skill file was left alone".to_owned())),
        n => Err(Error::Usage(format!("{n} skill files were left alone"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_installed_skill_keeps_its_frontmatter_first_and_carries_the_marker() {
        let text = text();
        assert!(text.starts_with("---\nname: prime-radiant\n"), "{text}");
        let after = text
            .split_once("\n---\n")
            .map(|(_, after)| after)
            .expect("the frontmatter closes");
        assert!(after.starts_with(&format!("{MARKER}\n")), "{after}");
        assert_eq!(text.replace(&format!("{MARKER}\n"), ""), SKILL);
    }
}
