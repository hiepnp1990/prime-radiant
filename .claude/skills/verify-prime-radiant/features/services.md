# Services

`service install hub` and `service install collector` keep the hub or a collector running as the user's own service, with no sudo. On Linux that is a systemd user unit in `~/.config/systemd/user`, and on macOS a launchd agent in `~/Library/LaunchAgents`. Installing again replaces the definition. `service uninstall` stops the service and removes its definition, and leaves its data and log. `service list` shows each service, whether it runs, and where its log is. `--dry-run` prints the file and the commands, and changes nothing.

## Sub-features

- `service-hub` defines `hub serve --data <dir>` with the data folder made absolute, and caps memory on Linux.
- `service-collector` defines `collect` with the same source and hub flags, every path absolute. On Linux the unit is ordered after `prime-radiant-hub.service`.
- `service-tilde` expands a leading `~`, because a service has no shell to do it.
- `service-dry-run` prints the steps and the file, and writes nothing.
- `service-uninstall` plans the stop, the removal, and the reload, and says so when nothing is installed.
- `service-list` names each installed definition with its state and log.

## How to get to it (user POV)

- Run `prime-radiant service install hub [--data <dir>] [--memory-max <size>] [--dry-run]` on the always-on machine.
- Run `prime-radiant service install collector --origin <name> --claude-code <folder> ... --hub-socket <sock>` (or `--hub-command`) on each machine.
- Run `prime-radiant service uninstall hub` or `prime-radiant service uninstall collector --origin <name>`.
- Run `prime-radiant service list`.

## Driving it with verify.sh

Preconditions:

- Baseline preconditions hold. Every step runs with `HOME` set to an empty folder inside the run, so the files a dry run names are under it, not under the user's home. Run `mkdir "$RUN/home"` first. Placeholders are not swapped inside variables, so write `$RUN` there.

- **Hub.** Run `HOME="$RUN/home" scripts/verify.sh cli "$RUN" service-hub service install hub --data '~/data' --memory-max 2G --dry-run`. Exit 0. On Linux the first line is `would write <RUN>/home/.config/systemd/user/prime-radiant-hub.service`, and the unit has `ExecStart=<BIN> hub serve --data <RUN>/home/data` and `MemoryMax=2G`. On macOS the lines are `would create <RUN>/home/Library/Logs/prime-radiant`, `would write <RUN>/home/Library/LaunchAgents/dev.prime-radiant.hub.plist`, and `would run launchctl bootstrap gui/<uid> <that plist>`. Its `ProgramArguments` are `<BIN>`, `hub`, `serve`, `--data`, `<RUN>/home/data`, and stderr says `warning: launchd has no memory cap, so --memory-max 2G is not applied`.
- **Collector.** Run `HOME="$RUN/home" scripts/verify.sh cli "$RUN" service-collector service install collector --origin verify --claude-code @SESSIONS@/claude-code --codex @SESSIONS@/codex --hub-socket @SOCK@ --dry-run`. The definition runs `<BIN> collect --origin verify --claude-code <SESSIONS>/claude-code --codex <SESSIONS>/codex --hub-socket <SOCK>`. On Linux the unit has `After=prime-radiant-hub.service`.
- **Plist syntax.** On macOS, save the hub's dry run from `<?xml` on to a file and run `plutil -lint` on it. It prints `OK`.
- **Nothing changed.** Run `find "$RUN/home"`. It prints only `<RUN>/home`.
- **Uninstall.** Write a stand-in definition, `<RUN>/home/.config/systemd/user/prime-radiant-hub.service` on Linux or `<RUN>/home/Library/LaunchAgents/dev.prime-radiant.hub.plist` on macOS, holding the word `stand-in`. Run `HOME="$RUN/home" scripts/verify.sh cli "$RUN" service-uninstall service uninstall hub --dry-run`. On Linux it prints `would run systemctl --user disable --now prime-radiant-hub.service`, `would remove <file>`, and `would run systemctl --user daemon-reload`. On macOS it prints `would run launchctl bootout gui/<uid>/dev.prime-radiant.hub` and `would remove <file>`. The file still holds `stand-in`.
- **List.** Run `HOME="$RUN/home" scripts/verify.sh cli "$RUN" service-list service list`. It prints `hub: <state>`, then `definition: <file>` and `log: <log>`. The log is `journalctl --user -u prime-radiant-hub.service` on Linux and `<RUN>/home/Library/Logs/prime-radiant/hub.log` on macOS.
- **Nothing to uninstall.** Run `HOME="$RUN/home" scripts/verify.sh cli "$RUN" service-missing service uninstall collector --origin verify --dry-run`. It prints `no collector for verify is installed as a service`.

## Gotchas

- A real `service install` or `service uninstall`, without `--dry-run`, changes the user's login services. It writes under their real home and runs `systemctl --user` or `launchctl`. Run one only with the user's go-ahead, and never from a verify run: its definition would point at the run's copy of the binary, which `cleanup` deletes.
- A stand-in `HOME` moves only the files. `service list` still asks the real `systemctl --user` or `launchctl` for each state, which is read-only. With no real service by that name, the state is `stopped` or `unknown` on Linux and `not loaded` on macOS.
- A dry run proves the file and the plan, not that the service manager accepts them. `plutil -lint` checks a plist without loading it. On Linux, `systemd-analyze --user verify <file>` checks a unit without starting it, where systemd is installed.
- On Linux, a dry or real install warns on stderr when lingering is off, or when `loginctl` cannot say, because user services stop at logout without it. It prints `loginctl enable-linger $USER` and never runs it.
- On macOS, launchd runs agents only while the user is logged in, and `launchctl bootstrap gui/<uid>` needs that login session. An always-on Mac needs the user logged in after a reboot, for example through automatic login.
