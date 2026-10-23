# Setup

A new user saves the route to the hub once with `prime-radiant config set`, in `~/.config/prime-radiant/config.toml` (or under `$XDG_CONFIG_HOME`). Every shell reads that file, including an agent's non-interactive tool shell, which never sees a `PRIME_RADIANT_HUB` exported in `~/.bashrc`. A command with no hub flag takes `PRIME_RADIANT_HUB` first, then the config file, then the hub socket in `~/.local/share/prime-radiant` when a hub serves that folder. `collect` and `service install collector` read every harness the machine has when no source flag is given, and say which. `prime-radiant skill install` writes the skill into each harness's skills folder, and `prime-radiant doctor` checks all of it, read-only, with the next step for each warning and failure.

## Sub-features

- `config-route` takes a flag, then `PRIME_RADIANT_HUB`, then the config file, then the default data folder's hub socket.
- `config-show` prints the route in effect, where it came from, and what the config file names.
- `config-unset` removes the config file, which holds only the hub.
- `config-empty-env` reaches the hub from a shell whose environment holds only `HOME` and `PATH`.
- `no-route` names the config file, `config set`, `~/.bashrc`, and `doctor` when nothing names a hub.
- `collect-detect` collects every harness found when no source flag is given, and the hub from the route when no hub flag is given.
- `skill-install` writes the skill, leaves a current copy alone, refuses a file it did not write unless `--force`, and prints a dry run.
- `skill-uninstall` removes only files it wrote, and the emptied folder.
- `skill-link` leaves a skill folder that is a link alone, even with `--force`, and `doctor` compares the linked copy with the binary's skill.
- `doctor-ok` prints only `ok` lines and exits 0 when everything is set up.
- `doctor-fail` prints `fail` with a next step and exits 1 when no hub is set.

## How to get to it (user POV)

- Run `prime-radiant config set hub-command "ssh <hub-host> .local/bin/prime-radiant hub connect"` or `prime-radiant config set hub-socket <path>`, then `prime-radiant config show`. Run `prime-radiant config unset hub` to remove it.
- Run `prime-radiant service install collector --origin <name>` or `prime-radiant collect --origin <name>` with no source or hub flag.
- Run `prime-radiant skill install [--harness claude-code|codex|grok]... [--force] [--dry-run]`, and `prime-radiant skill uninstall` with the same flags.
- Run `prime-radiant doctor`.

## Driving it with verify.sh

Preconditions:

- Baseline preconditions hold, and the collector has synced, as in [Find and brief](./find-and-brief.md).
- Every step runs through `scripts/verify.sh home`, so `HOME` and the XDG folders are in `RUN/home` and `PRIME_RADIANT_HUB` is unset. Placeholders are not swapped inside variables, so write `$RUN` there, and `$RUN/data/hub.sock` for the socket.

