# Import and stats

`import` copies Claude Code, Codex, Grok, and T3 Code session files into a data folder's raw layer and indexes them. `stats` counts what the folder holds. Importing the same files again changes nothing.

## Sub-features

- `import-sources` reads each source flag (`--claude-code`, `--codex`, `--grok`, `--t3-code`) and prints one line per source.
- `import-idempotent` makes a second import of unchanged files append 0 bytes, store 0 versions, and index 0 messages.
- `stats-json` prints `counts`, `streams`, and `sessions`.

## How to get to it (user POV)

- Run `prime-radiant import --data <dir> --origin <name> --claude-code <folder> ...`.
- Run `prime-radiant stats --data <dir> --json`.

## Driving it with verify.sh

Preconditions:

- Baseline preconditions hold. Use `@RUN@/local` as the data folder, not `@DATA@`, because the hub owns `@DATA@`.

- **First import.** Run `scripts/verify.sh cli "$RUN" import-first import --data @RUN@/local --origin verify --claude-code @SESSIONS@/claude-code --codex @SESSIONS@/codex --grok @SESSIONS@/grok`. Exit 0. One `scanned` line per source with bytes appended, then `indexed N messages from M streams` with N > 0.
- **Counts.** Run `scripts/verify.sh cli "$RUN" stats-first stats --data @RUN@/local --json`. `counts.sessions` is 7 and `counts.messages` is 48 for the unmodified fixtures.
- **Second import.** Repeat the import with label `import-second`. Every source line says `0 bytes appended, 0 versions stored`, then `indexed 0 messages from 0 streams`.
- **Proof.** Run `stats` again with label `stats-second`. Its JSON is identical to `stats-first`.

## Gotchas

- There is no T3 Code fixture on disk. The T3 tests build their database in code. `--t3-code` against the user's real database is read-only and needs the user's go-ahead.
- `import` refuses a data folder a running hub holds. It exits 1 with `another process is writing <dir>; is a hub serving it?`.
