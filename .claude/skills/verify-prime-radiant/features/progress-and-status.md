# Progress and status

A collector says what it is doing at any time. It keeps a state file, `collect-<origin>.json` in `$XDG_STATE_HOME/prime-radiant` or `~/.local/state/prime-radiant`, rewritten about once a second and on every phase change. Its phases are connecting, scanning, first sync, live, and reconnecting. The first sync's totals are what the hub lacked when the scan compared each file with the hub's confirmed state, so the percentage reaches 100. `prime-radiant status` reads every state file and asks the hub what it holds from each origin.

## Sub-features

- `progress-first-sync` shows the first sync's percentage, bytes done of the total, rate, time left, and the file being sent, then live with the first sync at 100%.
- `progress-log` prints a whole line on each phase change when stderr is not a terminal, such as a service log. It also prints one every 30 s during the first sync and every 5 min while live.
- `progress-terminal` redraws one line about once a second when stderr is a terminal, and keeps each phase's last line when the phase changes.
- `status-hub` shows each origin's stream count, confirmed bytes, and the hub's last write from it, or `no write since the hub started`.
- `status-reconnecting` shows a running collector that lost the hub as `reconnecting in N s (<reason>)`, and says why the hub part is missing.
- `status-stopped` shows a collector that died as `stopped, last seen X ago`, with what it was doing then.
- `status-json` prints the same as one JSON object.

## How to get to it (user POV)

- Run `prime-radiant collect --origin <name> --hub-socket <sock> --claude-code <folder> ...`, with `--state-file <path>` to keep the state elsewhere. Watch its stderr in a terminal or in the service's log.
- Run `prime-radiant status` with `--hub-socket <sock>`, `--hub-command <command>`, `--data <dir>` (the hub serving that folder), or no flag for the route `config show` prints. Add `--json` for one object, `--watch` to redraw until Ctrl-C, and `--state-dir <dir>` when the state files are elsewhere.

## Driving it with verify.sh

Preconditions:

- Baseline preconditions hold, in a fresh run, because `big-session` adds an eighth session that the other features don't expect. `launch` already started the hub, and no collector has run.
- The collector that `verify.sh` starts keeps its state in `@RUN@/state/collect-verify.json`, so every `status` call below passes `--state-dir @RUN@/state`.

- **Big session.** Run `scripts/verify.sh big-session "$RUN" 2048`. It prints `wrote N bytes`, about 2.15 billion.
- **First sync.** Run `scripts/verify.sh collector "$RUN" start`, wait 6 s, then run `scripts/verify.sh cli "$RUN" status-first-sync status --state-dir @RUN@/state --hub-socket @SOCK@`. It shows `verify: running`, then `first sync N% · X of Y GiB · R MiB/s · about T s left` with N under 100, then `sending -home-dev-generated-project/<id>.jsonl` with that file's bytes and `M of 20 files done`. Y is the size of every fixture file plus the big session, since the hub was empty.
- **Live.** Repeat with label `status-live` until it shows `live · watching 20 files · last sent X ago` and `first sync 100%: 20 files, Y`. The hub part reads `verify: 20 streams, Y, last write X ago`.
- **Log.** Read `RUN/collector.log`. It has, in order, `collect: verify · connecting to the hub`, `collect: connected; the hub has 0 streams from verify`, `collect: verify · scanning`, `collect: verify · first sync 0% · 0.0 of Y GiB`, and `collect: verify · live · watching 20 files`. No line holds a carriage return.
- **JSON.** Run `scripts/verify.sh cli "$RUN" status-json status --state-dir @RUN@/state --hub-socket @SOCK@ --json`. `collectors[0].running` is true, `collectors[0].state.phase.name` is `live`, and `collectors[0].state.phase.first_sync` has `bytes_done` equal to `bytes_total`. `hub.origins[0].confirmed_bytes` equals that total too.
- **Hub down.** Run `scripts/verify.sh hub "$RUN" stop`, wait 3 s, then run `scripts/verify.sh cli "$RUN" status-hub-down status --state-dir @RUN@/state --hub-socket @SOCK@`. It shows `reconnecting in N s (hub unreachable)` or `reconnecting now (hub unreachable)`, `no connection to the hub for X`, and `hub: unavailable: cannot reach the hub: ...`.
- **Hub back.** Run `scripts/verify.sh hub "$RUN" start`, wait 10 s, then run `scripts/verify.sh cli "$RUN" status-hub-back status --state-dir @RUN@/state --hub-socket @SOCK@`. The collector is `live` again with `first sync: the hub lacked nothing`, and the hub part reads `verify: 20 streams, Y, no write since the hub started`.
- **Stopped.** Run `scripts/verify.sh collector "$RUN" crash`, then `scripts/verify.sh cli "$RUN" status-stopped status --state-dir @RUN@/state --hub-command "@BIN@ hub connect --data @DATA@"`. It shows `verify: stopped, last seen X ago` with X at most about 10 s, then `when last seen: live · ...`. The hub part still answers, through the bridge.
- **No hub.** Run `scripts/verify.sh home "$RUN" status-no-hub status --state-dir @RUN@/state`. It exits 0, shows the stopped collector, and ends with `hub: unavailable: no hub to ask: no hub is set: ...`.

## Gotchas

- `verify.sh` sends stderr to a log, so it never shows the terminal line. To see it, run the collector in a terminal against the run's hub with its own origin and `--state-file`, so it doesn't share `collect-verify.json`.
- `status --watch` runs until Ctrl-C, so `verify.sh cli` can't record it. Run it in a terminal, or record `--watch --json` for a few seconds, which prints one JSON line a second.
- A running collector holds a lock on `<state file>.lock`, and `status` calls a collector running only while that lock is held. A state file copied elsewhere therefore shows as stopped.
- A release build sends about 150 MiB/s from one folder to another on the same machine, so a smaller big session can finish before the first `status`. The rate shows after 3 s of sending, and `measuring speed` before that. If the first sync is already live, start over in a fresh run with a larger `big-session`.
- A hub older than this build cannot decode the origins question. `status` then shows `hub: unavailable: the hub does not know this question, so it is older than this client: bad frame: unknown variant origins ...` and exits at once.