- **Stand-in home.** Run `mkdir -p "$RUN/home/.claude" "$RUN/home/.local/state" && cp -R "$RUN/sessions/claude-code" "$RUN/home/.claude/projects" && ln -s "$RUN/state" "$RUN/home/.local/state/prime-radiant"`. The home now has one harness, Claude Code, and `doctor` finds the run's collector state where it looks.
- **No hub.** Run `scripts/verify.sh home "$RUN" setup-no-hub find 33831ab7`. It exits 1. Stderr starts with `error: no hub is set: no flag names one, PRIME_RADIANT_HUB is unset, <RUN>/home/.config/prime-radiant/config.toml names none`, and names `prime-radiant config set hub-command`, `~/.bashrc`, and `prime-radiant doctor`.
- **Doctor without a hub.** Run `scripts/verify.sh home "$RUN" doctor-no-hub doctor`. It exits 1. Stdout has `fail  no hub is set: ...` and, on the next line, `      next: save the hub with ...`. It also has `warn  skill: not installed for claude-code ...` with `      next: prime-radiant skill install --harness claude-code`. Stderr is `error: 1 check failed`.
- **Save the hub.** Run `scripts/verify.sh home "$RUN" config-set config set hub-command "@BIN@ hub connect --data @DATA@"`. It prints `wrote <RUN>/home/.config/prime-radiant/config.toml: the hub command <BIN> hub connect --data <DATA>`. The file holds the line `# Written by prime-radiant config set, which replaces this file when it runs again.` and then `hub_command = "<BIN> hub connect --data <DATA>"`.
- **Show.** Run `scripts/verify.sh home "$RUN" config-show config show`. It prints `hub: the hub command <BIN> hub connect --data <DATA>`, `from: the config file <RUN>/home/.config/prime-radiant/config.toml`, and `config file: <that file>, which names the hub command ...`.
- **Empty environment.** Run `env -i HOME="$RUN/home" PATH=/usr/bin:/bin bash scripts/verify.sh cli "$RUN" find-empty-env find 33831ab7 --limit 1`. The binary sees only `HOME` and `PATH`, like an agent's tool shell that skipped `~/.bashrc`. The first hit is `Add a JSON output flag`.
- **The variable comes first.** Run `HOME="$RUN/home" XDG_CONFIG_HOME="$RUN/home/.config" PRIME_RADIANT_HUB="exit 3" scripts/verify.sh cli "$RUN" env-first config show`. It prints `hub: the hub command exit 3` and `from: PRIME_RADIANT_HUB`. With the same variables, `find 33831ab7` as label `env-first-find` exits 1 with `the hub closed the connection without an answer`.
- **A flag comes first.** With the same variables, run `find 33831ab7 --hub-socket @SOCK@ --limit 1` as label `flag-first`. The first hit is `Add a JSON output flag`.
- **The default socket comes last.** Run `scripts/verify.sh home "$RUN" config-unset config unset hub`. It prints `removed <RUN>/home/.config/prime-radiant/config.toml, which held only the hub`. Run `mkdir -p "$RUN/home/.local/share/prime-radiant" && ln -s "$RUN/data/hub.sock" "$RUN/home/.local/share/prime-radiant/hub.sock"`, standing in for a hub that serves the default data folder. Then `scripts/verify.sh home "$RUN" config-default config show` prints `hub: the hub socket <RUN>/home/.local/share/prime-radiant/hub.sock` and `from: the default data folder, which has a hub socket`, and `scripts/verify.sh home "$RUN" find-default find 33831ab7 --limit 1` finds the session. Remove the link with `rm "$RUN/home/.local/share/prime-radiant/hub.sock"`.
- **Collector with no flags.** Run `scripts/verify.sh home "$RUN" config-set-socket config set hub-socket @SOCK@`, then `scripts/verify.sh home "$RUN" collector-detect service install collector --origin verify --dry-run`. The first line is `no source flag, so reading every harness on this machine: claude-code at <RUN>/home/.claude/projects (1 session)`. The second is `no hub flag, so the hub is the hub socket <SOCK>, from the config file <RUN>/home/.config/prime-radiant/config.toml`. The definition runs `<BIN> collect --origin verify --claude-code <RUN>/home/.claude/projects --hub-socket <SOCK>`.
- **Skill dry run.** Run `scripts/verify.sh home "$RUN" skill-dry-run skill install --dry-run`. It prints `claude-code: would write <RUN>/home/.claude/skills/prime-radiant/SKILL.md`, and that folder does not exist.
- **Skill install.** Run `scripts/verify.sh home "$RUN" skill-install skill install`. It prints `claude-code: wrote <file>`. The file starts with the skill's frontmatter, and the line after it is `<!-- Written by prime-radiant skill install, which replaces this file when it runs again. -->`. Run it again as `skill-again`, and it prints `claude-code: <file> is up to date`.
- **A file it did not write.** Run `mkdir -p "$RUN/home/.grok/skills/prime-radiant" && echo stand-in >"$RUN/home/.grok/skills/prime-radiant/SKILL.md"`, then `scripts/verify.sh home "$RUN" skill-foreign skill install --harness grok`. It exits 1, prints `grok: left <file> alone, because prime-radiant skill install did not write it; --force replaces it`, and the file still holds `stand-in`. Repeat with `--force` as `skill-force`, and it prints `grok: replaced <file>`.
- **Skill uninstall.** Run `scripts/verify.sh home "$RUN" skill-uninstall-dry-run skill uninstall --dry-run`. It prints `would remove` for claude-code and grok and `no skill at` for codex, and both files are still there. Then run `scripts/verify.sh home "$RUN" skill-uninstall-grok skill uninstall --harness grok`. It prints `grok: removed <file>`, and `RUN/home/.grok/skills/prime-radiant` is gone. Run `rm -r "$RUN/home/.grok"`, so that the home has only Claude Code again.
- **A linked skill.** Run `mkdir -p "$RUN/managed/prime-radiant" "$RUN/home/.codex/skills" && cp "$RUN/home/.claude/skills/prime-radiant/SKILL.md" "$RUN/managed/prime-radiant/" && ln -s "$RUN/managed/prime-radiant" "$RUN/home/.codex/skills/prime-radiant"`, standing in for a skill another tool manages. Then `scripts/verify.sh home "$RUN" skill-link skill install --harness codex --force` exits 1 and prints `codex: left <file> alone, because <RUN>/home/.codex/skills/prime-radiant is a link to <RUN>/managed/prime-radiant; remove the link for skill install to write its own copy`. The managed copy is unchanged.
- **Doctor passes.** Run `PATH="$RUN/bin:$PATH" scripts/verify.sh home "$RUN" doctor-ok doctor`. It exits 0, and every line starts with `ok`: the binary and its version, `prime-radiant on PATH is this binary`, `hub: the hub socket <SOCK>, from the config file ...`, `the hub answered in N ms: M streams from 1 origin` (a local socket answers in under a millisecond), `harness claude-code: <RUN>/home/.claude/projects, 1 session`, `harness codex: <RUN>/home/.codex, 0 sessions`, `collector verify: running, live · watching ...`, `services: none installed`, `skill: installed for claude-code`, and `skill: installed for codex, through a link to <RUN>/managed/prime-radiant`.
- **Leave no hub.** Run `scripts/verify.sh home "$RUN" config-cleanup config unset hub`, so that a later recipe's no-hub step finds none in the stand-in home.

