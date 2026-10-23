# Read and grep

`read` prints one page of a session, about 12,000 characters, with tool calls cut to 2,000 characters, tool results to 4,000, and secrets shown as `[redacted]`. Each page ends with the command for the next page. `grep` finds text in one session, ignoring case, and names the page of each match.

## Sub-features

- `read-page` prints a page and a footer naming the next page or saying it is the last.
- `grep-matches` prints the match count and, for each of the first 20, its role, page, and about 200 characters around it.

## How to get to it (user POV)

- Run `prime-radiant read <session> [--page N]`.
- Run `prime-radiant grep <session> <text>`.

## Driving it with verify.sh

Preconditions:

- Baseline preconditions hold, and the collector has synced as in Find and brief.

- **Read.** Run `scripts/verify.sh cli "$RUN" read read 33831ab7 --hub-socket @SOCK@`. The output ends with `page 1 of 1, the last page.`
- **Grep.** Run `scripts/verify.sh cli "$RUN" grep grep 33831ab7 render --hub-socket @SOCK@`. The first line reads `N matches for "render" in claude-code/33831ab7-...` with N > 0, and each match names `page 1`.

## Gotchas

- The fixture session fits on one page. To prove paging, append enough lines with `scripts/verify.sh append` (about 12,000 characters) and check that page 1 names `--page 2`.
- Secret redaction has unit tests in `tests/redact.rs`. The fixtures hold no secret, so an end-to-end proof needs an appended line holding a made-up token.
