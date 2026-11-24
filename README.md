# Grouper

A personal, multi-provider, multi-device AI orchestrator. Design:
`Grouper — Proposal v3.docx` (the project) and [docs/harness.md](docs/harness.md)
(the agent harness). Every change is logged in [Changelogs/](Changelogs/).

## What works now

- **Hub**: devices, tokens and live presence; SQLite storage; HTTP API and a live event stream.
- **Harness**: project chat (an orchestrator that starts subagent **threads** on your
  devices or on the hub and is woken when they finish or ask something) and standard
  chat (one model). Threads run in parallel, can ask the orchestrator questions, and you
  can message or stop any thread.
- **Tools and permissions**: threads get workspace files, the shell, and the device's own
  tools. Each device's tools are in a registry with Manual / Auto / Bypass levels that only
  you can change; approvals reach every client.
- **Models**: Anthropic and any OpenAI-compatible API (OpenRouter, OpenAI, DeepSeek, Qwen,
  Grok, local servers), plus a scripted fake model for tests. Keys belong to the account and
  sync to every device. Token usage and estimated cost are recorded.
- **Swift ⇄ Rust**: the Mac demo app runs threads inside the Rust core and offers a
  Swift-implemented tool to them.

## Layout

| Path | What |
|---|---|
| `crates/core-protocol` | Wire protocol and shared types (frames, events, registry rows) |
| `crates/providers` | Model adapters: Anthropic, OpenAI-compatible, scripted |
| `crates/core-agent` | The loop engine, workspace and shell tools, permission rules, thread runner |
| `crates/core-client` | Device side: connection, presence, running threads, approvals |
| `crates/hub` | `grouper-hub`: storage, orchestrator, threads, HTTP and WebSocket API |
| `crates/ffi` | UniFFI bindings exposing `core-client` to Swift |
| `crates/cli` | `grouper`: connect a device, chat, watch threads, approve, manage keys/tools/models |
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
| `grouper connect` | Be a device: stay online, run threads here (`--workspace`, `--no-shell`) |
| `grouper chat "…"` (`-c <id>`, `--standard`) | Talk to Grouper; prints replies and thread activity |
| `grouper watch` | Live events from every conversation |
| `grouper threads`, `grouper log <conversation>` | Threads and history |
| `grouper say <thread> "…"`, `grouper stop <thread>` | Step into a thread |
| `grouper approve <id>`, `grouper deny <id>` | Answer approval requests |
| `grouper tools list`, `grouper tools set <device> <tool> manual\|auto\|bypass` | Tool registry |
| `grouper keys list`, `grouper keys set <provider>` | Account provider keys |
| `grouper models list`, `grouper models add …` | Model registry |
| `grouper devices`, `grouper call <device> <tool>` | Presence and direct device tools |

## Swift demo node

```bash
scripts/build-apple.sh
cd apple/GrouperKit && GROUPER_TOKEN=<token> swift run grouper-mac
```

## Run the hub in the VM on the PC

```bash
docker compose -f deploy/compose.yaml up -d --build
docker compose -f deploy/compose.yaml exec hub grouper-hub device add "MacBook" --platform macos
```

Point clients at `https://<your hub domain>` once the tunnel or reverse proxy is set up.

## Notes

- Tokens are `<device-id>.<secret>`; the hub stores only a SHA-256 of the secret.
- Provider keys are stored unencrypted in the hub's database and held in memory on
  devices for now (encryption at rest and the OS keychain are on the list).
- Hub threads get file tools only, until they run in sandbox containers.
- `apple/GrouperKit` links a static library so it builds with the Command Line Tools.
  With Xcode installed, switch to an XCFramework for iOS.
