# Models and harnesses — design

Status: draft for the owner, 27 November 2026. Design only; nothing here is built. "verify" marks a fact I haven't confirmed against the live API or CLI.

Owner's request: adding an API key or subscription account registers every model on it; API models and harnesses are separate in model selection; a harness can't be the orchestrator of a project chat (unless a sound way is found) but can be picked in a standard chat.

Verdict on that "unless": **not now; later as a Claude Code-only experiment** (section 4).

## 1. Auto-registration of API models

### Flow

- **Trigger:** `PUT /v1/providers/{id}` (`server.rs::put_provider`) saves the key, then starts a sync in the background. A failed listing never fails the key save.
- **On demand:** `POST /v1/providers/{id}/sync-models`, `grouper models sync [provider]`, and a "Refresh models" button on the provider group in `web/views/models.js`.
- **Periodic:** once a day in `usage.rs::collector_loop`. The hourly OpenRouter price refresh that already runs there stays.
- **Result:** `{added, updated, hidden, filtered, error}`, stored on the provider and shown in its group header ("212 models · synced 09:14").
- **Preset:** chosen by `options.preset`, else by the `base_url` host, else `generic`.
- **Registry id:** `catalog::derive_registry_id(model)`. If another provider already holds that id, prefix it with `<provider>-`.

### Endpoints and fields

Bases follow today's code: for `openai` kind the base includes `/v1`; for `anthropic` it doesn't.

| Preset | List call | Fields used |
|---|---|---|
| Anthropic (`anthropic`) | `GET {base}/v1/models?limit=1000`, headers `x-api-key`, `anthropic-version: 2023-06-01`; page with `after_id` while `has_more` | `data[].id`, `display_name`, `created_at`. No context or price fields (verify): both come from the price table |
| OpenAI (`api.openai.com`) | `GET {base}/models`, `Authorization: Bearer` | `data[].id`, `created`, `owned_by`. Nothing else: context and price from the table |
| OpenRouter (`openrouter.ai`) | `GET {base}/models` (already in `catalog.rs`). Prefer `GET {base}/models/user`, which respects the account's provider settings (verify) | `id`, `name`, `description`, `context_length`, `pricing.prompt/completion/input_cache_read/input_cache_write`, `architecture.input_modalities/output_modalities`, `supported_parameters` |
| Gemini (`generativelanguage.googleapis.com`, chat base `…/v1beta/openai`) | Native list: `GET https://generativelanguage.googleapis.com/v1beta/models?pageSize=1000`, header `x-goog-api-key`; page with `nextPageToken` | `name` (strip `models/`), `displayName`, `inputTokenLimit`, `supportedGenerationMethods`. Verify the stripped name is what the OpenAI-compatible endpoint accepts |
| xAI (`api.x.ai`) | `GET {base}/language-models`; fall back to `GET {base}/models` | `models[].id`, `aliases`, `input_modalities`, `output_modalities`, `prompt_text_token_price`, `completion_text_token_price`, `cached_prompt_text_token_price` (price units: verify) |
| DeepSeek (`api.deepseek.com`) | `GET {base}/models` | `data[].id` only |
| Generic OpenAI-compatible | `GET {base}/models` | `data[].id`; also accept a bare array or a `models` key (LM Studio, Ollama: verify) |

A provider with `via_device` must be listed from its host device, because its `base_url` only means something there. Use an unlisted internal MCP tool `grouper_list_models`, the same pattern as `grouper_usage_probe`.

### Chat versus non-chat

Use positive evidence where the provider gives it, a deny-list otherwise:

- OpenRouter: keep when `architecture.output_modalities` contains `text`.
- Gemini: keep when `supportedGenerationMethods` contains `generateContent`.
- xAI: `/language-models` already excludes image models.
- Everyone else: drop ids containing `embed`, `tts`, `whisper`, `transcribe`, `audio`, `realtime`, `dall-e`, `image`, `imagen`, `veo`, `sora`, `moderation`, `rerank`, `aqa`, `instruct`, `babbage`, `davinci`.

Dropped models aren't registered; the sync result counts them and the owner can still add one by hand. Also store `tools: yes | no | unknown` (OpenRouter: `supported_parameters` contains `tools`; table for the rest). A model with `tools: no` can't take the orchestrator role.

### Prices

1. OpenRouter rows: its own `pricing` (existing parser).
2. xAI: prices from the list call, once the units are verified.
3. Otherwise a built-in table, `crates/hub/src/prices.json`, compiled in: id prefix → input, output, cache prices, context window, tools. Covers Anthropic, OpenAI, Gemini, xAI and DeepSeek. Longest prefix wins, so dated snapshots inherit their family's price.
4. No hit: price 0 with `price_source: none`. The picker already says "No price set". Such a row is **never** marked `free`, because standard chat's default prefers free models.

