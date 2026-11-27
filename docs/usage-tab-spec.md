# Usage tab — design specification

Status: design target, 27 November 2026. Written from the owner's request of the same day. No code was
changed to write this. It replaces `docs/gui-spec.md` section 5.8's page layout and the "Phone tabs are
Chat / Devices / More" decision; the two-track rule (API costs and subscription usage are separate and
never added together) and the look in `docs/gui-concept-plan.md` still stand.

Owner's request: "The costs and usage section should be a tab at the bottom which has two summary
sections." (1) API costs with a donut by API key, a Day / Week / Month meter, and token counts per key
and per model. (2) Subscription usage meters per provider, showing whatever windows each plan has.

## 1. What changes, in one page

- **Usage becomes a top-level destination everywhere**, and a tab on the phone.
- The page opens on **two summaries**: *API costs* (donut by key, key list, models under each key) and
  *Subscriptions* (one card per signed-in plan, each with its own meters).
- **Keys get a colour**, stored on the hub's provider record so every device agrees.
- Two new device probes: **Grok** (weekly credits) and **Antigravity** (two groups, each with 5-hour and
  weekly). Claude and ChatGPT probes stay as they are.
- Probing is switched on **in the app on the device that holds the sign-in**, not only by a CLI flag.
- Nothing is removed: the breakdown table, the limits and budgets editor, provider-reported balances
  and per-chat usage all keep a home (section 6).

## 2. Navigation

| Platform | Where Usage lives | Notes |
|---|---|---|
| iPhone | Bottom tab bar: **Chat · Usage · Devices · More** | SF Symbol `chart.pie` (the system shows the filled variant when selected). `PhoneTab.usage = 1`, which is the unused value between `chat = 0` and `devices = 2`. Native `TabView`, brand tint, as today. |
| Mac | Sidebar pages: **Devices · Models · Usage · Settings** | New `MacSection.usage`, same `chart.pie` symbol, added to the command palette's pages. |
| Web, wide (> 760px) | Sidebar: Devices · Models · **Usage** · Memory · Skills · Settings | Same position as today; the label shortens from "Usage & limits" to "Usage". Icon `usage-fill` (exists). |
| Web, narrow (≤ 760px) | Bottom `tabbar`: **Chat · Usage · Devices · More** | Usage leaves the More tab's view list and the More page's list. |
| Windows app | Same as web (it hosts the web client) | Plus the "read subscription usage on this PC" switch (section 5.6). |

Why second: it is the owner's most-checked page after Chat, and keeping More last matches every other
phone app. Four tabs fit at 320px with labels (each tile is at least 72px wide).

Rules:

- The Chat tab keeps the Needs-you badge. The Usage tab has **no** badge in this round (alerts are
  later work; a badge that is always lit teaches the owner to ignore it).
- Routes: `#/usage` (the two summaries), `#/usage/breakdown`, `#/usage/limits`, and the existing
  `#/usage?conversation=<id>` and `?provider=<id>` (section 6). iOS adds the URL `grouper://usage`.
- The concept plan's checkpoint 7 ("three tabs") becomes four tabs. The floating rounded bar, 18px
  container and tinted selected tile do not change.

## 3. Page layout

One scrolling page. Title "Usage". A Refresh button sits in the bar (web, Mac); pull-to-refresh on iPhone.

- **Phone and narrow web:** stacked. API costs first, then Subscriptions, then a short "More" list
  (Breakdown, Limits and budgets, Where these numbers come from).
- **Wide web and Mac (content ≥ 1100px):** two columns of equal width, API costs left, Subscriptions
  right, the "More" list full width below. Under 1100px they stack.
- Each summary is one card on `--surface` with a 1px `--border` hairline, radius 12, no shadow in dark.
  Section title 15/600, and one line under it saying what the numbers are:
  - API costs: "Estimated from calls made through Grouper."
  - Subscriptions: "Read from the apps signed in on your devices. Unofficial; may lag."
- The period selector changes section 4 only. Subscription windows have their own periods.

## 4. Section 1 — API costs

### 4.1 Period selector

A three-way segmented control at the top of the card: **Day · Week · Month**. Default Day. The choice
is remembered per device.

| Choice | Exact range | Caption under the total |
|---|---|---|
| Day | Local midnight today → now | "Today" |
| Week | Local midnight 6 days ago → now (7 days including today) | "Last 7 days" |
| Month | Local midnight 29 days ago → now (30 days including today) | "Last 30 days" |

Recommended and why:

