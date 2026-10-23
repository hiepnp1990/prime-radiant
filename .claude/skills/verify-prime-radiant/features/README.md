# Prime Radiant verification map

The maintained source for verifying what users of Prime Radiant can observe. Read this index, then use the matching feature file as the recipe.

## Baseline preconditions

- `scripts/verify.sh launch` has printed `RUN` and `EVIDENCE`, and `scripts/verify.sh doctor "$RUN"` reports all `ok`.
- The run's fixtures are fresh copies of `tests/fixtures/`. The Claude Code fixture is session `33831ab7-5c2a-48a4-a8d7-0b5125040f4d`, titled `Add a JSON output flag`, with subagent `agent-a75ce3543dd257a2d`.
- No step drives a hub this run did not start.

## Driving conventions

- Run every command through `scripts/verify.sh cli "$RUN" <label> ...` so it is recorded. Use bash: zsh does not split a variable holding several flags.
- Use the `@SOCK@`, `@DATA@`, `@SESSIONS@`, `@RUN@`, `@BIN@`, and `@HOME@` placeholders, never typed paths.
- Run a call that has no hub flag, or that touches the config file, skills, or state folder, through `scripts/verify.sh home`, so the user's own setup never answers.
- Prefer `--json` output for checks, and plain text where the user-facing wording is the thing under test.

## Proof and skip reporting

- CLI proof is the recorded evidence file: command, exit code, stdout, stderr.
- A change to stored state needs a second read-only view, such as `stats --json` counts.
- Report an entry point you could not reach with the command tried and the missing precondition. Never report it as verified through another path.

## Feature entry contract

Each feature file starts with an H1 and one paragraph on the user-visible behavior, then four H2 sections in this order: `Sub-features`, `How to get to it (user POV)`, `Driving it with verify.sh`, `Gotchas`.

## Features

- [Import and stats](./import-and-stats.md): reads session folders into a data folder; a second import changes nothing.
- [Find and brief](./find-and-brief.md): finds a session by ID, prefix, alias, or words, and summarizes it.
- [Read and grep](./read-and-grep.md): pages through a session and finds text inside it.
- [Live sync](./live-sync.md): the collector streams changes to the hub, survives a crash, and a new line becomes findable.
- [Remote access](./remote-access.md): the CLI reaches the hub through a stdio bridge such as SSH.
- [Services](./services.md): `service install`, `uninstall`, and `list` keep the hub and collectors running as login services; a dry run prints the definition and changes nothing.
- [Progress and status](./progress-and-status.md): the collector reports its phase and first-sync progress, and `status` shows every collector and what the hub holds from each origin.
- [Setup](./setup.md): `config set` saves the route to the hub in a file every shell reads, `doctor` checks the setup and names the next step, and `skill install` gives each harness the skill.
- [Search quality](./search-quality.md): measures topic search on the user's own sessions against ground truth from the harnesses' own files.