`free` is set only when OpenRouter prices are all zero or the provider is local. Prices the owner edits by hand (`price_source: manual`) are never overwritten.

### Refresh and disappearing models

New columns on `models`: `source` (`manual`/`auto`), `hidden`, `hidden_reason` (`gone`/`user`), `last_seen`, `display_name`, `price_source`, `tools`.

- A sync updates `last_seen`, display name, context and non-manual prices. It never touches `use_cases`, `roles` or `local`.
- A model missing from **two successive successful** listings becomes `hidden: gone`. An empty or failed listing changes nothing, so an outage can't hide a whole provider.
- Hidden models leave the picker, defaults and automatic routing. The row stays, so threads, usage and outcome stats still resolve their model. Nothing is deleted.
- A hidden model that reappears is unhidden. `hidden: user` (the owner's own "hide") is never undone by a sync.
- A conversation pinned to a hidden model gets the existing "using the default model instead" event, reworded to say the provider no longer lists it.

### Role defaults

- A sync never moves a role that some visible model already holds.
- First key on an empty registry: assign `orchestrator` to the highest-ranked tools-capable model (a `rank` field in the price table), and `standard` to a free model if there is one, else the cheapest ranked model. `thread` is left to the existing fallback (`default_model_from`). Say what was chosen in the sync result.
- A role holder that becomes hidden: the existing fallback chain applies, and a Needs-you notice asks the owner to pick a new default.

## 2. Harnesses as a separate kind

A harness is a CLI agent installed and signed in on one device. It brings its own loop, tools and session store; Grouper sends it a message and gets a reply.

### Detection and model lists

The device node already loads the subagent-chat plugin (`mcp_host.rs`), whose `catalog` server reports installed CLIs, versions and models without sending a prompt. A new unlisted internal tool, `grouper_harness_scan`, wraps it and adds a signed-in check. It runs on connect, daily, and on demand. It returns names and states only; no token ever leaves the device.

| Harness | Signed in | Models (all verify) |
|---|---|---|
| Claude Code | `~/.claude/.credentials.json` or the macOS keychain item, present and unexpired (`usage_probes.rs` already parses this) | No list command. Static: `sonnet`, `opus`, `haiku` plus any full ids in the plugin's `MODELS` |
| Codex | `~/.codex/auth.json` present and unexpired (also already parsed) | `~/.codex/models_cache.json`: `models[]` where `visibility == "list"`; `slug`, `display_name`, `supported_reasoning_levels` |
| Antigravity | `agy models` succeeds | `agy models`, first tab-separated column |
| Grok | `grok models` output lacks "not authenticated / not signed in" | `grok models`, lines `* name` / `- name` |
| Hermes | provider model cache exists | `provider_models_cache.json` in Hermes's home (`{provider: {models: []}}`) plus `custom_providers` in its config |
| pi | verify | `models.json` in pi's agent directory |

The plugin also covers Cursor and OpenCode; the same mechanism registers them if wanted. "Installed but not signed in" is shown on the device page with the command to run; it registers nothing.

### Registry shape

- One **provider row per harness per device**: `kind: harness`, id `claude-code@<device>`, `options: {harness, via_device, version, signed_in}`, no key, no base URL. `via_device` reuses what exists: offline hosts are skipped by automatic choice, and a pinned model on an offline device says which device is offline.
- One **model row per harness model** under it, `source: auto`, prices 0, `free: false`, `local: false`. A harness runs on your device but the model is still a cloud model, so "sensitive → local only" rules must not match it.
- `GET /v1/models` adds two computed fields per row: `kind` (`api`/`harness`) and `device_id`.
- `providers::build` keeps rejecting `harness`: a harness is not a `Provider`. It has no tool definitions to accept and owns its own history, so it gets its own path (below).
- Disappearing models, CLI uninstalled, or signed out: hidden by the same rule as section 1.

### Running a harness turn

One device-side function, `core-client/src/harness.rs::turn(agent, model, session, message, workdir, level)`. It calls the plugin's `<agent>__<agent>_chat` tool through the local MCP host, passing the stored `chat_id` so the harness resumes its own session.

- **As a thread.** `start_thread` with a harness model. The thread's device is the harness's device; naming a different one is an error. `ThreadSpec` gains `harness: {agent, model}`, and the device's thread runner calls `turn` instead of the API loop. The brief is the first message and the working directory is the thread workspace. The final reply is the summary. `message_thread` and the user stepping in are further turns on the same session. `stop_thread` cancels the call (the host already restarts a server that won't stop). There is no `ask_orchestrator` in the first version.
- **As the standard-chat responder.** New frames mirroring the device-hosted model relay (`device_provider.rs`): `HubMessage::HarnessTurn` and `NodeMessage::HarnessDelta` / `HarnessFinished`. `PROTOCOL_VERSION` → 7. `orchestrator.rs` branches before `build_model_provider` when the resolved model is a harness. The hub still stores the visible transcript as events. The `[Memory]` block is prepended to the message as today.
- **Switching into a harness mid-chat.** Its session knows nothing of earlier turns, so the first harness turn carries a compact copy of the visible conversation.
- **Streaming.** The plugin returns the whole reply at once, so version one shows a working indicator and then the full text. Token streaming is a later package: Claude Code `--output-format stream-json`, Codex `exec --json` (probably item-level, not tokens: verify).
- **Approvals.** Grouper can't see inside a harness's own tool calls. It keeps the model from `docs/mcp.md`: the turn is one gated call, clamped by the device's permission level for that agent (Codex sandbox, Claude Code permission mode, …), with the Auto risk rules applied to the message. Hermes, Grok and OpenCode can't be confined, so each turn asks unless the owner set Bypass. A standard chat runs in an empty per-chat folder on the device, at the most confined level the agent has.
- **Usage.** Token counts from the harness's reply are recorded with cost 0 and `billing: subscription`, so they never enter the API-cost track or the spend budgets. Each turn triggers the existing subscription probe (`usage::after_device_thread`), and the plan windows stay on the Usage page's subscription track.

