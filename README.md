# Grouper

A personal, multi-provider, multi-device AI orchestrator. Design:
`Grouper — Proposal v3.docx` (the project) and [docs/harness.md](docs/harness.md)
(the agent harness). Every change is logged in [Changelogs/](Changelogs/).

## What works now

- **Web app**: open the hub's address in a browser (`https://grouper.clockmillbarn.uk/`) and
  sign in with an admin token: chat with live streaming and thread cards, approvals, the
  thread inspector, devices and permissions, models, usage and limits, memory and skills.
  Served by the hub itself; a session cookie, never the token, stays in the browser.
- **Hub**: devices, tokens and live presence; SQLite storage; HTTP API and a live event stream.
- **Harness**: project chat (an orchestrator that starts subagent **threads** on your
  devices or on the hub and is woken when they finish or ask something) and standard
  chat (one model). Threads run in parallel, can ask the orchestrator questions, and you
  can message or stop any thread. A thread for an offline device works to a checkpoint
  on the hub and continues on the device when it reconnects.
- **MCP**: each device is an MCP server to the hub over its WebSocket, and hosts local MCP
  servers. Your subagent-chat plugin is found automatically, so threads can hand work to
  Claude Code, Codex, Antigravity, Hermes, Grok, pi, opencode and Cursor
  (`codex__codex_chat` and so on). Add other servers in `<config dir>/grouper/mcp.json`
  (Claude-style `mcpServers`). See [docs/mcp.md](docs/mcp.md).
- **Tools and permissions**: threads get workspace files, the shell, the device's own
  tools and its MCP tools. Each device's tools are in a registry with Manual / Auto / Bypass levels that only
  you can change; approvals reach every client.
- **Streaming**: replies and thread text reach every client token by token; only full
  messages are stored.
- **Memory and skills**: long-term memory shared by every conversation, recalled
  automatically when relevant; skills (saved procedures) offered on similar tasks.
- **Models**: Anthropic and any OpenAI-compatible API (OpenRouter, OpenAI, DeepSeek, Qwen,
  Grok, local servers), plus a scripted fake model for tests. Keys belong to the account,
  are encrypted at rest on the hub, and sync to every device.
- **Registry that learns**: thread outcomes, grades and your ratings build per-model and
  per-device stats; priority rules (e.g. sensitive → local only) and stats choose each
  thread's model; the hub proposes use-case changes for you to accept.
- **Accounting**: tokens and estimated cost per call, conversation, model and day
  (`grouper usage`), and OpenRouter/DeepSeek balances.
- **Windows**: thread shells run PowerShell, with Windows-aware Auto risk rules; the device
  token is stored with DPAPI (`grouper login`).
- **Swift ⇄ Rust**: the Mac demo app runs threads inside the Rust core and offers a
  Swift-implemented tool to them.

## Layout

| Path | What |
|---|---|
| `crates/core-protocol` | Wire protocol and shared types (frames, events, registry rows) |
| `crates/core-registry` | Model and device registry: learning, stats, routing, priority rules, cost maths, probes |
| `crates/core-memory` | Long-term memory and skills: SQLite + FTS5 storage, search and recall blocks |
| `crates/providers` | Model adapters: Anthropic, OpenAI-compatible, scripted |
| `crates/core-agent` | The loop engine, workspace and shell tools, permission rules, thread runner |
| `crates/hub/web` | The web app (plain ES modules, embedded in `grouper-hub`) |
| `crates/core-sync` | Local SQLite cache of conversations, threads and events, kept in sync with the hub; offline reads |
| `crates/core-client` | Device side: connection, presence, running threads, approvals |
| `crates/hub` | `grouper-hub`: storage, orchestrator, threads, HTTP and WebSocket API |
| `crates/ffi` | UniFFI bindings exposing `core-client` to Swift |
| `crates/cli` | `grouper`: connect a device, chat, watch threads, approve, manage keys/tools/models |
| `apple/GrouperApp` | SwiftUI apps for iPhone and Mac (first draft, not yet built: see its README; needs Xcode 26) |
| `apple/GrouperKit` | Swift package wrapping the Rust core, plus the `grouper-mac` demo node |
| `examples/scripted-demo.json` | Script for the fake model, to try everything without API keys |
| `Dockerfile`, `deploy/` | Hub container for the Linux VM on the PC (later a VPS) |

## Try it without API keys

```bash
cargo build --workspace
H="target/debug/grouper-hub --data ./data"
$H device add "Mac CLI" --platform macos      # token for the device that runs threads
$H device add "Phone" --platform ios --admin  # admin token for you, the chat client
$H provider scripted --kind scripted --options "{\"script\": \"$PWD/examples/scripted-demo.json\"}"
$H model fake --provider scripted --model fake-1 --roles orchestrator,standard,thread
$H serve
```

Device tokens can connect and read. Changing tool permissions, answering approvals,
managing keys and models, chatting and steering threads need an admin token
(`--admin`, or `grouper-hub device admin <id>` later). Keep admin tokens off devices
that run threads with a shell. Thread shells never see `GROUPER_*` variables.

In a second terminal, connect a device that runs threads:

```bash
GROUPER_TOKEN=<Mac CLI token> target/debug/grouper connect
```

