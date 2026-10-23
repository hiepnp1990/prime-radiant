# Live sync

The collector watches session folders and sends new bytes to the hub over one open connection. A line written to a running session becomes findable on the hub within seconds. A collector that is killed resumes from the hub's confirmed state with no duplicates.

## Sub-features

- `sync-initial` sends every fixture stream to the hub when the collector starts.
- `sync-append` makes a new line findable through the hub.
- `sync-crash` resumes after SIGKILL, picking up lines written while it was down, with no duplicate segments.

## How to get to it (user POV)

- Run `prime-radiant hub serve --data <dir>` on the always-on machine.
- Run `prime-radiant collect --origin <name> --hub-socket <sock> --claude-code <folder> ...` (or `--hub-command`) on each machine.

## Driving it with verify.sh

Preconditions:

- Baseline preconditions hold. `launch` already started the hub.

- **Start the collector.** Run `scripts/verify.sh collector "$RUN" start`. Then run `scripts/verify.sh cli "$RUN" hub-stats stats --hub-socket @SOCK@ --json` until `counts.sessions` is 7.
- **Append and find.** Run `FIRST=$(scripts/verify.sh token)`, `scripts/verify.sh append "$RUN" "verification token $FIRST"`, then `scripts/verify.sh wait-find "$RUN" "$FIRST" 30`. It prints `findable after X s` with X under 5, and appends that line to `EVIDENCE/freshness.txt`.
- **Crash.** Run `scripts/verify.sh collector "$RUN" crash` (SIGKILL), then `SECOND=$(scripts/verify.sh token)` and `scripts/verify.sh append "$RUN" "written while down $SECOND"`. Run `scripts/verify.sh cli "$RUN" find-while-down find "$SECOND" --hub-socket @SOCK@ --json --limit 1`. No hit quotes `$SECOND`.
- **Resume.** Run `scripts/verify.sh collector "$RUN" start`, then `scripts/verify.sh wait-find "$RUN" "$SECOND" 30`. It is findable.
- **Proof of no duplicates.** Run `scripts/verify.sh cli "$RUN" stats-after-crash stats --hub-socket @SOCK@ --json`. `counts.streams` is unchanged, and `counts.messages` and `counts.segments` each grew by exactly the number of appended lines.

## Gotchas

- Use one fresh `token` per append. `find` matches any query word, so two tokens that share a part, such as `quokka-123` and `wombat-123`, find each other's lines. A crash proof built on them proves nothing.
- `wait-find` measures from the last `append`. After a crash, that includes the downtime.
- The collector waits for 2 s of quiet before sending a file that keeps changing, so a stream of rapid appends is findable about 2 s after the last one.
- The sweep runs every 5 s in these runs (`--sweep-seconds 5`). The default is 300 s.