## 3. Selection rules

### Picker (`web/views/model-picker.js`, `apple/GrouperApp/Shared/ModelPickerSheet.swift`)

- Order: Default, Suggested, Local, **Harnesses**, More models by provider.
- The Harnesses section groups by device. Each row has a "Harness" badge, the device name and its online state: "Claude Code · opus — Gaming PC · online". It shows no price, and "Subscription" where a price would be.
- In a project chat's orchestrator picker the section is still shown, disabled, with one line: "Harnesses can run threads and standard chats, not orchestrate."
- Suggested never contains a harness in a project chat.

### Hub enforcement

1. `db.set_conversation_model`: a project conversation and a harness model → 400 `harness_cannot_orchestrate`.
2. `PUT /v1/models/{id}` and import: a harness row may hold `standard` and `thread`, never `orchestrator`.
3. `core_registry::default_model_from`: for the `orchestrator` role, drop harness rows before any fallback step, including the final "first available model".
4. `orchestrator.rs`, at the start of a turn: if the resolved model is a harness in a project chat, emit an error event and use the API default. This catches rows that predate the rule.
5. `free_alternates` never offers a harness as a fallback for an API model.

### Allowed, and role defaults

- Standard chat: yes, pinned or as the `standard` default.
- Threads: yes, when named in `start_thread`. `list_models` tells the orchestrator which entries are harnesses and where they live.
- Priority rules gain the properties `harness` and `api` (`has_property`).
- Automatic thread routing picks a harness only if the owner opted that harness in (question 2).

## 4. Can a harness be the orchestrator?

The proposal: the hub serves the orchestrator's tools (`start_thread`, `message_thread`, `stop_thread`, `list_devices`, `list_threads`, memory) as an MCP server. A harness CLI on a device connects to it and plays orchestrator; Grouper keeps the conversation, threads and approvals.

It can be made to work with Claude Code. It costs the project chat most of what makes it dependable:

- **Availability.** The orchestrator is on the hub so it is always there. A harness orchestrator lives on one device: when that device sleeps, the chat stops, and thread wake-ups queue with no one to receive them. The offline-checkpoint flow assumes the opposite.
- **Transcript ownership.** The model history would live in the harness's session files on that device. The hub would hold only visible text, so it couldn't change orchestrator model mid-chat, replay history, or keep its append-only cache guarantees.
- **Wake-ups.** Each wake is a fresh headless process resuming a session: seconds of start-up per thread event, where the hub loop takes none.
- **Streaming.** Token deltas only from Claude Code (stream-json). Codex is coarser; the rest give none.
- **Approvals.** Grouper's tools arrive through the hub, so they stay gated. The harness's own tools (shell, edit, web) do not, and they run on the device.
- **Security.** Today the orchestrator has delegation tools only, on purpose: it reads thread summaries and web-derived text, and a prompt injection there can't touch a machine. A harness orchestrator has a shell on a device unless its built-in tools are switched off. Claude Code can restrict tools to `mcp__grouper__*` (verify the exact flags). Codex can only be sandboxed read-only. Hermes and Grok can't be confined at all. The hub would also need a new authenticated MCP endpoint, where today it is only an MCP client.
- **Cost tracking.** No per-token cost, so conversation and daily budgets can't count the orchestrator.
- **System prompt.** The orchestrator prompt would be appended to a coding agent's own prompt, which pulls toward doing the work itself instead of delegating.
- **Terms.** Whether a subscription plan permits unattended orchestration: verify per vendor.

