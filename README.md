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
