# 0001. Central session store on an always-on hub

- Status: Accepted
- Date: 2026-10-22
- Revised: 2026-10-23. Adds web chats, retrieval, access, multiple machines, and the split between hot and cold storage.

## Context

A coding agent can read only the sessions of its own harness on its own machine. Each harness writes native transcripts in its own format to a local folder or database. Harness cleanup also deletes old transcripts. Claude Code, for example, deletes transcripts after 30 days by default.

A typical setup has a laptop that is often asleep and one always-on machine, such as a home server or a VPS. Native transcripts grow fast. Most of their size is images embedded in the transcript, and the transcripts on one laptop can reach tens of gigabytes.

Finding a session from its ID alone works only for some harnesses. Claude Code and Codex put the session ID in the file name, so a file search finds it. T3 Code keeps its thread IDs inside a SQLite database, so a file search never finds them. A coding agent on one machine also can't read the sessions of a laptop that is asleep.

Web chats are a second kind of source. ChatGPT and Claude subscriptions offer no API and no webhook for your own conversations. The only official way out is a manual account export, and ChatGPT says an export can take up to 7 days. Every client, whether web, phone, or desktop, syncs its conversations to the provider's servers.

Prime Radiant must meet these requirements:

- A coding agent in any harness, on any machine, finds a session by ID, title, or topic.
- A query returns a brief by default: decisions, outcome, and artifacts. It returns the full transcript when the user asks for it.
- Sessions from coding agents, and from ChatGPT and claude.ai on any device, reach the hub without any action from the user.
- A running session is readable within seconds.
- Transcripts survive harness cleanup and the loss of the machine where they ran.
- The users are one person and that person's coding agents.

## Decision

Prime Radiant has four parts. Collectors on every machine capture sessions. A hub on one always-on machine accepts, indexes, and answers. Storage keeps recent data on the hub and complete history in cloud object storage. Access on every machine lets a coding agent ask questions. A coding agent never reads another machine directly.

![Inside Prime Radiant: collectors and access on every machine, the hub, and storage](../diagrams/prime-radiant-overview.svg)

| Part | Runs on | Contains |
| --- | --- | --- |
| Collectors | Every machine. The importer runs on the hub. | Transcript tailing, the web chat extension and its local spool, and the account export importer |
| Hub | The always-on machine | Intake, indexer, query, and archiver |
| Storage | The hub, and cloud object storage | Hot store and index on the hub. Cold archive in the cloud. |
| Access | Every machine | A session CLI and a skill. An MCP server, a prompt hook, and a T3 Code picker can wrap the CLI later. |

### Components

![Prime Radiant components on each machine, the hub, and cloud storage](../diagrams/prime-radiant-components.svg)

Query also reads old sessions from the cold archive. The diagram leaves out that arrow so that it crosses no other arrow.

### Capture

| Source kind | Sources | How | Freshness |
| --- | --- | --- | --- |
| Transcript tailing | Claude Code, Codex, Grok, T3 Code | The collector watches native transcripts, `tool-results/`, and subagent transcripts, and sends new bytes. A sweep every few minutes catches changes that file watching missed. | Seconds |
| Web chat capture | ChatGPT, claude.ai | A browser extension takes a snapshot of a conversation after each reply. Every few minutes it also sweeps the provider's list of recently updated conversations, which covers chats from the phone and desktop apps. A native messaging helper writes each snapshot to a local spool, and the collector sends it. | Seconds on the web. Minutes for phone and desktop chats, while a browser with the extension runs on one of the user's machines. |
| Account export import | ChatGPT, claude.ai | The importer on the hub loads account exports and fills gaps that web chat capture missed. | Each export |

Intake accepts two kinds of write. An append sends the bytes after an offset that the hub confirmed. A snapshot sends a whole conversation with a checksum, and intake stores it only if the checksum is new.

Every session has one address, `<origin>/<source>/<native-id>`. The origin is a machine ID for a local session and an account for a web chat. The index maps other IDs to that address, such as a T3 Code thread ID or a Claude desktop `local_` ID.

### Machines and connections

- Each machine keeps one connection open to the hub. The connection carries the collector's segments and the agents' queries. The hub's own collector and access use no network.
- Each origin writes only to its own folder. The indexer is the only writer to the index.
- On every connect, a collector asks intake for its confirmed offsets and sends only newer bytes. A retry after a crash or a network change is safe.
- Storage never deletes a transcript because its source deleted it.

### Retrieval

| Operation | Returns |
| --- | --- |
| `find` | Ranked candidates for an ID, a title, or words about the topic |
| `brief` | Decisions, outcome, and links to artifacts, pull requests, and changed files |
| `read` | The transcript in pages of fixed size, with images as references |
| `grep` | Matching passages in one session |

The skill tells a coding agent to start with `find` and `brief`, and to read the full transcript only when the user asks. The skill installs into the skills folder of each harness, such as Claude Code, Codex, and Grok, on every machine.

## Consequences

- A query does not depend on the machine where the session ran. A sleeping laptop stops sending new activity, and its sessions stay readable on the hub.
- Web chats from every device arrive without user action. Web chat capture depends on undocumented endpoints of the chat web apps. A site change can stop capture for that site until its adapter is fixed. Account exports fill the gap.
- Phone and desktop chats arrive only while a browser with the extension runs on one of the user's machines.
- The hub is a single point of failure for queries. While the hub is down, collectors wait and lose nothing.
- The cold archive becomes the only copy of transcripts that a source has deleted.
- Storage contains every secret that appeared in a session.
- Phase 2 can move the hot store and the index off the hub. If collectors then write to object storage directly, their destination changes too. Collectors send segments, so that change stays small.

## MVP implementation

These choices are proposals. A change to one of them does not change the architecture.

| Component | MVP choice |
| --- | --- |
| Always-on hub | Any always-on machine the user controls, such as a home server or a VPS |
| Connection | SSH over a private network, such as Tailscale or WireGuard, with one connection kept open per machine |
| Index | SQLite with full-text search |
| Cold archive | Cloudflare R2 |
| Web chat capture | A Chrome extension with a native messaging helper |
| Access | A session CLI and a skill |

## Open questions

- Does the extension's sweep list chats from the phone and desktop apps, and does it work without an open chat tab? A throwaway prototype answers this before the extension is built.
- Does the hub encrypt transcripts before they reach cloud storage, or rely on the provider's encryption at rest?
- Does the MVP start with extractive briefs, or with briefs that an LLM writes?
- Does a collector copy media files that a session only links to, such as videos, audio, and generated files?
- When the hub is down, does a machine fall back to its own native transcripts?
- Does the MVP include Grok?
- The MVP is read-only. Sending messages to a running session is out of scope.
