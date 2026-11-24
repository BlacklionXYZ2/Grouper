# Grouper harness — design

Status: agreed 2026-11-24. The first version is built and tested with the scripted model. This doc covers the agent
harness: how a message becomes work done by an orchestrator and its subagent
threads. The overall project is in `Grouper — Proposal v3.docx`.

## Two chat modes

| | Project chat | Standard chat |
|---|---|---|
| Who answers | The **orchestrator**, which plans and hands work to **threads** | One model, doing everything itself |
| Where it runs | Orchestrator on the hub; threads on devices (or the hub) | The hub |
| Model | Registry default for `orchestrator` | Registry default for `standard`: a free model or key if one is registered |
| Good for | Real work across devices and models | Short questions and one-off chats, without spending your paid usage |

Both modes share the account's memory and history (memory itself comes later).

Project chat works like Claude's projects: one conversation with the orchestrator,
and every delegated task becomes a **thread** you can watch in a side panel.

## One loop engine, configured per role

Grouper has a single agent loop (`core-agent`) used for the orchestrator, standard
chat and API threads:

1. Send the conversation and tool definitions to the model.
2. Run every tool the model asked for, in parallel when there are several.
3. Add the results and repeat until the model stops asking for tools.

What separates roles is configuration, not code:

| | Orchestrator | Thread (subagent) | Standard chat |
|---|---|---|---|
| Sees | Conversation, devices, tool registry, model registry, thread summaries | Only its brief and its workspace | Conversation |
| Tools | `start_thread`, `message_thread`, `stop_thread`, `list_devices`, `list_threads` | Device tools (files, shell, the device's own tools) and `ask_orchestrator` | none (for now) |
| Runs on | Hub | A device, or the hub | Hub |
| Ends with | A reply to you | A summary for the orchestrator | A reply to you |

External harnesses (Claude Code, Codex, Antigravity, pi, Hermes) are a second kind
of thread, added after this first version. Their behaviour comes from the
subagent-chat plugin, and they need their CLIs installed on the device.

## Threads

The orchestrator starts a thread with a **brief**, not a transcript:

```json
{ "title": "Draft the README", "device": "macbook", "model": "optional",
  "goal": "...", "context": "...", "constraints": "...", "expected_output": "..." }
```

- **Doesn't wait.** `start_thread` returns at once. The orchestrator replies to you
  ("I've started 3 threads…") and is woken again when something happens.
- **Wake-ups:** your message, a thread finishing or failing, a thread asking it a
  question, or you stepping into a thread.
- **Where:** on the device named in the brief. The hub is also a device (`hub`).
  When a thread runs on the hub instead of its natural device (for example because
  the device is offline), the reason is recorded on the thread.
- **Workspace:** each thread gets its own folder on the device it runs on.
- **Needs another device?** A thread calls `ask_orchestrator`. The orchestrator
  either answers itself (for example by checking `list_devices`) or starts another
  thread, then replies with `message_thread`.
- **You can step in:** message a thread (it sees your message on its next turn),
  redirect it, or stop it. The orchestrator is told whenever you do, so it
  doesn't undo your change.
- **Results:** the thread's final message is its summary. The orchestrator keeps
  the summary; the full thread is in the event log for you to read.

## Devices, tools and permissions

- Every device reports its tools when it connects. The hub keeps a **tool
  registry** (SQLite): device, tool, description, permission level.
- Permission levels, per device and tool:
  - **Manual:** every call waits for your approval.
  - **Auto:** runs unless the call hits a risk rule (deleting, leaving the
    workspace, privileged or networked shell commands, sending or spending), in
    which case it asks.
  - **Bypass:** always runs.
- Defaults: file tools inside the thread's workspace start on **Auto**; everything
  else starts on **Manual**.
- **Only you change permission levels.** No model has a tool for it, so a
  prompt-injected agent can't raise its own access.
- Approvals go to every connected client; the first answer wins.

## API keys and models

- Keys belong to the account. Add one from any device; the hub stores it and
  syncs it to every device, which uses it to call providers directly. Devices
  report token usage back so cost is counted in one place.
- Revoking a device should prompt you to rotate the keys it held.
- **Providers:** `anthropic` (Messages API) and `openai` (any OpenAI-compatible
  endpoint: OpenAI, OpenRouter, DeepSeek, Qwen, Grok, local servers).
- **Model registry** (SQLite): provider, model, prices, context window, use cases,
  `free` flag, and which roles it's the default for (`orchestrator`, `standard`,
  `thread`).

## Storage

SQLite on the hub, because tools can fetch just the rows they need, which keeps
model context (and cost) small. Tables: devices, tools, providers (with keys),
models, conversations, threads, events, usage. Markdown views can be exported
later for reading.

The orchestrator's model history is stored append-only, which keeps prompt
caching and Claude's preserved thinking working.

## Events

Everything that happens is an event in one ordered log: your messages, assistant
replies, thread started / text / tool call / tool result / finished, approvals,
usage. Clients receive events live over their WebSocket and can fetch history over
HTTP, so every device shows the same chat and thread panel.

## First version (built)

In: providers (Anthropic, OpenAI-compatible, plus a scripted fake model for tests);
the loop engine; project chat with asynchronous threads; standard chat; threads on
devices and on the hub; file and shell tools; the tool registry with
Manual/Auto/Bypass and approvals; account keys synced to devices; model registry;
SQLite event log; `grouper chat`, `threads`, `approve`, `keys`, `tools` in the CLI.

Out (later): external harness threads, memory and skills, learning, the
offline-device question/timeout flow, streaming text token by token, sandboxing
hub threads in containers (hub threads get file tools only until then), keys in
the OS keychain on devices, encrypted keys at rest on the hub, the Swift chat UI,
and a web client.

## Known gaps in the first version

- Text arrives per model turn, not token by token.
- A thread on a device that disconnects is marked failed; it isn't resumed.
- Approvals have no timeout; a thread waits until you answer or stop it.
- No limit yet on how many threads run at once, or on spending.
- Thread usage is shown in `grouper chat`; the orchestrator's own usage is recorded on the
  hub but there's no command to show totals yet.

## Open questions

- Should a web client come first, to design the screens before the native apps?
- How long does a thread's `ask_orchestrator` wait before giving up?
- Spending limits per thread, per conversation and per day.