- **Local time, not UTC.** "Today" should mean the owner's day. The client works out the boundaries in
  the device's time zone and sends them to the hub as instants, so the hub needs no time-zone setting.
- **Rolling, not calendar.** A calendar week is nearly empty every Monday and a calendar month on the
  1st, so the donut would be useless exactly when it is opened. Rolling ranges are always full and
  always comparable. Calendar month is open question 1.
- The **daily budget stays a UTC day** (that is how the hub enforces it). So it is shown as its own
  line, never as the Day total: "Daily budget: $1.20 of $5.00 · resets 00:00 UTC (01:00 your time)".
  With no budget set the line reads "No daily budget set" and links to Limits and budgets.

Custom ranges and "All time" stay in Breakdown (section 6), not here.

### 4.2 Total and donut

- **Total:** the period's estimated cost at 28/600, with an "estimated" badge beside it and the caption
  from the table above. Under it, in `--text-3`: "1.84M tokens in · 212K out · 96 calls".
- **Donut:** 180px on phone, 200px on wide layouts; ring thickness 22px; a 2px gap between segments in
  the card's surface colour. One segment per key, in the key's colour (4.4), sized by its share of the
  period's cost. Segments run clockwise from 12 o'clock, largest first; the key list below is in the
  same order.
- **Centre label:** nothing focused → the total and "All keys". A key focused → its name (one line,
  truncated), its cost, and its share ("38%").
- **Focus:** tapping or clicking a segment, or the swatch end of a key row, focuses that key: its
  segment keeps full colour and grows 3px outward, the others drop to 35% opacity, and its row
  expands. Tapping it again, the centre, or pressing Escape clears focus. `#/usage?provider=<id>`
  opens with that key focused, so reload and Back work.
- **Small shares:** a key with cost above zero always gets at least a 3° arc so it stays visible.
  With more than eight paid keys, the smallest are drawn as one "Other" segment in `--fam-unknown`;
  every key is still its own row in the list.
- **Legend:** the key list is the legend. There is no separate legend block.

Special cases:

| Case | Donut | Text |
|---|---|---|
| No calls in the period | An empty ring in `--border` | Centre "$0.00". Below: "No calls through Grouper in this period." Keys are still listed. |
| One paid key | A full ring in that key's colour, no gaps | Centre shows the total and the key's name |
| A key with $0 (free or local models only, or unused) | No segment | Its row is listed with "$0.00" and its tokens; the swatch is an outline ring, not filled |
| Every key is $0 but tokens were used | Ring sized by **tokens** per key | Centre shows total tokens. Caption: "No paid usage. Showing share of tokens." |
| A model with no price set was used | Unchanged | A warning line under the total: "3 calls used a model with no price set and are not in this total." Links to Models |
| Usage from a provider that has since been removed | Segment in `--fam-unknown` | Row named "Removed key (<id>)" |

### 4.3 Key list and models

Under the donut, a list titled "Registered keys": one row per API provider, including unused ones.
Harness (subscription) providers are not listed here; they are section 5.

Collapsed row, left to right: colour swatch (12px disc) · provider name (its id, as today; there are no
nicknames) · badges ("Local" for a provider on a device or a local address; "Low balance" when the
provider reports under 10% of its limit left) · right-aligned cost · chevron. Second line in
`--text-3`: "412K in · 38K out · 31 calls". The row's share of the total follows the cost as text
("38%").

Expanded row (the "dropdown"), in this order:

1. **Models.** One row per model used through this key in the period, largest cost first: family mark
   (the two-letter glyph in the family colour, `lib/family.js`) · model display name · badge "Free",
   "Local" or "No price set" where it applies · cost · "in / out" tokens. Under each, a 4px share bar in
   the **key's** colour showing that model's share of the key's cost. More than six models: show six
   and "Show all 14".
2. **Key totals.** "Input 412K (of which 301K read from cache, 12K written to cache) · Output 38K".
   Cache numbers appear only here and on a model row's detail, and only when they are not zero.
3. **Provider reports** (only when the hub has a balance or spend snapshot for this key): for example
   "Provider reports: $12.40 credit left of $20.00 · fetched 5 min ago", the OpenRouter free requests
   line, and the "Stale" label after an hour. This is where today's per-key balance cards move to.
4. Actions: "Change colour", "See breakdown" (opens `#/usage/breakdown?provider=<id>`).

Definitions the UI must keep to:

- **Tokens in** = input + cache-read + cache-write tokens. **Tokens out** = output tokens. Numbers are
  abbreviated (412K, 1.84M) with the exact figure in the tooltip and the accessible label.
