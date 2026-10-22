# Prime Radiant

Prime Radiant lets a coding agent find and read any past AI session: coding-agent sessions from any harness on any machine you own, and ChatGPT and claude.ai chats from any device.

Today each agent harness keeps its sessions in its own format on the machine where they ran. A Codex session on one machine can't read a Claude Code session on another, web chats stay inside each chat app, and harness cleanup deletes old transcripts. Prime Radiant copies every session to one always-on hub, indexes it, and answers queries by session ID, title, or topic.

## Status

Prime Radiant is in early development. The repository has the Rust project skeleton and its tests.

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

## License

[MIT](LICENSE)
