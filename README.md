# Prime Radiant

Prime Radiant lets a coding agent find and read any past AI session: coding-agent sessions from any harness on any machine you own, and ChatGPT and claude.ai chats from any device.

Today each agent harness keeps its sessions in its own format on the machine where they ran. A Codex session on one machine can't read a Claude Code session on another, web chats stay inside each chat app, and harness cleanup deletes old transcripts. Prime Radiant copies every session to one always-on hub, indexes it, and answers queries by session ID, title, or topic.

## Status

Prime Radiant is in early development. It reads Claude Code, Codex, Grok, and T3 Code sessions, and a hub serves them to the CLI on any machine. A coding agent follows [the skill](skills/prime-radiant/SKILL.md) to find, brief, search, and read past sessions.

## Quickstart

These steps set up a laptop and an always-on machine, such as a home server or a VPS, and end with a `find` that answers from the hub. The hub keeps every session. Each machine that runs coding agents also runs a collector, which sends that machine's sessions to the hub. To use one machine, do the always-on machine's steps on it and skip the laptop's.

Prime Radiant runs on macOS and Linux. Run every step as your own user, with no sudo.

### 1. Install the binary on each machine

You need a recent stable Rust toolchain, such as one from rustup, and a C compiler, because Cargo builds SQLite and zstd from source. This command builds `prime-radiant` and installs it to `~/.local/bin`:

```sh
cargo install --git https://github.com/hiepnp1990/prime-radiant --locked --root ~/.local
```

If `~/.local/bin` isn't on your `PATH`, add it where your login shell sets `PATH`, such as `~/.profile` or `~/.zprofile`, and open a new terminal. Agents run `prime-radiant` by name.

### 2. On the always-on machine, start the hub

```sh
prime-radiant service install hub
```

The hub keeps its sessions in `~/.local/share/prime-radiant`, and `service install hub` returns once the hub answers on `hub.sock` in that folder. The CLI on this machine finds the hub through that socket, with no other setup.

If this machine also runs coding agents, start a collector for it:

```sh
prime-radiant service install collector --origin server
```

The collector finds the machine's Claude Code, Codex, Grok, and T3 Code sessions and sends them to the hub. `--origin` is the name this machine's sessions carry in `find` results.

Both run as login services, a systemd user unit on Linux and a launchd agent on macOS. Add `--dry-run` to print the definition and change nothing. On Linux, if `service install` warns that lingering is off, run `loginctl enable-linger $USER` so the services keep running after you log out. On macOS, launchd runs them only while you are logged in at the Mac, and `service install` needs that login, not only an SSH session.

### 3. On the laptop, connect to the hub

The laptop reaches the hub over SSH. Set up SSH key access to the hub's machine, for example with `ssh-keygen` and `ssh-copy-id <hub-host>`. Then check it the way the collector's service uses it, with no terminal to answer a prompt:

```sh
ssh -o BatchMode=yes <hub-host> .local/bin/prime-radiant --version
```

It prints the version when the key works without a prompt and the binary is where the hub command expects it. A key with a passphrase also needs an `ssh-agent` that the service can reach, which this check doesn't prove. Step 5 shows whether the collector got through.

Then save the route to the hub, and start the laptop's collector:

```sh
prime-radiant config set hub-command "ssh <hub-host> .local/bin/prime-radiant hub connect"
prime-radiant service install collector --origin laptop
```

SSH runs the command in a non-interactive shell, which may not have `~/.local/bin` on its `PATH`. That's why the command names the binary by its path, relative to the home folder on the hub's machine. With no `--data`, `hub connect` reaches the hub that serves `~/.local/share/prime-radiant`.

`config set` writes `~/.config/prime-radiant/config.toml`, which every shell reads, including an agent's tool shell. A `PRIME_RADIANT_HUB` exported in `~/.bashrc` reaches only interactive shells. The collector's service keeps the route and the harnesses it found, so run `service install collector` again after you change either.

### 4. On each machine that runs coding agents, give them the skill

```sh
prime-radiant skill install
```

It writes [the skill](skills/prime-radiant/SKILL.md) to `~/.claude/skills`, `~/.codex/skills`, and `~/.grok/skills`, for each of those harnesses the machine has. It replaces only a copy it wrote.

### 5. On each machine, check the setup and watch the first sync

```sh
prime-radiant doctor
prime-radiant status --watch
```