## Gotchas

- Never run `config set`, `skill install`, or `skill uninstall` outside `scripts/verify.sh home`. They write the user's real `~/.config/prime-radiant` and harness skills folders.
- `doctor` runs the hub command to reach the hub, as any query does. Over SSH it also runs `ssh -G <host>`, which reads the real `~/.ssh/config`, because ssh finds that folder from the user database, not `HOME`. It only reads. The local bridge here is not SSH, so the connection-sharing line doesn't show. The CLI test `doctor_warns_when_only_the_variable_names_the_hub_and_ssh_shares_no_connection` covers it with a stand-in `ssh`.
- `doctor` lists services from the stand-in home's service folder, which is empty, so it never asks the real `launchctl` or `systemctl` about one.
- The run's collector keeps its state in `RUN/state`, not under `XDG_STATE_HOME`. The link from the stand-in home is what lets `doctor` see it. Without it, `doctor` warns `no collector has run on this machine`.
- A skill folder or file that is a link belongs to whatever made the link, such as a repository of managed skills, so neither `skill install` nor `skill uninstall` writes through it or removes it. A skills folder that is itself a link, such as `~/.claude/skills` kept in a dotfiles repository, is written as usual.
- `skill install` with no `--harness` covers only harnesses the stand-in home has, so codex and grok are skipped until their folders exist. `skill uninstall` with no `--harness` covers every harness that reads skills.
