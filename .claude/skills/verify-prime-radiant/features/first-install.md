# First install

A new user follows the README quickstart from nothing to a `find` that answers from the hub. On the always-on machine, `service install hub` serves `~/.local/share/prime-radiant`, and `service install collector` with no flags reads every harness and reaches that hub through its socket. On the laptop, `config set hub-command "ssh <hub-host> .local/bin/prime-radiant hub connect"` saves the route. `hub connect` with no `--data` reaches the hub that serves `~/.local/share/prime-radiant`, and the binary is named by its path because SSH runs the command in a non-interactive shell. `skill install` gives each harness the skill, `doctor` passes, `status` shows the first sync reach live, and `find` names the session. `scripts/fresh-install-check.sh` at the repository root runs the one-machine half of this journey, and CI runs it on Linux and macOS.

## Sub-features

- `first-hub` defines the hub's service on `~/.local/share/prime-radiant` when no `--data` is given.
- `first-collector` defines the collector with every harness in the home folder and the default data folder's hub socket, when no flag names either.
- `first-connect-default` reaches the hub through `hub connect` with no `--data`, over a stand-in for SSH.
- `first-doctor-by-name` fails to reach a hub whose SSH command runs `prime-radiant` by name where the remote `PATH` lacks it, and warns with the command that names the binary by `.local/bin/prime-radiant`.
- `first-doctor-ok` exits 0 with only `ok` lines once the route, the collector, and the skill are set up.
- `first-live` shows the collector `live` with the first sync at 100%, and the hub holding every fixture stream.
- `first-find` finds the fixture session by its full ID and by the word `json`.

## How to get to it (user POV)

- Follow the README's quickstart: install the binary into `~/.local/bin`, run `prime-radiant service install hub` and `prime-radiant service install collector --origin <name>` on the always-on machine, set up SSH key access, then `prime-radiant config set hub-command "ssh <hub-host> .local/bin/prime-radiant hub connect"` and `prime-radiant service install collector --origin laptop` on the laptop. Then run `prime-radiant skill install`, `prime-radiant doctor`, `prime-radiant status --watch`, and `prime-radiant find <words>`.
- Run `scripts/fresh-install-check.sh` from the repository root for the one-machine journey in a throwaway home under `/tmp`.

## Driving it with verify.sh

Preconditions:

- Baseline preconditions hold, in a fresh run, with no collector started yet.
- One stand-in home, `RUN/home`, plays both machines. The run's hub stands in for the hub's service, and a stand-in `ssh` runs the laptop's hub command in that same home. Placeholders are not swapped inside variables, so write `$RUN` there.

