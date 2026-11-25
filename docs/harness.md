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

Both modes share the account's long-term memory: relevant memories are added to each turn
automatically (see "Memory and skills" below).

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
  When a thread runs on the hub instead of its natural device, the reason is recorded.
- **Device offline:** the thread waits for the device (`waiting_device`). Meanwhile the hub
  is told: "the target device is disconnected; define the next checkpoint, implement it
  here, then stop and wait". It works to that checkpoint in a hub workspace (file tools
  only) and writes a checkpoint report. When the device reconnects, the hub copies the
  workspace files over (approval first if `write_file` is Manual) and the same thread
  continues on the device from the checkpoint. A device that drops mid-thread goes the same
  way, with its progress so far added to the brief.
- **Workspace:** each thread gets its own folder on the device it runs on. A device can
  also offer **scoped folders** (`@name/...`, read or read/write) for real work on your
  files; reads run on Auto, writes and shells there ask.
- **Files back:** `grouper files <thread>` lists and downloads a thread's output from its
  device; the orchestrator can quote one with `read_thread_file`.
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

### Models on a device

Set `ProviderConfig.options.via_device` to a device id or name to host a provider there, for example `grouper keys set local --kind openai --base-url http://127.0.0.1:45123/v1 --via-device "Gaming PC" --no-key`. Its `base_url` is interpreted on that device (so a loopback LM Studio URL stays private); hub calls and threads on other devices stream through the host's existing outbound connection, while threads already on the host call it directly. Disconnects and cancellation stop the relayed call, offline hosts are skipped by automatic routing, and an explicitly selected model reports which device is offline.

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

## Memory and skills

- **Memory** lives in the hub (SQLite with full-text search). The orchestrator has
  `remember`, `recall`, `update_memory` and `forget`; each turn's text is searched and up
  to six matching memories are appended to that turn as a `[Memory]` block (old messages are
  never rewritten, so caching still works). Standard chat gets the recall only. You manage
  memories with `grouper memory`.
- **Skills** are procedures the orchestrator saves after a successful task (`save_skill`):
  steps, tools, and the models and devices that did them. The two best matches for a turn
  are offered as a `[Skills]` block.

## Model registry learning

- Every finished thread records an outcome: model, device, task type, status, duration,
  cost, the orchestrator's grade (`grade_thread`) and your rating (`grouper rate`).
- Stats per model, device and task type feed model choice: when the orchestrator doesn't
  name a model, priority rules filter first (e.g. "sensitive → local only", which can
  refuse), then proven models for the task type win. The reason is recorded on the thread.
- When stats clearly disagree with a model's use cases, the hub (or the orchestrator)
  proposes new text; you accept or reject it (`grouper models proposals`), so the table
  doesn't drift.

## Accounting

Every model call records tokens and an estimated cost (cache-aware, per provider kind or
the model's own cache prices). `grouper usage` totals them by day, provider, model or
conversation; OpenRouter and DeepSeek balances are polled hourly. The orchestrator can read
recent totals with `list_usage`.

## Built so far

Providers (Anthropic, OpenAI-compatible, scripted) with token streaming; the loop engine;
project chat with asynchronous threads; standard chat; threads on devices and the hub; file
and shell tools (PowerShell on Windows); the tool registry with Manual/Auto/Bypass and
approvals; account keys encrypted at rest and synced to devices; device tokens stored with
the OS (`grouper login`); memory, skills, registry learning and priority rules; usage and
cost reporting; MCP between devices and the hub and on each device, with the
subagent-chat CLI agents as default device tools ([mcp.md](mcp.md)).

The offline-device checkpoint flow. Model rows can be imported from OpenRouter's catalog
(prices, context, modalities; use cases drafted for you to accept), and a conversation
can be pinned to a model.

Later: limits on threads and spending,
sandboxing hub threads (file tools only until then), the Swift apps, and a web client.

## Limits and timeouts

All off until you set them (`grouper limits set <key> <value|off>`): running threads
(account, per device), budgets per thread, conversation and UTC day (with a warning at
80 %), how long an approval waits before it's denied or the thread stopped, and how long
`ask_orchestrator` waits (30 minutes by default). Spend is estimated from the registry's
prices.

## Clients offline

`core-sync` keeps a local copy of conversations, threads, events and memories in SQLite:
it catches up by `seq`, follows the live stream, fills gaps and survives reconnects. The
CLI reads it when the hub is unreachable (`grouper sync` refreshes it); the apps get it
through UniFFI (`SyncStore`). Only reading works offline: sending messages while offline
needs a decision on how they merge back (open question below).

## Known gaps

- Auto's shell rules are a blocklist: they ask more than needed for ordinary words, and
  obfuscated commands can get past them. Use Manual for shells you don't trust.

## Open questions

- Should a web client come first, to design the screens before the native apps?
- Offline writes: queue messages typed offline and send them on reconnect, or answer them
  on the device with an API model and merge the reply into the hub's log? How do they
  order against events that happened meanwhile, and who wins on memory edits?
- What values do you want for limits, budgets and timeouts? (Mechanisms exist; all off.)
