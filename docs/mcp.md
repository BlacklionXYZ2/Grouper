# MCP in Grouper — design

Status: agreed 2026-11-25 (drafted with Gemini 4 Argon, reviewed by Claude). Implements the
proposal's "each node exposes its capabilities as an MCP server, and the hub is the client"
and CLI-agent subagents "through MCP (as in the subagent-chat plugin)".

## Two roles on every device

1. **MCP host (client of local servers).** A device launches local MCP servers (stdio;
   streamable HTTP later) and offers their tools to threads running on it.
2. **MCP server (to the hub).** The device is an MCP server over its existing WebSocket to
   the hub; the hub is the MCP client (`tools/list`, `tools/call`,
   `notifications/tools/list_changed`). This replaces the custom `ToolCall` / `ToolResult`
   frames and the tool lists in `Hello`.

Crate: the official Rust SDK, `rmcp` (pin the current release; features: client, server,
child-process and io transports).

## Device MCP host (`crates/core-client/src/mcp_host.rs`)

- **Config:** `<OS config dir>/grouper/mcp.json` (override: `--mcp-config` /
  `GROUPER_MCP_CONFIG`), Claude-style so existing configs paste in:
  ```json
  { "includeSubagentChat": true, "subagentChatRoot": null,
    "mcpServers": { "git": { "command": "uvx", "args": ["mcp-server-git"], "env": {}, "disabled": false } } }
  ```
- **subagent-chat by default:** root from `GROUPER_SUBAGENT_CHAT_ROOT`, then
  `subagentChatRoot`, then autodetected under `~/.claude/plugins` (folder `subagent-chat`
  containing `scripts/start.py`; newest wins). Its `.mcp.json` is loaded with
  `${CLAUDE_PLUGIN_ROOT}`/`${PLUGIN_ROOT}` bound to that root. User entries override.
- **Expansion:** `${VAR}` and `${VAR:-default}`. On Windows, `python3` falls back to
  `python` when `python3` is missing or is the Store stub.
- **Secrets:** servers never inherit `GROUPER_*` variables.
- **Lifecycle:** started when the device connects (the hub needs the tool list up front,
  and agents keep `chat_id` sessions in memory), restarted on the next call after a crash,
  stopped on shutdown. Stopping a thread cancels its in-flight call
  (`notifications/cancelled`) and, if the server doesn't stop the work, restarts that server.
- **Names:** `<server>__<tool>` (e.g. `codex__codex_chat`), valid for every provider.
- **Timeouts:** per call from `timeout_seconds` in the arguments (default 600 s, max
  1,800 s) plus a grace period, not the hub's 60 s.

## Device as MCP server to the hub

- New frames: `NodeMessage::Mcp { message }` and `HubMessage::Mcp { message }` carrying
  JSON-RPC, through an rmcp transport over the connection's channels.
- Stay custom (MCP has no equivalent): `Hello`/`Welcome`/`Ping`/`Pong`/`Error`,
  `Presence`, `Providers`, thread frames (`StartThread`, `ThreadInput`, `StopThread`,
  `ThreadEvent`, `ThreadFinished`, `AskOrchestrator`), approvals (broadcast to every
  admin client, first answer wins).
- The device lists: thread tools (when it runs threads), platform tools (Swift via UniFFI,
  CLI), and every `<server>__<tool>` from its MCP host. Workspace tools are refused outside
  a thread.
- The hub upserts the list into the tool registry on connect and on `list_changed`.
- **Platform tools get real input schemas:** `Capability.input_schema` (protocol) and
  `input_schema_json` (UniFFI). Names lose dots (`mac.info` → `mac_info`); the hub
  migrates dotted registry rows.
- `PROTOCOL_VERSION` → 4.

## CLI agents and permissions

- Tools of the agent servers (`hermes`, `antigravity`, `claude-code`, `codex`, `pi`,
  `opencode`, `cursor`, `grok`, `catalog`) default to **Auto**; other MCP tools to Manual.
- `Hooks::gate` takes `&mut Value` and runs before the `ToolCall` event, so the log shows
  the arguments that actually ran. Threads pass their `Workspace` to the gate.
- **Clamping** (agents never ask, so Grouper sets their safety arguments):

  | Agent | Auto | Bypass |
  |---|---|---|
  | codex | `sandbox: workspace-write` (or `read-only` if asked) | `danger-full-access` |
  | claude-code | `permission_mode: acceptEdits` (or `plan`/`default` if asked) | `bypassPermissions` |
  | antigravity | `mode: accept-edits`, `allow_all_tools: false` | `allow_all_tools: true` |
  | cursor | `sandbox: enabled` | `sandbox: disabled` |
  | pi | runs without asking with `tools: read-only`/`none`; asks with `all` (or none given) | `tools: all` |
  | hermes, grok, opencode | no way to confine them: every chat call asks first | run |

  A model's request for *less* access is always kept. Manual asks for every call.

- **Auto risk rules** for agent calls: `list_*` tools never ask; `*_chat` asks when the
  working directory leaves the workspace, the model asked for a bypass mode itself, or the
  message contains clearly destructive or outward-facing instructions (`git push`,
  `rm -rf`, `npm publish`, `| sh`, secrets such as `~/.ssh` or `GROUPER_`).

## Order

1. Schemas on capabilities, dot-free names, registry migration.
2. MCP tunnel: device server, hub client, `Hub::call_tool` over MCP.
3. Device MCP host with subagent-chat discovery; tools in threads.
4. Agent permission defaults, gate with `&mut Value`, clamping and risk rules.
