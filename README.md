# Prime Radiant

Prime Radiant lets a coding agent find and read any past AI session: coding-agent sessions from any harness on any machine you own, and ChatGPT and claude.ai chats from any device.

Today each agent harness keeps its sessions in its own format on the machine where they ran. A Codex session on one machine can't read a Claude Code session on another, web chats stay inside each chat app, and harness cleanup deletes old transcripts. Prime Radiant copies every session to one always-on hub, indexes it, and answers queries by session ID, title, or topic.

## Status

Prime Radiant is in early development. It reads Claude Code, Codex, Grok, and T3 Code sessions, and a hub serves them to the CLI on any machine. A coding agent follows [the skill](skills/prime-radiant/SKILL.md) to find, brief, search, and read past sessions.

## Install

Prime Radiant runs on macOS and Linux.

### Build from source

You need a recent stable Rust toolchain. Cargo builds the `prime-radiant` binary and installs it to `~/.cargo/bin`:

```sh
cargo install --git https://github.com/hiepnp1990/prime-radiant --locked
```

### Install a release binary

No release is published yet, so this command fails today. After the first release, it installs a prebuilt binary for macOS on Apple silicon or Linux on x86_64:

```sh
curl -fsSL https://raw.githubusercontent.com/hiepnp1990/prime-radiant/main/install.sh | sh
```

The script downloads the binary for your platform from the latest release and checks it against the release's `SHA256SUMS`. It installs `prime-radiant` to `~/.local/bin` without sudo and tells you whether that folder is on your `PATH`. It never edits your shell files.

To install a specific release, set `PRIME_RADIANT_VERSION` to its tag. To install to `$PREFIX/bin`, set `PREFIX`:

```sh
curl -fsSL https://raw.githubusercontent.com/hiepnp1990/prime-radiant/main/install.sh | PRIME_RADIANT_VERSION=v0.1.0 PREFIX="$HOME/tools" sh
```

On other platforms, build from source.

## Set up

The hub runs on an always-on machine, such as a home server or a VPS. Each machine that runs coding agents also runs a collector, which sends that machine's sessions to the hub.

On the hub's machine, run the hub and a collector as login services:

```sh
prime-radiant service install hub
prime-radiant service install collector --origin <name>
```

The hub serves `~/.local/share/prime-radiant`, and the CLI on the same machine finds it there with no other setup.

On each other machine, save the route to the hub once, then install a collector:

```sh
prime-radiant config set hub-command "ssh <hub-host> prime-radiant hub connect --data <hub data folder>"
prime-radiant service install collector --origin <name>
```

`config set` writes `~/.config/prime-radiant/config.toml`, which every shell reads. A `PRIME_RADIANT_HUB` exported in `~/.bashrc` reaches only interactive shells, and an agent's tool shell often isn't one. Over SSH, the hub's machine runs the command in a non-interactive shell too, so give the full path to `prime-radiant` if that shell doesn't have it on its `PATH`.

With no source flag, `collect` and `service install collector` read every harness the machine has: Claude Code, Codex, Grok, and T3 Code. With no hub flag, they reach the hub that `prime-radiant config show` prints.

Then give each harness's agents the skill, and check the setup:

```sh
prime-radiant skill install
prime-radiant doctor
```

`skill install` writes the skill to `~/.claude/skills`, `~/.codex/skills`, and `~/.grok/skills`, for each of those harnesses the machine has. It replaces only a copy it wrote, and never one that is a link to another folder. `doctor` changes nothing. It prints `ok`, `warn`, or `fail` for each check, and the next step after each warning and failure.

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

Tests use only made-up example sessions under `tests/fixtures/`.

To publish a release, set `version` in `Cargo.toml` and merge to `main`. Then tag that commit with the same version, such as `v0.1.0`, and push the tag. CI tests and builds the tagged commit. If the tag matches the version the binary reports, CI attaches `prime-radiant-linux-x86_64`, `prime-radiant-macos-arm64`, and `SHA256SUMS` to a GitHub release for the tag.

## License

[MIT](LICENSE)
