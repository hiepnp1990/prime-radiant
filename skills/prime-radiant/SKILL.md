---
name: prime-radiant
description: Look up a past AI session with Prime Radiant. Use when the user mentions an earlier session by ID, title, link, or topic, such as "the session where we fixed the login bug" or a pasted session ID.
---

# Prime Radiant

Prime Radiant keeps every past coding-agent and chat session on one hub. The `prime-radiant` CLI answers from that hub on any machine.

## When to use it

Use it when the user refers to a past session: a session ID or ID prefix, a title, a link, or a topic ("what did we decide about retries last week"). Don't use it for the current session.

## Order

1. `find` the session. Pass an ID, an ID prefix of 8 or more characters, a link, or a few words about the topic.
2. If one candidate clearly fits, run `brief` on its key. The brief is usually enough to answer.
3. If several candidates are close, show them to the user (title, project, last activity) and ask which one they mean. Don't guess.
4. For one detail the brief lacks, such as an error message, a command, or a file name, run `grep` on the session, then `read` only the page it names.
5. Run `read` page by page only when the user asks for the full transcript. Each page ends with the command for the next page.

## Commands

```sh
prime-radiant find <id, link, or words> [--limit N]
prime-radiant brief <session>
prime-radiant grep <session> <text>
prime-radiant read <session> [--page N]
```

- `<session>` is a key from `find`, such as `claude-code/<id>`, an alias, or a unique ID prefix.
- `find` returns up to 10 candidates, best first. `brief` returns the first request, the last answer, pull requests, changed files, and subagents.
- `grep` ignores case, shows the first 20 matches with about 200 characters around each, and names the `read` page of each match.
- `read` pages hold about 12,000 characters. Tool calls are cut to 2,000 characters and tool results to 4,000. Copied history in a fork is marked as copied.
- Add `--json` to any command for structured output.

## Reaching the hub

With no flag, the CLI runs the hub command in `PRIME_RADIANT_HUB`, such as `ssh <hub-host> prime-radiant hub connect --data <dir>`. To choose another way, pass `--hub-command "<command>"`, `--hub-socket <path>` for a hub on this machine, or `--data <dir>` for a local data folder. If no flag is given and the variable is unset, the CLI says so. Tell the user instead of guessing a hub.

## Secrets

Output is already redacted. Tokens, keys, and passwords show as `[redacted]`. Don't try to recover them, and don't ask the hub for the raw files.
