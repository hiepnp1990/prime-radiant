# Find and brief

`find` returns up to 10 candidate sessions, best first, for an ID, an ID prefix of 8 or more characters, an alias, a link, or words about the topic. Filters narrow the candidates by harness, machine, project, and last activity, and `--phrase` requires words together and in order. `brief` summarizes one session: title, key, its parent (`fork of` or `subagent of`), project, origin, activity, first request, last answer, pull requests, changed files, and subagents.

## Sub-features

- `find-id` puts the session named by a full ID, prefix, or alias first.
- `find-words` ranks sessions by words in their messages and title.
- `find-filters` narrows any lookup by `--source`, `--origin`, `--project`, `--since`, and `--until`. An ID outside the filters finds nothing.
- `find-phrase` keeps only sessions holding every `--phrase`, words adjacent and in order. A phrase alone is a valid query.
- `brief-main` summarizes a main session with no parent line.
- `brief-parent` labels a subagent `subagent of: <key>` and a fork `fork of: <key>`. In JSON, `parent` is `{"key", "kind"}` or null.

## How to get to it (user POV)

- Run `prime-radiant find <id, prefix, link, or words> [--limit N] [--json]`.
- Add `--source <harness>`, `--origin <machine>`, `--project <text>`, `--since <YYYY-MM-DD>`, `--until <YYYY-MM-DD>`, or `--phrase "<words>"` to any `find`.
- Run `prime-radiant brief <key, alias, or prefix> [--json]`.
- Each works with `--data <dir>`, `--hub-socket <sock>`, `--hub-command <cmd>`, or `PRIME_RADIANT_HUB`.

## Driving it with verify.sh

Preconditions:

- Baseline preconditions hold, and the collector has synced. Start it with `scripts/verify.sh collector "$RUN" start`, then wait until `stats --hub-socket @SOCK@ --json` shows 7 sessions.

- **By prefix.** Run `scripts/verify.sh cli "$RUN" find-prefix find 33831ab7 --hub-socket @SOCK@`. The first hit is `Add a JSON output flag` with key `claude-code/33831ab7-5c2a-48a4-a8d7-0b5125040f4d`.
- **By words.** Run `scripts/verify.sh cli "$RUN" find-words find "json output flag" --hub-socket @SOCK@ --limit 3`. The first hit is titled `Add a JSON output flag`.
- **Filter by harness.** Run `scripts/verify.sh cli "$RUN" find-source find "json flag" --source codex --hub-socket @SOCK@ --json`. Every hit's key starts with `codex/`. Repeat with `--source claude-code`, and every key starts with `claude-code/`.
- **Filter that excludes an ID.** Run `scripts/verify.sh cli "$RUN" find-id-filtered find 33831ab7 --source codex --hub-socket @SOCK@ --json`. It prints `[]`.
- **Filter by date.** Run `scripts/verify.sh cli "$RUN" find-until find "json flag" --until 2000-01-01 --hub-socket @SOCK@ --json`. It prints `[]`.
- **Phrase.** Run `scripts/verify.sh cli "$RUN" find-phrase find --phrase "output flag" --hub-socket @SOCK@`. The first hit is `Add a JSON output flag`. Then run `scripts/verify.sh cli "$RUN" find-phrase-reversed find --phrase "flag output" --hub-socket @SOCK@ --json`. It prints `[]`.
- **Main brief.** Run `scripts/verify.sh cli "$RUN" brief-main brief 33831ab7 --hub-socket @SOCK@`. It shows `first request:`, `last answer:`, and a `subagents` entry `Find the list rendering code`, with no parent line.
- **Subagent brief.** Run `scripts/verify.sh cli "$RUN" brief-subagent brief claude-code/33831ab7-5c2a-48a4-a8d7-0b5125040f4d/agent-a75ce3543dd257a2d --hub-socket @SOCK@`. The third line is `subagent of: claude-code/33831ab7-5c2a-48a4-a8d7-0b5125040f4d`.

## Gotchas

- The fixtures hold no fork. The CLI test `brief_names_a_subagent_or_a_fork_and_the_session_it_belongs_to` covers `fork of`. To drive it by hand, copy the fixture transcript to a new `<uuid>.jsonl` in the same folder, append one user line with that UUID as `sessionId`, and let the collector pick it up.
- Words that appear only in a fork's copied history count toward the session it copied, not the fork.