- **Estimated** = tokens the provider reported for each call Grouper made × the registry price at the
  time of the call. It only covers calls made through Grouper. The total carries the "estimated" badge
  and the section's subtitle says it once; the rows do not repeat it. (This relaxes 5.8's "attach it to
  every figure" for this screen, because every figure in this list is the same kind of number.)
- **Provider reports** = what the provider's own balance or spend API says. It can include use outside
  Grouper. It is always labelled, always inside the key's expanded row, and never added to the total
  or drawn in the donut.
- **Free models** cost $0.00 and show the "Free" badge. **Local models** show "Local" and "$0.00".
  Neither earns a donut segment by cost. Their tokens always count in the token lines.
- **Subscription (harness) calls** are recorded with cost 0 and `billing = subscription`; they are
  excluded from this whole section.

### 4.4 Key colours

Keys have no colour today. The rule:

- **Palette.** Ten named colours, reusing the hues that already exist as family colours, so light,
  dark and increased-contrast values are already defined in `app.css`:
  `teal` (`--fam-gemini`), `purple` (`--fam-qwen`), `orange` (`--fam-llama`), `pink` (`--fam-claude`),
  `blue` (`--fam-gpt`), `green` (`--fam-nemotron`), `amber` (`--fam-ernie`), `rose` (`--fam-mistral`),
  `indigo` (`--fam-radium`), `cyan` (`--fam-deepseek`). Add aliases `--key-teal: var(--fam-gemini)` and
  so on; Apple gets the same ten in `Theme.swift`. Grok's near-white/near-black and the unknown grey
  are left out (grey is reserved for "Other" and removed keys).
- **Stored as a name, not a hex value** (`"color": "purple"`), on the hub's provider record. Each
  client maps the name to its own theme's value, so contrast modes keep working.
- **Auto-assigned** when a provider is created, and once for existing providers in creation order:
  1. If the provider clearly belongs to one maker and that colour is unused, take it: Anthropic →
     pink, OpenAI → blue, Google → teal, DeepSeek → cyan, Mistral → rose.
  2. Otherwise take the first unused colour in this order, which keeps neighbours far apart:
     teal, purple, orange, pink, blue, green, amber, rose, indigo, cyan.
  3. With more than ten keys, reuse the colour used by the fewest keys.
- **User-changeable:** "Change colour" on the key row (and on the provider in Settings) opens a row of
  ten swatches, each with its name as a label and tooltip; a colour used by another key shows that
  key's name under it. Picking one saves at once.
- **Key colour and family colour are different facts and both stay.** The key colour says *whose bill*
  (swatch, donut segment, share bars). The family mark says *which kind of model* (glyph beside each
  model name). A Claude model bought through OpenRouter shows OpenRouter's swatch colour on its bar and
  the pink "Cl" mark beside its name. Key colours are used only on the Usage tab, on provider headers in
  Models, and on providers in Settings; thread rows and model pickers stay coloured by family.
- This serves V2 of the concept plan: a normal account shows three or four clearly different hues in
  the donut, on navy.

## 5. Section 2 — Subscriptions

### 5.1 Cards

One card per **subscription account**: a provider plus a plan as the device reported it (for example
"Claude Max"). Cards are ordered by how close they are to a limit (highest used percent first), so the
one to worry about is on top.

Card header: family mark for the provider (Cl, GP, Gk, Ge) · plan name ("Claude Max", "ChatGPT Plus",
"SuperGrok", "Antigravity") · on the right, the age of the reading ("3 min ago").
Second line in `--text-3`: "Signed in on Gaming PC" or "Signed in on Gaming PC and MacBook".

Same plan on several devices: if two devices report the same provider and plan label and their shared
windows agree within 5 points, show **one** card using the newest reading and name both devices. If
they disagree, they are different accounts: show one card per device.

### 5.2 Meters

Horizontal bars, one row per window. Bars, not rings: a card can hold five windows and each needs a
label, a percent and a reset time on one line; the donut stays the page's only circle.

Each row: label · bar (6px tall, radius 3) · "62% used" · "resets in 2h 14m".

- The bar fills with **used** percent. Fill colour is the provider's family colour below 80%. From 80%
  it is `--warn` with a warning icon and "Nearly used"; at 100% `--danger` with "Used up". The state
  is always the word and icon, never the colour alone.
- Reset text: under an hour "resets in 38 min"; under a day "resets in 2h 14m"; otherwise "resets Thu
  3 Dec, 09:00" in local time.
- The UI **renders whatever windows the account reports**. It has no fixed list per plan. A Claude Pro
  card shows two rows, a Max card adds "Weekly · Fable", a plan with no 5-hour window simply has no
  such row.

