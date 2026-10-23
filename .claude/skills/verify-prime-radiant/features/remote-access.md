# Remote access

On a machine without the hub, the CLI reaches it through any command whose stdin and stdout connect to `prime-radiant hub connect --data <dir>` on the hub machine, usually over SSH. `prime-radiant config set hub-command` saves that command for every call in every shell, and the `PRIME_RADIANT_HUB` variable sets it for the shells that have it.

## Sub-features

- `remote-flag` answers `find`, `brief`, `read`, and `grep` through `--hub-command`.
- `remote-env` answers the same calls when `PRIME_RADIANT_HUB` holds the command and no flag is given.
- `remote-missing` tells the user when no flag, variable, config file, or local hub names a hub, and how to save one.

## How to get to it (user POV)

- Run `prime-radiant find <words> --hub-command "ssh <host> prime-radiant hub connect --data <dir>"`.
- Set `export PRIME_RADIANT_HUB="ssh <host> prime-radiant hub connect --data <dir>"`, then run `prime-radiant find <words>`.

## Driving it with verify.sh

Preconditions:

- Baseline preconditions hold, and the collector has synced. The bridge runs locally, standing in for SSH.

- **Flag.** Run `scripts/verify.sh cli "$RUN" remote-find find 33831ab7 --hub-command "@BIN@ hub connect --data @DATA@" --limit 1`. The first hit is `Add a JSON output flag`.
- **Variable.** Run `PRIME_RADIANT_HUB="<BIN> hub connect --data <DATA>" scripts/verify.sh cli "$RUN" remote-env brief 33831ab7`. Take BIN and DATA from `RUN/env`, because placeholders are not swapped inside variables. The brief prints.
- **Missing.** With no config file in `RUN/home`, as in a fresh run or after the setup recipe, run `scripts/verify.sh home "$RUN" remote-missing find 33831ab7`. It exits 1, and stderr starts with `error: no hub is set: no flag names one, PRIME_RADIANT_HUB is unset, <RUN>/home/.config/prime-radiant/config.toml names none`. It goes on to name `prime-radiant config set hub-command`, say that non-interactive shells often skip `~/.bashrc`, and point at `prime-radiant doctor`.

## Gotchas

- A local bridge doesn't exercise SSH itself: authentication, `BatchMode`, and keep-alives. Over real SSH, the hub host needs the binary on its PATH or the full path in the command.