In a third, chat:

```bash
export GROUPER_TOKEN=<Phone token>
target/debug/grouper chat "Write two haiku, one on my Mac and one in the cloud"
```

The Mac thread's shell command waits for approval: run the `grouper approve …` it prints.

## End-to-end test

Run the full smoke test against the real `grouper-hub` and `grouper` binaries (no API keys needed):

```powershell
powershell -ExecutionPolicy Bypass -File scripts/e2e.ps1
```

```bash
scripts/e2e.sh
```

Pass `--no-build` to reuse existing `target/debug` binaries, and `--keep` to keep the temporary directory for debugging.

## Use a real model

```bash
export GROUPER_TOKEN=<your admin token>
GROUPER_PROVIDER_KEY=<key> target/debug/grouper keys set openrouter --kind openai
target/debug/grouper models add orchestrator --provider openrouter --model <model> --roles orchestrator,thread
target/debug/grouper models add free --provider openrouter --model <free model> --free --roles standard
```

Anthropic: `keys set anthropic --kind anthropic`, then models such as `claude-opus-5-5`.

## CLI

| Command | Does |
|---|---|
| `grouper login --hub <url>`, `grouper logout` | Store this device's token (DPAPI / Keychain), so `GROUPER_TOKEN` isn't needed |
| `grouper connect` | Be a device: stay online, run threads here (`--workspace`, `--no-shell`, `--mcp-config`, `--folder name=path[:rw]`) |
| `grouper files <thread> [<path> -o out]` | List or download a thread's output files, from whichever device it ran on |
| `grouper chat "…"` (`-c <id>`, `--standard`) | Talk to Grouper; prints replies and thread activity |
| `grouper watch` | Live events from every conversation |
| `grouper threads`, `grouper log <conversation>` | Threads and history |
| `grouper say <thread> "…"`, `grouper stop <thread>` | Step into a thread |
| `grouper approve <id>`, `grouper deny <id>` | Answer approval requests |
| `grouper tools list`, `grouper tools set <device> <tool> manual\|auto\|bypass` | Tool registry |
| `grouper keys list`, `grouper keys set <provider>` | Account provider keys |
| `grouper models list`, `grouper models add …` | Model registry |
| `grouper models import <provider> <model>` | Add a model from the provider's catalog (OpenRouter): prices, context, modalities, drafted use cases |
| `grouper chat --model <id>`, `grouper conversations model <id> <model\|default>` | Pin a conversation to a model |
| `grouper models stats`, `models proposals`, `models accept\|reject <id>` | What the registry learned, and its proposals |
| `grouper rules list\|add\|remove` | Priority rules for choosing models |
| `grouper rate <thread> up\|down` | Rate a thread's result |
| `grouper memory list\|add\|edit\|forget` | Long-term memory |
| `grouper skills list\|show\|forget` | Saved skills |
| `grouper usage [--by day\|provider\|model\|conversation]`, `grouper usage balances` | Tokens, cost and provider balances |
| `grouper limits`, `grouper limits set <key> <value\|off>` | Thread caps, budgets, approval and ask timeouts (all off by default) |
| `grouper sync` | Refresh the local cache; `log`, `conversations` and `threads` read it when the hub is unreachable |
| `grouper devices`, `grouper call <device> <tool>` | Presence and direct device tools |

## Swift demo node

```bash
scripts/build-apple.sh
cd apple/GrouperKit && GROUPER_TOKEN=<token> swift run grouper-mac
```

## Run the hub in the VM on the PC

See [deploy/README.md](deploy/README.md): a Hyper-V VM made by script, the hub in
Docker, and a Cloudflare Tunnel to `https://grouper.clockmillbarn.uk`.

## Scoped folders

Threads normally see only their own workspace. To let them reach real folders on a
device, list them in `<config dir>/grouper/folders.json` or with `grouper connect --folder
projects=C:\Users\you\Documents\GitHub:rw` (read-only unless `:rw`). Threads use
`@projects/...` paths. On Auto, reads there run; writes, a shell started there, or a CLI
agent working there ask you first. Paths never leave the device; the hub only sees names
and modes.

## Permissions for CLI agents

Agent tools (`codex__codex_chat`, `claude-code__claude_code_chat`, …) start on **Auto**:
Grouper sets each agent's own safety option (Codex `workspace-write`, Claude Code
`acceptEdits`, Antigravity accept-edits without all tools, Cursor sandbox on) and runs it in
the thread's workspace. A call asks you first when it would leave the workspace, asks for
a bypass mode, or contains clearly destructive instructions. Set the tool to **Bypass** and
the agent runs in its own bypass mode too. Hermes, Grok, opencode (and pi with all its
tools) can't be confined to a folder, so on Auto each of their chat calls asks you first.

## Notes

- Tokens are `<device-id>.<secret>`; the hub stores only a SHA-256 of the secret.
- Provider keys are encrypted in the hub's database (XChaCha20-Poly1305) under a master
  key kept outside the data volume (`GROUPER_MASTER_KEY_FILE`); devices hold them in memory.
- Hub threads get file tools only, until they run in sandbox containers.
- `apple/GrouperKit` links a static library so it builds with the Command Line Tools.
  With Xcode installed, switch to an XCFramework for iOS.