Labels are built from the snapshot's `window` value, `<span>` or `<span>:<scope>` (section 7.3):

| `window` | Label |
|---|---|
| `5h` | 5-hour |
| `7d` | Weekly |
| `7d:fable`, `7d:opus` | Weekly · Fable, Weekly · Opus |
| `30d` | Monthly |
| any other span, e.g. `3h`, `14d` | 3-hour, 14-day |
| a scope with a `label` from the provider | the span, then that label |
| `kind: spend`, `window: extra` | Extra usage (paid): "$4.10 of $20.00" |
| `kind: balance`, `window: credits` | Credits: the amount |

Order inside a card or group: shortest span first, plain windows before scoped ones, then spend and
balance rows.

### 5.3 Per-provider shapes

- **Claude:** 5-hour and Weekly; per-model weekly rows when reported (Fable, Opus…); "Extra usage
  (paid)" when enabled. A plan without a 5-hour window just doesn't report one.
- **ChatGPT (Codex):** the primary and secondary windows as reported (usually 5-hour and Weekly), and
  Credits when present.
- **Grok:** one "Weekly" row from the weekly credit period (reset = the period's end). If the response
  lists more than one product, one extra row per product ("Weekly · Grok Build"); with a single product
  whose percent equals the overall figure, show only the overall row. On-demand and prepaid amounts are
  not shown until their unit is confirmed (section 7.5).
- **Antigravity:** the card is split into its **groups**, each with a 12.5/600 sub-heading taken from
  the provider's own group name: "Gemini models" with 5-hour and Weekly, then "Claude and GPT models"
  with 5-hour and Weekly. A third group, if Antigravity adds one, appears without a client change.
  Antigravity reports the share *remaining*; the probe converts it so every bar on the page means
  "used". Today's reading would show: Gemini Weekly 99.6% used ("Nearly used"), Gemini 5-hour 1% used, third-party Weekly 50% used, third-party 5-hour "Not in use".

### 5.4 States

| State | What the owner sees |
|---|---|
| Fresh (read under 1 hour ago) | Bars as above; "3 min ago" in the header |
| **Stale** (over 1 hour, device offline or probe failing) | Last values kept, bars at 60% opacity, a "Stale · read Thu 14:05" badge. Never blanked |
| **Window has reset since the reading** (reset time is in the past) | That row's bar is empty with a dashed outline: "Reset at 14:00. Waiting for a new reading." |
| **No active window** (0% used and no reset time; Antigravity reports the year 1970) | Empty bar, "Not in use. Starts when you next use it." |
| **Sign-in expired** on the device (Grok's token lasts about an hour and only the Grok CLI renews it) | Last values kept as stale, plus: "Sign-in expired on Gaming PC. Open Grok there to renew it." |
| **Not signed in** | A slim card: "Claude on MacBook: not signed in." No bars |
| **Unavailable** (endpoint changed, API-key login, enterprise plan) | A slim card with the probe's plain message, e.g. "Antigravity on Gaming PC is signed in with an API key, so it has no plan windows." |
| **Opt-in needed** (a harness is detected and signed in on a device, but reading is off there) | A slim card: "ChatGPT on Gaming PC. Usage reading is off on that device." with "How to turn it on" |
| **No subscriptions at all** | Section body: "No subscriptions found. Sign in to Claude Code, Codex, Grok or Antigravity on one of your devices and turn on usage reading there." |

Slim cards sit below full cards. "Detected" comes from the harness scan the hub already keeps per
device (`docs/models-and-harnesses-design.md`).

### 5.5 Harness tokens (small, optional line)

Calls Grouper makes through a harness are metered with cost 0. Each card may end with one `--text-3`
line: "Through Grouper today: 212K tokens in, 18K out" (follows the Day / Week / Month choice). This is
context only and is never turned into a cost.

### 5.6 Turning usage reading on

Today it is a CLI flag (`grouper connect --usage-probes`) or the desktop app's `GROUPER_USAGE_PROBES`
environment variable; the Apple node has it hard-wired off. Proposed:

- **A switch on the device that holds the sign-in**, in the app's Settings under "This device":
  "Read subscription usage on this device", with the text "Lets this device read how much of your
  Claude, ChatGPT, Grok and Antigravity plans is used. It reads the sign-ins those apps already keep
  here. They never leave this device; only percentages and reset times are sent to your hub."
- Under it, one switch per harness found on that device (all on by default), so the owner can leave
  one provider out.
- Windows app and Mac app: a native setting that updates the node's config and reconnects. CLI nodes
  keep the flag and gain `--usage-probes=claude,codex,grok,antigravity`.
- **It cannot be switched on from another device.** The phone shows each device's state and how to
  change it ("Open Grouper on Gaming PC › Settings › This device"). A hub must not be able to make a
  device start reading credentials; that stays the device owner's local act. See open question 4.
- The iPhone holds no harness sign-ins, so it has no such switch.

## 6. Where the existing pieces go

| Today (on `#/usage`) | New home |
|---|---|
| Per-key cards: Grouper's estimate | Section 1: total, donut, key rows |
| Per-key cards: provider-reported balance, spend, OpenRouter free requests | Inside each key's expanded row, "Provider reports" (4.3) |
| Subscription snapshots | Section 2 |
| Breakdown table (by provider, model, chat, thread, day; ranges Today / 7 / 30 / 90 / All / Custom) | `#/usage/breakdown`, unchanged in content, reached from the "More" list and from "See breakdown" on a key. `?provider=<id>` filters it as now |
| Limits and budgets editor | `#/usage/limits`, unchanged. The daily budget also shows as the one-line progress in section 1 |
| Source legend, last refreshed, unmeasurable providers | "Where these numbers come from" at the foot of the page (a disclosure, closed by default) |
| Per-chat usage, `#/usage?conversation=<id>` | Kept. Shows section 1 scoped to that chat (same donut and key list, period selector plus "Whole chat", default "Whole chat"), then that chat's threads table, and a "Back to chat" link. Section 2 is hidden |
| `#/usage?provider=<id>` | Kept. Opens `#/usage` with that key focused and expanded |

On iPhone and Mac, Breakdown and Limits are pushed screens from the "More" list on the Usage tab. They
may ship after the two summaries (package A2), but the rows are present from day one and open the web
page in the meantime only if the owner accepts that; otherwise they show "Coming to this app".

## 7. Data and API changes

All new endpoints are admin-only like the existing usage endpoints. All additions are backwards
compatible: new fields are optional and old clients ignore them.

### 7.1 Key colour

- Database: `providers` gains `color TEXT` (nullable). A migration fills it for existing rows using the
  rule in 4.4.
- `ProviderInfo` (`core-protocol`) gains `color: Option<String>`, one of the ten names. Returned by
  `GET /v1/providers`. It is not part of `ProviderConfig`, so it is never synced to devices and a
  `PUT /v1/providers/{id}` that replaces the config keeps the stored colour.
- `PUT /v1/providers/{id}/color` with `{"color": "purple"}` → 200 with the updated `ProviderInfo`;
  400 for a name outside the palette; 404 for an unknown provider. `{"color": null}` re-runs
  auto-assignment.
- Creating a provider assigns a colour in the same transaction.
- The hub emits its usual "providers changed" notification to clients (add one if none exists), so an
  open Usage page on another device recolours without a reload.

### 7.2 One call for the donut and key list

`GET /v1/usage/summary?since=<t>&until=<t>[&conversation=<id>]`

- `since` / `until` use the same parser as `GET /v1/usage`, which must accept an exact instant (Unix
  seconds or RFC 3339) so clients can send local-midnight boundaries. Half-open range `[since, until)`.
  `until` defaults to now.
- Only rows with `billing = 'api'` feed `total` and `keys`.

```json
{
  "since": 1795564800, "until": 1795651200,
  "total": {"calls": 96, "input_tokens": 0, "output_tokens": 0,
            "cache_read_tokens": 0, "cache_write_tokens": 0, "cost": 4.82},
  "unpriced_calls": 3,
  "keys": [
    {"provider": "openrouter", "kind": "openai", "color": "teal",
     "local": false, "removed": false,
     "totals": {"calls": 31, "input_tokens": 0, "output_tokens": 0,
                "cache_read_tokens": 0, "cache_write_tokens": 0, "cost": 1.83},
     "models": [
       {"model": "anthropic/claude-fable-5", "name": "Claude Fable 5",
        "free": false, "local": false, "priced": true,
        "totals": {"calls": 12, "cost": 1.61}}
     ]}
  ],
  "subscriptions": [
    {"provider": "claude-code@gaming-pc", "harness": "claude", "device_id": "…",
     "totals": {"calls": 9, "input_tokens": 0, "output_tokens": 0, "cost": 0}}
  ]
}
```

- `totals` everywhere has the shape of the existing `UsageTotal` without `key`.
- `keys` lists **every** non-harness provider, including those with no usage (zero totals, empty
  `models`), plus any provider id found in the usage rows that no longer exists (`removed: true`,
  `color: null`). Sorted by cost, then tokens, then id.
- `name`, `free`, `local`, `priced` come from the current registry entry for that provider and model;
  when the entry is gone, `name` is the raw model string and the flags are false / `priced: true`.
- `unpriced_calls` counts API calls in range whose model has no price set.
- `subscriptions` is the `billing = 'subscription'` usage grouped by harness provider (for 5.5).
- Implementation is two grouped queries (`provider_id, model` for each billing kind) and a registry
  join in memory. `GET /v1/usage` is unchanged and still serves Breakdown.

### 7.3 Snapshots

`UsageSnapshot` is reused. One optional field is added:

- `label: Option<String>` — the provider's own name for the scope of a window, capped at 64 bytes by
  the hub like other probe strings. Used for Antigravity's group names and Grok's product names.
  Database: `usage_snapshots.label TEXT`.

`window` grammar (already in use for Claude): `<span>` or `<span>:<scope>`, where span is a number plus
`m`, `h` or `d`, and scope is a short lower-case id. Clients group rows that share a scope **and** have
a `label` under that label as a sub-heading (Antigravity); a scope without a label is shown inline
("Weekly · Fable").

| Provider | Rows |
|---|---|
| `claude` (unchanged) | `5h`, `7d`, `7d:<model>`; spend `extra` |
| `codex` (unchanged) | `5h`, `7d` (or the reported durations); balance `credits` |
| `grok` | `7d` with `used_pct = creditUsagePercent`, `resets_at = currentPeriod.end`; per product `7d:<product>` with `label` = the product name split into words ("Grok Build") |
| `antigravity` | per bucket: `5h:<group>` / `7d:<group>` where group is `gemini` or `third_party` (derived from the bucket id prefix, lower-cased), `label` = the group's `name`, `used_pct = (1 − remaining_fraction) × 100`, `resets_at` = `reset_time` or null when it is before the year 2000 |

A window with `used_pct = 0` and no `resets_at` means "no active window" (5.4). No new field is needed.
The existing `kind: status` rows keep carrying not-signed-in / expired / unavailable per device.

`GET /v1/usage/snapshots` is unchanged apart from the new field. For "opt-in needed" the client also
needs to know which devices have a signed-in harness but no probing: add `usage_probes: bool` and
`usage_probe_providers: [string]` to each device in `GET /v1/devices` (the hub already holds the flag
on the live connection; persist the last reported value so offline devices still show it).

### 7.4 Hub probe plumbing

- `PROBE_PROVIDERS` and the device's `SUPPORTED_PROVIDERS` gain `grok` and `antigravity`. The
  device advertises only the providers switched on locally (5.6) in `grouper/usage_probes`.
- `classify_probe_provider` maps Grok and Antigravity harness threads to their probes, so a finished
  thread triggers a fresh reading as it does for Claude and Codex.
- **Cadence:** unchanged at every 10 minutes for online opted-in devices, on device connect, and after
  a matching thread. New: clients call `POST /v1/usage/refresh` when the Usage tab opens if the newest
  subscription snapshot is over 2 minutes old, and on manual refresh. The device's 30-second
  single-flight cache already protects the providers. Official balance collectors stay hourly.

### 7.5 New device probes (`core-client/src/usage_probes.rs`)

Both follow the existing rules: read-only, never refresh or modify a credential, hardcoded
destinations, redirects disabled, 15-second timeout, tokens and account ids never logged, never
returned to the hub, redacted from `Debug`, response size capped, only the listed fields parsed.

**Grok**

- Credential: the Grok CLI's stored sign-in file in the user's `.grok` folder, read-only. If the
  access token is past its expiry → status `expired` (the probe never renews it; that is the CLI's
  job). File missing → `not_signed_in`.
- Request: `GET https://cli-chat-proxy.grok.com/v1/billing?format=credits`, host hardcoded, with the
  bearer token, the user-id header from the same file, `x-grok-client-mode: cli`, the client-version
  header and a Grok CLI user agent using the installed CLI's version from the harness scan. 401 or 403
  → `expired`; any other non-200 or an unexpected shape → `unavailable` with a short reason.
- Parse: `config.currentPeriod.type` → span (`…WEEKLY` → `7d`, `…MONTHLY` → `30d`, `…DAILY` → `1d`,
  anything else → `unavailable`); `config.creditUsagePercent`; `config.productUsage[]`.
- `onDemandCap`, `onDemandUsed`, `prepaidBalance`: **not emitted** until their unit is confirmed
  (credits or currency minor units). Leave a code comment and a test fixture.
- Account label: `supergrok` unless the response names a plan.
- This is an undocumented internal endpoint and may change; failure must degrade to `unavailable`,
  never to wrong numbers.

**Antigravity**

- Runs the installed CLI: `agy -p "/usage" --output-format json`. The binary path comes from the
  harness scan, not from a caller. Fixed arguments, no shell, no stdin, 15-second timeout, output
  capped at 64 KB, the working directory is an empty temporary folder. It is answered locally and
  spends no quota; the acceptance check verifies that.
- Exit code 2 → `unavailable`: "signed in with an API key, so it has no plan windows". CLI not found →
  provider not advertised. Other failures → `unavailable`.
- Parse `command.data.groups[].{name, buckets[].{id, name, window, remaining_fraction, reset_time}}`;
  `window` "5h" → `5h`, "weekly" → `7d`; unknown window strings are skipped.
- Account label: `antigravity`.
- This replaces the local language-server approach sketched in `docs/usage-tracking.md`.

`Config` gains `usage_probe_providers: Option<Vec<String>>` beside `usage_probes: bool` (none = all
supported).

## 8. Look, accessibility and states

Look (per `docs/gui-concept-plan.md`):

- Navy surfaces, cards two tone-steps up with hairlines, no shadows in dark. The donut and the bars
  are the page's colour; the only filled teal control is the selected period segment.
- Glass is chrome only: tab bar and sidebar. The summary cards, donut and bars are opaque.
- iPhone and Mac use system materials for bars (Liquid Glass), `Theme` tokens for everything else,
  Swift Charts `SectorMark` for the donut and plain shapes for the bars. Web draws the donut as inline
  SVG (no library, no build step).
- Numbers use tabular figures so columns line up.

Accessibility:

- **Never colour alone.** Every segment has a row with the key's name, cost and percent in text, in
  the same order as the ring. Focus links the two (ring highlight plus expanded row). Segments are
  separated by gaps, and in forced-colours mode they are outlined in the system text colour.
- The donut is one image to assistive tech with a full label ("API costs, last 7 days, $4.82
  estimated. OpenRouter $1.83, 38%. Anthropic $1.61, 33%…"); the list is the operable part. On Apple
  the chart also exposes an audio graph and each row is one VoiceOver element.
- Keyboard (web, Mac): the period control is a radio group; key rows are buttons with
  `aria-expanded`; arrow keys move between rows; Enter toggles; Escape clears focus.
- Each meter is a `meter`/progress element with a text value: "Weekly, 62 percent used, resets in 2
  hours 14 minutes". Warning states add the word, not just the tone.
- Contrast: text ≥ 4.5:1; segment and bar fills ≥ 3:1 against the card in every theme (the family
  tokens already meet this; verify the ten in light mode). Touch targets ≥ 44px. Layout holds at 320px
  and at 200% text size (cost wraps under the name rather than truncating).
- Reduced motion: no ring sweep or number roll; values just change.

Loading, offline, live:

- **Loading:** skeleton ring and three skeleton rows; skeleton cards in section 2. Each section loads
  and fails on its own.
- **Error:** an inline banner in the failing section with "Try again"; the other section stays.
- **Offline:** the last copy of both sections with "Showing the copy from 12 min ago"; Refresh, colour
  changes and limit edits are disabled.
- **Live:** when a usage event arrives, re-fetch the summary at most once every 5 seconds while the
  page is visible. Reset countdowns tick once a minute.

## 9. Build plan

Packages are independent once H1's contract (section 7) is agreed; W and A can start against fixture
JSON. One owner per file. Every package appends to `Changelogs/YYYY-MM-DD.md`.

**H1. Hub: colour, summary, snapshot label** — `core-protocol`, `hub` (`db.rs`, `server.rs`,
`usage.rs`), `core-registry/src/cost.rs`, `cli/src/usage.rs` (print new windows).
- Done when: existing providers get distinct colours after migration; `PUT …/color` validates and
  persists, and a full provider `PUT` does not lose it; `/v1/usage/summary` totals equal
  `GET /v1/usage?group=provider` for the same range with subscription rows excluded; a removed
  provider and an unpriced model appear as specified; `label` round-trips through the probe store;
  device list reports the probe flags. Unit tests for each.

**D1. Device probes: Grok and Antigravity** — `core-client/src/usage_probes.rs`, `core-client` config,
`cli` flag parsing.
- Done when: parser tests pass on fixtures of today's two responses (with secrets removed) and on
  malformed input; expired Grok token → `expired` without any network call; redirect responses are not
  followed; `Debug` output and logs contain no token or user id (test); Antigravity exit 2 →
  `unavailable`; a run on the owner's PC shows the same percentages as the CLIs and Antigravity's
  quota is unchanged before and after ten probes.

**W1. Web: navigation and the Usage page** — `app.js` (nav and routes), `views/usage.js` (rewritten
as the two summaries), new `views/usage-breakdown.js` and `views/usage-limits.js` (moved code, no
behaviour change), new `css/usage.css` registered in `index.html` and `web.rs` ASSETS, `views/more.js`,
`views/settings.js` (colour picker on providers).
- Done when: at 390×844 the tab bar reads Chat · Usage · Devices · More and Usage is not in More; at
  1440×900 the two sections sit side by side; Day / Week / Month change total, donut and list
  together; every special case in 4.2 and every state in 5.4 is reachable with fixtures and
  screenshotted in light and dark; `?conversation=` and `?provider=` links still work; the old
  breakdown and limits behave as before at their new routes; keyboard and screen-reader pass per
  section 8; `node --check` and the service-worker test pass.

**A1. Apple: tab, sidebar and the two summaries** — `GrouperKit` (summary, colour, snapshot `label`,
device flags), new `Shared/UsageView.swift`, `iOS/GrouperIOSApp.swift`, `macOS/GrouperMacApp.swift`,
`Shared/Theme.swift` (ten key colours), `Shared/SettingsView.swift` (remove the "Later milestones"
Usage row).
- Done when: the Apple workflow builds both targets with no new warnings and GrouperAPI decoding tests
  cover the new payloads; the owner confirms on device: four tabs, donut focus, expand a key, the
  Antigravity card with two groups, VoiceOver reads the chart summary. List what was not seen.

**A2. Apple: Breakdown and Limits screens** — after A1; read-only breakdown first, then the limits
editor.

**P1. Desktop and Mac node: the on-device switch** — `apps/desktop/src-tauri` (setting stored in the
app's config, replaces the environment variable as the default source, reconnects the node), its
settings surface; Mac node controller likewise; `ffi` stops hard-wiring `usage_probes: false`.
- Done when: switching it on in the Windows app makes that PC's plans appear on the phone within one
  refresh, switching it off removes the device's advertisement and its cards go stale then show
  "Usage reading is off on that device"; no way exists to flip it from another device.

Order: H1 and D1 first (in parallel); W1, A1 and P1 in parallel after H1's contract is merged.
Afterwards update `docs/gui-spec.md` (3.2, 3.4, 5.8), `docs/gui-concept-plan.md` (V10, checkpoint 7)
and `docs/usage-tracking.md` (Grok and Antigravity rows) to match.

Not in this round: usage alerts and notifications, a tab badge, home-screen widgets, routing by
headroom, Cursor, admin-key reconciliation for Anthropic and OpenAI.

## 10. Open questions for the owner

1. **What Day / Week / Month mean.** *Recommend:* today, last 7 days, last 30 days, in your local
   time. The alternative is calendar week and calendar month, which match invoices but look empty at
   the start of each period.
2. **iPhone tab order.** *Recommend:* Chat · Usage · Devices · More. The alternative is Chat · Devices ·
   Usage · More, which leaves Devices where your thumb already expects it.
3. **May you change a key's colour?** *Recommend:* yes; Grouper picks a distinct one automatically and
   you can change it from ten named colours. The alternative is fixed automatic colours (less to build,
   no way to fix a pair you find hard to tell apart).
4. **Turning on usage reading only at the device itself.** *Recommend:* yes, for safety: the phone
   shows which devices have it off and how to turn it on, but cannot flip it. The alternative is a
   remote switch from the phone, which means the hub can tell a device to start reading sign-ins.
5. **Grok's reading uses an internal, undocumented Grok CLI endpoint and identifies itself as the Grok
   CLI.** It is read-only and uses your own sign-in, like the Claude and ChatGPT readings, but it is
   the least official of the four. *Recommend:* build it, on by default with the others, with its own
   switch so you can turn just Grok off.
6. **When nothing cost money (free and local models only), should the donut show share of tokens
   instead?** *Recommend:* yes, automatically, with a caption saying so. A permanent Cost / Tokens
   switch is possible later if you want to compare them on paid days too.
7. **The same plan signed in on two devices: one card or two?** *Recommend:* one card naming both
   devices when their numbers agree, two cards when they differ.
8. **On the iPhone and Mac, until Breakdown and Limits are built natively:** *Recommend:* show the
   rows as "Coming to this app" rather than opening the web page inside the app.