Headless and MCP support (all verify): Claude Code has both, plus tool restriction and a system-prompt flag. Codex has both, without tool removal. Antigravity and Hermes have both, without confinement. Grok: headless yes, MCP unknown. pi: no built-in MCP.

**Verdict: later, as an experiment; Claude Code only; behind a flag; not the default.** The rule in section 3 ships now. Most of the benefit arrives without it: an inexpensive API model orchestrates (little token use) while harness threads do the heavy work on the subscription. The experiment becomes worth running once package H5 exists and three things are measured on a real device: built-in tools fully off, wake latency, and behaviour when the device sleeps.

## 5. Build packages

"Device" means it needs a real device with the CLI signed in.

| # | Package | Owns | Verify |
|---|---|---|---|
| M1 | Registry columns, `kind`/`device_id` on `/v1/models`, hidden-model handling | `hub/src/db.rs`, `migrate.rs`, `core-protocol` (`ModelEntry`), `core-registry/src/registry.rs` | Unit tests: hidden rows skipped by defaults and routing; old databases migrate |
| M2 | Listing and filtering per preset; price table | new `hub/src/discover.rs`, `hub/src/prices.json` | Recorded-JSON tests per preset, as `catalog.rs` does; no live calls |
| M3 | Sync on key save, endpoint, daily tick, CLI, role defaults, two-miss hiding | `server.rs` (`put_provider`, route), `usage.rs` (tick), `cli/src/main.rs` | Scripted fake listing server; then one live key per provider |
| M4 | `via_device` listing | `core-client` (internal tool), `discover.rs` | LM Studio on a device. **Device** |
| H1 | Harness scan and registration | new `core-client/src/harness.rs`, new `hub/src/harness.rs` | Temp-home tests for signed-in states; then a real scan. **Device** |
| H2 | Hub rules 1–5 | `db.rs`, `server.rs`, `registry.rs`, `orchestrator.rs` | Unit tests for each rule; no device needed (fake harness rows) |
| H3 | Harness threads | `core-client` thread runner, `hub/src/threads.rs`, `ThreadSpec` | Start, message, stop a Claude Code and a Codex thread. **Device** |
| H4 | Harness in standard chat | `core-protocol` frames, `orchestrator.rs`, `hub/src/harness.rs` | Two-turn chat that resumes its session; device-offline message. **Device** |
| U1 | Pickers and Models page | `model-picker.js`, `models.js`, `ModelPickerSheet.swift` | Fixtures with harness rows; disabled state in project chats; hub still refuses when the UI is bypassed |
| U2 | Subscription-billed usage rows | `usage.rs`, `web/views/usage.js` | Harness turn adds no API cost and refreshes the plan window. **Device** |
| H5 | Experiment: hub MCP server and Claude Code orchestrator, flagged | new `hub/src/mcp_server.rs` | Only after the owner says go. **Device** |

Order: M1 → M2 → M3, then H2 (the rule, cheap and safe), H1, H3, H4, with U1 after H1.

## 6. Owner questions

1. **OpenRouter lists several hundred models. Register all of them?** Recommend yes, as asked; they sit under "More models" and search, and never crowd Suggested.
2. **May automatic routing give threads to a harness without being told?** Recommend off by default, with a per-harness switch "use for threads automatically", and skipped when its plan window is above 90 %. Your subscription quota is what you run short of.
3. **Hermes, Grok and OpenCode can't be confined. Allow them in standard chats?** Recommend yes, with an approval per turn until you set that device's tool to Bypass.
4. **Harness detection reads whether the Claude and Codex credential files exist and are unexpired, on every device, without the usage-probe opt-in.** Recommend allowing it: only "signed in / expired / not signed in" leaves the device, never a token.
5. **Run the harness-orchestrator experiment (H5) at all?** Recommend deciding after H3 and H4 have been in use; until then the answer in the product is no.