`doctor` changes nothing. It prints `ok`, `warn`, or `fail` for each check, with the next step under each warning and failure. `status` shows what each collector on this machine is doing and what the hub holds from each origin. The first sync is done when the collector shows `live`. Press Ctrl-C to stop watching.

### 6. Find a session

```sh
prime-radiant find <words about a past session>
```

Each hit shows the session's key, project, and the machine it ran on. `prime-radiant brief <key>` summarizes one session. Your agents use the same commands through the skill.

### How the laptop reaches the hub

There are two kinds of connection:

- A collector holds one long-lived connection to the hub. When it drops, the collector reconnects and sends only what the hub lacks.
- A CLI query, such as `find` or `status`, runs the hub command each time. Over SSH, every query opens a new SSH connection unless SSH connection sharing is on.

To share one SSH connection, add this to `~/.ssh/config` on the laptop:

```text
Host <hub-host>
  ControlMaster auto
  ControlPath ~/.ssh/control-%C
  ControlPersist 10m
```

### If doctor fails

Each `fail` line has the next step under it. If the hub command can't reach the hub, run the command yourself. It prints nothing and exits when the route works, and an error when it doesn't:

```sh
ssh <hub-host> .local/bin/prime-radiant hub connect </dev/null
```

- A password prompt or `Permission denied` means SSH key access isn't set up. Repeat step 3.
- `.local/bin/prime-radiant: No such file or directory` or `command not found` means the binary isn't at that path on the hub's machine. Install it there, or put its path in the command.
- `hub.sock: No such file or directory` or `hub.sock: Connection refused` means no hub serves `~/.local/share/prime-radiant` on the hub's machine. Run `prime-radiant service list` there, and read the hub's log.

Run `doctor` again after each fix. If a collector shows `stopped` or `reconnecting`, `prime-radiant service list` names its log.

### Uninstall

On each machine, remove the services, the skill, the saved route, and the binary:

```sh
prime-radiant service uninstall collector --origin <origin>
prime-radiant service uninstall hub
prime-radiant skill uninstall
prime-radiant config unset hub
cargo uninstall prime-radiant --root ~/.local
```

Run `service uninstall hub` only on the always-on machine. `service uninstall` leaves the data and the logs. To delete the stored sessions too, remove `~/.local/share/prime-radiant` on the hub's machine. Collector state is in `~/.local/state/prime-radiant`.

## Other ways to install

### Install a release binary

No release is published yet, so this command fails today. After the first release, it installs a prebuilt binary for macOS on Apple silicon or Linux on x86_64:

```sh
curl -fsSL https://raw.githubusercontent.com/hiepnp1990/prime-radiant/main/install.sh | sh
```

The script downloads the binary for your platform from the latest release and checks it against the release's `SHA256SUMS`. It installs `prime-radiant` to `~/.local/bin`, the folder the quickstart uses, and tells you whether that folder is on your `PATH`. It never edits your shell files. A binary installed this way is removed with `rm ~/.local/bin/prime-radiant`.

To install a specific release, set `PRIME_RADIANT_VERSION` to its tag. To install to `$PREFIX/bin`, set `PREFIX`:

```sh
curl -fsSL https://raw.githubusercontent.com/hiepnp1990/prime-radiant/main/install.sh | PRIME_RADIANT_VERSION=v0.1.0 PREFIX="$HOME/tools" sh
```

On other platforms, build from source as in the quickstart.

## Design decisions

- [0001. Central session store on an always-on hub](docs/decisions/0001-central-session-store.md)
- [0002. Data model](docs/decisions/0002-data-model.md)
- [0003. Thin slice](docs/decisions/0003-thin-slice.md)

## Development

Prime Radiant is written in Rust. With the Rust toolchain installed:

```sh
cargo build
cargo test
```

Tests use only made-up example sessions under `tests/fixtures/`. `scripts/fresh-install-check.sh` runs the quickstart on one machine from nothing, in a throwaway home folder under `/tmp`, with copies of those sessions. It installs the binary from the checkout with `cargo install --path`, and the hub, the collector, and the CLI talk over a local socket, so no step after the build uses the network. CI runs it on Linux and macOS.

To publish a release, set `version` in `Cargo.toml` and merge to `main`. Then tag that commit with the same version, such as `v0.1.0`, and push the tag. CI tests and builds the tagged commit. If the tag matches the version the binary reports, CI attaches `prime-radiant-linux-x86_64`, `prime-radiant-macos-arm64`, and `SHA256SUMS` to a GitHub release for the tag.

## License

[MIT](LICENSE)
