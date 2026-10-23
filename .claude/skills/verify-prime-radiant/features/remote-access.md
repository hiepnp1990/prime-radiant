# Remote access

On a machine without the hub, the CLI reaches it through any command whose stdin and stdout connect to `prime-radiant hub connect --data <dir>` on the hub machine, usually over SSH. The `PRIME_RADIANT_HUB` variable sets that command once for every call.

## Sub-features

- `remote-flag` answers `find`, `brief`, `read`, and `grep` through `--hub-command`.
- `remote-env` answers the same calls when `PRIME_RADIANT_HUB` holds the command and no flag is given.
- `remote-missing` tells the user when no flag is given and the variable is unset.

## How to get to it (user POV)

- Run `prime-radiant find <words> --hub-command "ssh <host> prime-radiant hub connect --data <dir>"`.
- Set `export PRIME_RADIANT_HUB="ssh <host> prime-radiant hub connect --data <dir>"`, then run `prime-radiant find <words>`.

## Driving it with verify.sh

Preconditions:

- Baseline preconditions hold, and the collector has synced. The bridge runs locally, standing in for SSH.

- **Flag.** Run `scripts/verify.sh cli "$RUN" remote-find find 33831ab7 --hub-command "@BIN@ hub connect --data @DATA@" --limit 1`. The first hit is `Add a JSON output flag`.
- **Variable.** Run `PRIME_RADIANT_HUB="<BIN> hub connect --data <DATA>" scripts/verify.sh cli "$RUN" remote-env brief 33831ab7`. Take BIN and DATA from `RUN/env`, because placeholders are not swapped inside variables. The brief prints.
- **Missing.** Run `env -u PRIME_RADIANT_HUB scripts/verify.sh cli "$RUN" remote-missing find 33831ab7`. It exits 1, and stderr reads `give --data, --hub-socket, or --hub-command, or set PRIME_RADIANT_HUB to a command that reaches the hub`.

## Gotchas

- A local bridge doesn't exercise SSH itself: authentication, `BatchMode`, and keep-alives. Over real SSH, the hub host needs the binary on its PATH or the full path in the command.
