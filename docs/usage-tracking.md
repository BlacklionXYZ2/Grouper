# Usage and cost tracking — research and proposed design

Status: research, 2026-11-24. Nothing built yet. Goal: Grouper always knows how much
each linked account and API key has used and has left, so you don't need to check each
one yourself, and the orchestrator can pick a provider that still has headroom.

Inspiration: **Limits: AI Usage Tracker** (iOS) shows subscription windows for Codex
(ChatGPT), Claude, Cursor, Grok and Antigravity. Open-source apps that do the same and
are worth reading before building: CodexBar (steipete, macOS, many providers, a
fallback chain per provider), OpenUsage, AI Usage (steven-mi), codenotch.

## What can be measured

There are three kinds of number, with very different reliability.

### 1. Grouper's own metering (reliable, every provider)

Every model call Grouper makes returns token counts. The harness already plans for
devices to report usage back to the hub (`docs/harness.md`, "API keys and models"),
and the model registry has prices. `tokens × price` gives cost per call, thread,
conversation, model, key and day, for every provider, including ones with no usage
API (Gemini, local servers).

Limit: it only sees calls made through Grouper.

### 2. Provider-reported API spend and balance (official)

| Provider | Endpoint | Key needed | Gives |
|---|---|---|---|
| Anthropic | `GET /v1/organizations/usage_report/messages`; cost via the Usage and Cost Analytics API (`/v1/organizations/analytics/cost_report`; the older `/cost_report` is marked legacy) | Admin key (`sk-ant-admin…`, org admins only) | Tokens by model/workspace/key, USD cost by day |
| OpenAI | `GET /v1/organization/usage/completions`, `GET /v1/organization/costs` | Admin key (org owners only) | Tokens by model/project, USD cost by day. No per-key-name grouping yet |
| OpenRouter | `GET /api/v1/key`, `GET /api/v1/credits` | The normal key | Key usage (daily/monthly/total), cap, `limit_remaining`, account credits |
| xAI API | `POST management-api.x.ai/v1/billing/teams/{team_id}/usage`, prepaid balance endpoints | Management key | Usage time series, prepaid balance |
| DeepSeek | `GET /user/balance` | The normal key | Remaining balance |
| Gemini API | none for spend | — | Use Grouper's metering; real spend is only in Cloud Billing |

These also catch spend outside Grouper (other apps on the same key). Admin keys are
powerful: store them on the hub only, never sync them to devices, and use them
read-only.

### 3. Subscription windows (unofficial, fragile)

This is what Limits shows. No provider documents these; every tool uses the same
endpoints the official clients use, with the user's own sign-in. Expect breakage.

| Account | How tools read it | Where the credential lives | Gives |
|---|---|---|---|
| Claude (Pro/Max) | `GET https://api.anthropic.com/api/oauth/usage`, `anthropic-beta: oauth-2025-04-20`, bearer = Claude Code's OAuth token | Claude Code's login (macOS Keychain, `~/.claude/.credentials.json` on Linux) | `five_hour` / `seven_day` (and per-model) `utilization` % + `resets_at`; extra-usage spend. Enterprise plans return nothing |
| ChatGPT / Codex | `GET https://chatgpt.com/backend-api/wham/usage`, bearer + account-id header from Codex's login. Fallback: `codex app-server` RPC | `~/.codex/auth.json` | Primary (5 h) and secondary (weekly) windows, % used, reset times, credits |
| Cursor | `GET https://cursor.com/api/usage-summary` with the `WorkosCursorSessionToken` cookie | Browser cookie (a `crsr_…` API key can't read it). Teams: official Admin/Analytics API | Plan usage %, on-demand spend |
| Antigravity | Local language server: `…/LanguageServerService/GetUserStatus` on localhost; port and CSRF token parsed from the process arguments | Only while Antigravity is running on that device | Per-model quota remaining, prompt credits, reset times. Shown numbers sometimes disagree with real 429s |
| Grok (grok.com) | Since March 2026 grok.com no longer returns remaining queries in its responses; trackers now estimate locally | — | Not measurable; skip or estimate from Grouper's own use |

**Terms of service:** these are read-only calls with your own sessions, which is how
all these apps work, but they are not sanctioned APIs. Don't reuse another app's
OAuth client to log in (Google's terms specifically warn that this can suspend
Antigravity accounts). Read the credentials the official tools already have instead.

## How it fits Grouper

The architecture already suits this: subscription credentials live on devices, and
devices already expose tools to the hub.

1. **Device usage probes.** Each device registers `usage.<provider>` tools for the
   accounts it can see (Claude Code login, Codex login, a running Antigravity, Cursor
   cookie). The tool reads the local credential, calls the endpoint, and returns a
   normalized snapshot. **Subscription tokens never leave the device**, unlike API
   keys, which are account-wide. On the Mac this can be Swift (Keychain access) behind
   the same tool interface.
2. **Hub collectors.** The hub polls the official endpoints in section 2 with keys
   it holds (OpenRouter, DeepSeek, and admin keys if you add them).
3. **One table.** SQLite `usage_snapshots`:
   `account, provider, kind (window|spend|balance), window (5h|7d|month|…), used_pct,
   used_amount, limit_amount, currency, resets_at, source (probe|admin_api|metered|
   estimate), device_id, fetched_at`. Plus a `usage` table of Grouper's own per-call
   metering (already planned).
4. **Scheduling.** Poll subscription windows every ~5 min while a device is online
   (and right after Grouper uses that provider), admin APIs hourly. Offline devices
   keep their last snapshot, shown with its age.
5. **Alerts.** Notify at thresholds (e.g. 80 %, 95 %), when a window resets, and when
   balances run low. Pushed as events to every client, like everything else.
6. **Routing.** The orchestrator gets a `list_usage` tool and the model registry can
   prefer providers with headroom: e.g. send a thread to Codex when Claude's 5-hour
   window is at 90 %, or keep standard chat on a free key.
7. **UI.** A Limits-style panel and widgets in the Apple app later; `grouper usage`
   in the CLI first.

Each probe should be a small adapter with a fallback chain (as CodexBar does) and a
clear "unavailable / stale / estimated" state, because the unofficial endpoints will
change.

## Suggested order

1. Grouper's own metering + `usage` table + `grouper usage` (needed anyway).
2. OpenRouter and DeepSeek collectors (official, normal keys, matches the free
   OpenRouter key you're planning to add).
3. Claude and Codex probes on the Mac (the two most useful windows, both read from
   CLIs you already use).
4. Antigravity probe, then Cursor (cookie handling is the awkward part).
5. Anthropic/OpenAI admin-key collectors if you want reconciliation of total spend.
6. Alerts, routing, then the app UI and widgets.

Grok subscription usage: leave out unless grok.com exposes it again.

## Open decisions

- Probes in Rust (portable to Linux/Windows devices) or Swift on Apple (Keychain is
  easier)? Likely Rust with a Swift Keychain helper on macOS.
- Do you want to give Grouper admin keys for Anthropic/OpenAI, or is metering enough?
- Cursor: read cookies from a browser automatically, or paste the cookie once?
- Poll intervals and alert thresholds.
