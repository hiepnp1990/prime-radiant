---
name: verify-prime-radiant
description: Drive a disposable Prime Radiant instance (CLI, hub, collector) built from this checkout and capture proof that import, find, brief, read, grep, live sync, remote access, and collector progress and status work. Also measures search quality on the user's own sessions. Use before claiming a change to collection, indexing, search, the hub, or the CLI works, or when asked to verify Prime Radiant.
---

# Verify Prime Radiant

Prime Radiant is a CLI. Users touch it through `prime-radiant import|stats|find|brief|read|grep|status`, a hub (`hub serve`, reached through `hub connect` over any stdio pipe such as SSH), and a collector (`collect`) that streams session files to the hub. Verification drives the real binary against copies of `tests/fixtures/`, never against the user's live hub.

Everything goes through `scripts/verify.sh` (run it with bash). Paths below are relative to this skill's folder.

## Launch

```bash
scripts/verify.sh launch
```

This builds `cargo build --release --locked`, copies the binary and the Claude Code, Codex, and Grok fixtures into a fresh run folder, and starts `hub serve` on that folder's data directory. It prints `RUN=<dir>` and `EVIDENCE=<dir>`. Ready means `stats` answers on the hub socket, and the script waits up to 10 s for that. Pass `RUN` to every later call.

Run folders live under `/tmp/prime-radiant-verify/` because a Unix socket path must fit in about 100 bytes. Set `PR_VERIFY_ROOT` to move them, keeping the path short. Two runs can go side by side: each has its own data folder and socket.

## Doctor

```bash
scripts/verify.sh doctor "$RUN"
```

Read-only. It checks five things and prints `ok` or `FAIL` for each:
- the binary runs;
- it was built from the checked-out commit;
- the hub process this run started is alive;
- the hub serves this run's data folder;
- the hub answers on the run's socket.

Run it first whenever anything looks off, and drive nothing that fails it.

## Drive

`scripts/verify.sh cli "$RUN" <label> <prime-radiant args...>` runs the binary and records the call. Inside the args, the script swaps these placeholders for this run's paths:

| Placeholder | Becomes |
| --- | --- |
| `@RUN@` | the run folder |
| `@DATA@` | the hub's data folder |
| `@SOCK@` | the hub socket |
| `@SESSIONS@` | the copied fixtures |
| `@BIN@` | the binary |

Examples:

```bash
scripts/verify.sh cli "$RUN" find-prefix find 33831ab7 --hub-socket @SOCK@
scripts/verify.sh collector "$RUN" start
TOKEN=$(scripts/verify.sh token)
scripts/verify.sh append "$RUN" "verification token $TOKEN"
scripts/verify.sh wait-find "$RUN" "$TOKEN" 30
```

The hub is its data folder's only writer. To try `import`, give it its own folder, such as `--data @RUN@/local`. Never point `import` at `@DATA@` while the hub runs.

The feature map in `features/README.md` lists every user path and the end state that proves it. A proof covers every entry point the feature lists.

## Evidence

Each `cli` call writes `EVIDENCE/<NN>-<label>.txt` with the exact command, exit code, stdout, and stderr. `wait-find` appends to `EVIDENCE/freshness.txt`. Cleanup copies the hub and collector logs into `EVIDENCE`. Evidence lives under `${TMPDIR:-/tmp}/prime-radiant-evidence/`, or `PR_EVIDENCE_ROOT` if set, and cleanup never deletes it.

Proof standards:
- Use the real user path: the CLI and the hub socket or bridge, never SQL on the data folder.
- Capture the action and the resulting state. For example, after an append, show the `find` that hits, not only the append.
- Check side effects with a second read-only view. Use `stats --json` counts before and after, or a second `import` that reports 0 bytes appended.
- Real session folders (`~/.claude/projects`, `~/.codex`, `~/.t3/userdata/statev2.sqlite`, `~/.grok`) are read only, and only when the user asks. Their contents never go into the repo, commits, or PRs.

## Cleanup

```bash
scripts/verify.sh cleanup "$RUN"
```

It stops only the hub and collector whose pid files are in `RUN`, and only if their command line names `RUN`. Then it copies the logs into `EVIDENCE`, deletes `RUN`, and prints where the evidence is. Run it after every attempt, including failed ones.

## Helpers

- `scripts/verify.sh`: `launch`, `doctor`, `cli`, `collector start|stop|crash`, `hub stop|start`, `big-session`, `token`, `append`, `wait-find`, `cleanup`. Run `scripts/verify.sh` with no arguments for usage.
- `eval/`: ground truth and scoring for search quality on the user's own sessions. See `features/search-quality.md`.