- **Install.** Run `mkdir -p "$RUN/home/.local/bin" "$RUN/home/.local/share" "$RUN/home/.claude" && ln -s "$RUN/bin/prime-radiant" "$RUN/home/.local/bin/prime-radiant"`. A link, not a copy, so that `doctor` finds this binary on `PATH`.
- **Harnesses.** Run `cp -R "$RUN/sessions/claude-code" "$RUN/home/.claude/projects" && cp -R "$RUN/sessions/codex" "$RUN/home/.codex" && cp -R "$RUN/sessions/grok" "$RUN/home/.grok"`.
- **Hub.** Run `ln -s "$RUN/data" "$RUN/home/.local/share/prime-radiant"`, so the run's hub serves the default data folder. Then `scripts/verify.sh home "$RUN" first-hub service install hub --dry-run` exits 0, and the definition runs `<BIN> hub serve --data <RUN>/home/.local/share/prime-radiant`.
- **Collector.** Run `scripts/verify.sh home "$RUN" first-collector service install collector --origin verify --dry-run`. The first line is `no source flag, so reading every harness on this machine: claude-code at <RUN>/home/.claude/projects (1 session), codex at <RUN>/home/.codex (4 sessions), grok at <RUN>/home/.grok (3 sessions)`. The second is `no hub flag, so the hub is the hub socket <RUN>/home/.local/share/prime-radiant/hub.sock, from the default data folder, which has a hub socket`. Then run `scripts/verify.sh collector "$RUN" home`, which starts the same `collect` in the background.
- **SSH.** Run `scripts/verify.sh stand-in-ssh "$RUN"`, then `export PATH="$RUN/tools:$RUN/home/.local/bin:$PATH"` for every later step.
- **By name.** Run `scripts/verify.sh home "$RUN" first-config-by-name config set hub-command "ssh verify-hub prime-radiant hub connect"`, then `scripts/verify.sh home "$RUN" first-doctor-by-name doctor`. It exits 1. Stdout has `fail  cannot reach the hub: the hub closed the connection without an answer` with ``      next: run `ssh verify-hub prime-radiant hub connect </dev/null` to see its error``. The next lines are `warn  the hub command runs prime-radiant by name on verify-hub, and the non-interactive shell that SSH runs it in may not have ~/.local/bin on PATH` and `      next: if it printed that prime-radiant was not found, name the binary by its path: prime-radiant config set hub-command 'ssh verify-hub .local/bin/prime-radiant hub connect'`. Stderr has `sh: prime-radiant: command not found`, or `not found` in another wording, and `error: 1 check failed`.
- **By path.** Run `scripts/verify.sh home "$RUN" first-config config set hub-command "ssh verify-hub .local/bin/prime-radiant hub connect"`, then `scripts/verify.sh home "$RUN" first-skill skill install`. It prints `wrote` for claude-code, codex, and grok.
- **Doctor passes.** Run `scripts/verify.sh home "$RUN" first-doctor doctor`. It exits 0, and every line starts with `ok`, among them `the hub answered in N ms: 19 streams from 1 origin`, `SSH connection sharing is on for verify-hub`, `collector verify: running, live · watching 19 files`, and `skill: installed for` each of the three harnesses.
- **Live.** Run `scripts/verify.sh home "$RUN" first-status-json status --json`. `collectors[0].running` is true, `collectors[0].state.phase.name` is `live`, and `hub.origins[0]` is `verify` with 19 streams. Then `scripts/verify.sh home "$RUN" first-status status` shows `first sync 100%: 19 files, 34 KiB` and `hub (ssh verify-hub .local/bin/prime-radiant hub connect):`.
- **Find.** Run `scripts/verify.sh home "$RUN" first-find-id find 33831ab7-5c2a-48a4-a8d7-0b5125040f4d --limit 1`. The hit is `Add a JSON output flag`, key `claude-code/33831ab7-5c2a-48a4-a8d7-0b5125040f4d`, `ran on verify`. Then `scripts/verify.sh home "$RUN" first-find-word find json` lists that key among its hits.
- **Script.** Run `scripts/fresh-install-check.sh` from the repository root. It ends with `== pass: the quickstart works from nothing` and `removed /tmp/pr-fresh.<id>`, and exits 0.

## Gotchas

- The stand-in `ssh` drops only its first argument, the host, so give it no options. It never runs the real `ssh`, so `doctor` reads no real `~/.ssh/config`.
- The collector that `collector "$RUN" home` starts found the hub before `config set`, through the default data folder, as the always-on machine's collector does. The laptop's collector would keep the hub command instead.
- The 19 streams are the fixture files that the collector reads. A fixture added later changes the count.
- `fresh-install-check.sh` builds with `cargo install --path` before it changes `HOME`, because Cargo and rustup find their own files under the real home. Every `prime-radiant` call after that runs with `HOME` in `/tmp/pr-fresh.<id>`, the XDG variables and `PRIME_RADIANT_HUB` unset, and a `PATH` of `~/.local/bin` and the system folders, so no other `prime-radiant` on the machine answers.
- Neither path installs a real service. A real `service install` changes the user's login services. See [Services](./services.md). A real `service install hub` also waits up to 10 s for the hub to answer on its socket and prints `the hub answers on <socket>`, so that a `service install collector` right after it finds the hub. A dry run doesn't wait, so no recipe here shows that line.
