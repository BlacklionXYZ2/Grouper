# Grouper apps — design

Status: spec, 2026-11-25. Screen design for the clients in `docs/harness.md`: iPhone first, then a Mac menu-bar node and chat window. Routes are those in `crates/hub/src/server.rs`. The device node is `apple/GrouperKit`. No new endpoints.

One project conversation: the hub orchestrator delegates to threads, and you watch and steer them. Standard chat is that transcript with one model and no threads. Memory is shared.

## What the 2026 apps actually do

**Claude.** From 20–23 November 2026 the mobile app has a tab bar: Chat, Projects, Code, Artifacts. Long-press Projects or Code for recent items. The team drew their own bar so the message field stayed primary. Home can pick a device and start a Code session there. Projects show threads and what is waiting. On iPad and iPhone Duo (29–30 October) an artifact opens beside the chat and can go full screen. [Design Compass, 23 Nov 2026](https://designcompass.org/en/2026/11/23/claude-mobile-app-tab-bar-redesign/). [AIToolRanked, 21 Nov 2026](https://aitoolranked.com/news/claude-app-redesign-tab-bar-code-tab-how-to-use). [iThinkDiff, 30 Oct 2026](https://www.ithinkdiff.com/claude-ipad-iphone-duo-artifacts-beside-chat/).

**ChatGPT.** The 19 November 2026 mobile notes put Home (chats; history stays in a sidebar), a tab for your dot (a background agent), and an Explore grid on a bottom bar. A long-press pins Codex, Projects, Images and the rest, including scheduled tasks. Canvas left GPT-5.5 on 28 May 2026: writing and code now sit in blocks in the reply, with a full-screen editor for long pieces. [The Tech Hacker, 23 Nov 2026](https://thetechhacker.com/news/chatgpt-new-mobile-app-design-pin-tab-bar/). [Android Authority, 20 Nov 2026](https://www.androidauthority.com/chatgpt-app-redesign-rollout-3737931/). [Convika, updated 16 Nov 2026](https://convika.com/guides/chatgpt-canvas/).

**Google Antigravity.** The 19 May 2026 agent manager makes a project the boundary for settings, resources and permissions, and groups the sidebar by project, status or recency. `/swarm` (10 November 2026) adds a side pane: a Board feed, a Members roster (open an agent's tools and steer it), and a Graph replay. A 19 November note says rows that need a person sort first. [Antigravity, 19 May 2026](https://antigravity.google/blog/google-io-2026-feature-deep-dive). [Antigravity, 10 Nov 2026](https://antigravity.google/blog/gemini-4-carbon-in-google-antigravity). [Meng Li, 19 Nov 2026](https://pythonlibraries.substack.com/p/google-antigravity-redesign-multi) (partly paywalled; the visible claim is that status sort).

**Orca.** The agent workspace at [orca.dev](https://orca.dev/changelog) (paired phone, GitHub `stablyai/orca`) is the close match. It runs Claude Code, Codex, Antigravity, Cursor, Hermes, Pi and others across computers, with worker indicators in orchestrator chats. The phone home is whatever needs you. Version 1.4.259 (14 Nov 2026) opens one subagent on its task, live steps and answer, then returns to the parent. Version 1.4.271 (22 Nov) edits memory and skills from the phone. [MacGrove's notes](https://macgrove.app/apps/orca/changelog) record the same phone home. Other products have used the name; this workspace is the reference.

**Muse.** In November 2026 this is Meta's agent (UK launch 23 November), which is distinct from the older Mac brainstorming app of the same name. Spaces (6 November) are mini-apps you describe and pin to a sidebar or the main tabs. The shipped design is a messaging surface, side chats per task, and three status layers: what is happening, the process, and the detail. [Meta, 23 Nov 2026](https://about.fb.com/news/2026/11/introducing-muse-in-the-uk-a-personal-ai-agent-you-control/). [Meta, 6 Nov 2026](https://about.fb.com/news/2026/11/introducing-spaces-customize-muse-app/). [The Verge, 6 Nov 2026](https://www.theverge.com/tech/1028871/meta-muse-ai-spaces). [iMasters, 12 Oct 2026](https://imasters.com/design-ux/how-meta-designed-muse-to-not-feel-like-an-ai-app).

**Liquid Glass.** Announced 9 June 2025; it ships in iOS 26, iPadOS 26 and macOS Tahoe 26. The HIG makes it the functional layer: tab bars, sidebars, toolbars and controls float above content, and content scrolls under them. Regular glass is the legible default. Clear glass is for controls on photos or video, with a 35% dark dim on bright media. Standard materials (ultra-thin through thick) divide the content layer. Reduce Transparency and Increase Contrast change the material. On iOS 26 the tab bar shrinks on scroll and expands when you scroll back. [Apple Newsroom, 9 Jun 2025](https://www.apple.com/newsroom/2025/06/apple-introduces-a-delightful-and-elegant-new-software-design/). [HIG Materials](https://developer.apple.com/design/human-interface-guidelines/materials).

## 1. Principles

**Claude.** Borrow one conversation with delegated work beside it, the device named on each piece, and a glance at what is waiting. Leave the four-tab product split and the custom tab bar. Grouper has one transcript.

**ChatGPT.** Borrow Home as a chat, history as a list, and a place for work that outlives the turn. A thread is a different object from the transcript, so it can have a side panel; a document canvas cannot, and ChatGPT has already folded canvas into the reply. Leave the pin-anything bar and the tool grid.

**Antigravity.** Borrow the roster and the sort: approvals and `waiting_device`, then running, then finished, plus a steer field on the open thread. Leave the Board and the Graph. Threads report upward, and the log has no edges to draw.

**Orca.** Borrow worker chips in the parent, one thread view you enter and leave, a phone list ordered by who needs you, and memory and skills as lists. Leave the IDE (editor, blame, terminals, worktree cleanup).

**Muse.** Borrow three depths (chip, sheet, raw events) and a transcript that reads as a conversation. Leave generated spaces, avatars and personality, so permission, cost and device stay on fixed screens.

**Liquid Glass, as specified.** Glass is the tab bar, the Mac sidebar, navigation bars, menus and the composer. Transcripts, chips, thread output, the approval reason and lists stay on the system background. Regular variant only: there is no full-bleed media, so clear glass stays unused. Use the system bars and a `safeAreaBar` composer so scroll-shrink and Reduce Transparency come from the system. Custom `.glassEffect` is a last resort on the composer.

**Grouper rules.** The composer stays available during a turn, because `POST /v1/chat` is a wake-up, and there is no orchestrator cancel. Stop belongs to a thread. Deltas (`AssistantDelta`, `TextDelta`) are live-only, `seq` 0; the stored message replaces the draft. Manual, Auto and Bypass are labelled, and every approval shows the hub's `reason`. The screens state the gaps: approvals do not time out, the hub does not cap spend, chat text is a string, and an approval is a boolean.

## 2. Information architecture

### Tokens

| Client | Secret | Transport | Runs threads |
|---|---|---|---|
| iPhone | Admin, Keychain `grouper.admin` | HTTPS and `/v1/events` | No |
| Mac chat window | Same admin item, URLSession only | Same | No |
| Mac menu bar | Device token `grouper.device`, passed only into `GrouperClient` | `/v1/connect` inside the Rust core | Yes, with a workspace |

Neither secret is an environment variable. Thread shells already drop `GROUPER_*`. Settings requires `GET /v1/providers` to succeed for the admin field and to return 403 for the device token. If the device token also succeeds, the Mac refuses to start `GrouperClient`. The iPhone never opens `/v1/connect`. App targets are iOS 26 and macOS 26. `GrouperKit` may keep its macOS 14 package floor until the iOS XCFramework; the apps require 26.

### iPhone

Four tabs on the system tab bar, which may shrink on scroll.

| Tab | Contents |
|---|---|
| Chat | The pinned project conversation. The toolbar menu holds other chats, New project and New standard. |
| Threads | Needs you, then Running, then Finished. Badge = pending approvals + `waiting_device`. |
| Devices | Presence, and the tool registry on the detail. |
| More | Models, Usage, Memory, Skills, Settings. |

The composer is on Chat only. Thread messages are typed on the thread sheet. The home pin is local. Titles are the first 60 characters of the first message, and there is no rename route.

### Mac

A `MenuBarExtra`, a `NavigationSplitView` window, and a Settings scene. The menu shows hub reachability, whether this Mac's node is connected, up to three approvals with Approve and Deny, up to five running or waiting threads, Open Chat, Pause / Resume (`GrouperClient.stop` / `start`), and Settings. Pause stops the node. It does not change permissions or sign the window out.

Wide window, three columns: a glass sidebar (Chats, Threads in needs-you order, then Devices, Models, Usage, Memory, Skills); an opaque transcript; an inspector that is the iPhone thread sheet. Standard chats hide the inspector. A narrow window presents that sheet as a sheet. No Mac tab bar.

## 3. Screens

**Loading** is a skeleton of the real layout. **Error** is an inline banner with the hub's `error` string. Confirmations are the exception: Bypass, and Stop on a running thread. **Offline** means the socket is down or `GET /healthz` failed: cached stored events stay readable, live controls disable, and the banner says this is the last copy. HTTP 401 opens Settings. HTTP 403 on an admin route says this token cannot act for the person.

### Chat

Navigation title is the conversation title. The caption is Project or Standard, then the pinned model id or "Default". While any thread in this conversation is running or waiting, a content strip under the bar reads "2 running · MacBook offline". It uses `secondarySystemBackground`, not glass.

Stored events, in `seq` order: `UserMessage`; `AssistantMessage`, with `AssistantDelta` filling a draft until the stored text replaces it; `ThreadStarted` inserting a chip that later events update in place (`ThreadWaiting`, `ThreadCheckpoint`, `ThreadHandedOver`, `ThreadFinished`, `ApprovalRequested`); `Error` as a row. `ThreadActivity` stays off the transcript. The chip's second line is the latest model text (`Text` or the open `TextDelta`), one line, plus the open tool's name. Arguments and outputs stay in the sheet. `ThreadInput` stays in the sheet.

The composer has the field, a model menu (`GET /v1/models`, filtered by role) and Send. Mode is fixed at creation (`project` or `standard` on the first `POST /v1/chat`). Changing model later is `POST /v1/conversations/{id}/model`. The hub rejects `model` on `POST /v1/chat` after the first message.

**Streaming and reconnect.** Append deltas to a memory-only draft. On `AssistantMessage` or `AgentEvent::Text`, replace the draft with the stored text. `TurnFinished` clears "Replying". Send stays enabled. Reconnect with `GET /v1/conversations/{id}/events?after={lastStoredSeq}`, then `GET /v1/events?conversation={id}` (bearer token on the upgrade). Drop stored events at or below the fetched seq: the socket cursor starts at the hub's latest seq, so the HTTP catch-up is what fills the gap, and the stored text repairs missed deltas. Persist stored events only.

**States.** No conversation: the name, one sentence that a project can hand work to the Mac, the PC or the hub, and a live composer. The first send omits `conversation_id`. Loading: skeleton, composer enabled, optimistic user text until the POST returns an id or a 400. Streaming: a caret and "Replying". A 400 puts the text back in the field. Offline: composer disabled, cache scrollable.

**Interactions.** A chip opens the sheet or the inspector. A finished chip's menu rates it (`POST /v1/threads/{id}/rating`, `up`, `down` or `clear`). Scrolled up when a stored message arrives, a glass "New messages" control jumps to the end. Standard chat is this screen with no chips and no strip. Its menu marks `free` models and prefers role `standard`.

### Thread sheet

The Mac inspector is this sheet. Header: title, status word, device, model, and `ThreadStarted.reason` when the hub recorded why this model or why the hub. Then:

- If status is `waiting_device`: device name, `last_seen` from `GET /v1/devices`, `ThreadWaiting.reason`, and the newest `ThreadCheckpoint.summary`. After `ThreadHandedOver`, list `files` and `skipped`. Status returns to Running when the thread does. The checkpoint text stays so the hand-over remains visible.
- Live output: current model text, then collapsed tool rows (name, ok or failed). Expanding shows input and output in monospace. "Show events" loads `GET /v1/conversations/{id}/events?thread={id}`.
- "Message this thread" posts `POST /v1/threads/{id}/message`.
- Stop posts `POST /v1/threads/{id}/stop`. Confirm when status is `running`. While waiting, Stop is available in the ordinary style.
- On `done`, `failed` or `stopped`: `summary`, then rating. Step-in disables.

**States.** Loading is `GET /v1/threads/{id}` plus the thread's events. Streaming shows the caret. Waiting with no checkpoint yet says the hub will work to one. A 409 on message or stop keeps the typed text and shows the error. A 404 shows the hub string. Offline uses the cache and disables both buttons. The sheet shares Chat's socket.

### Approvals

`ApprovalRequested` has `approval_id`, `device_id`, `tool`, `input` and `reason`. The reason is the risk rule. It is body text, on the sheet and in the notification, and it is never a caption under a raw command.

A banner on Chat and a row on Threads open the sheet: thread title, tool, device, reason, then `input` pretty-printed and selectable. Approve posts `{"approve": true}` to `POST /v1/approvals/{id}`. Deny posts false. Change does not patch arguments. `ApprovalResult` is a boolean. Change denies, then opens the step-in field with a draft ("Don't run that. Instead: "). A 404, or an `ApprovalResolved` that arrives first, replaces the buttons with "Answered on another device." The sheet says the call waits until someone answers or the thread is stopped, and it links to Stop. The notification's Edit action opens this sheet and does not POST by itself. The banner omits `input`.

### Devices

`GET /v1/devices`: name, platform, Online or Offline, `last_seen` when offline, tool count. Detail: `GET /v1/tools?device={id}&all=true`, so a tool the device dropped still shows, marked Unavailable. Each row has name, description, and Manual / Auto / Bypass. The write is `PUT /v1/devices/{id}/tools/{tool}` with `{"permission":"manual"|"auto"|"bypass"}`.

The line under the control:

- Manual: every call waits for you.
- Auto: runs unless a risk rule asks. For `claude-code`, `codex`, `cursor`, `antigravity` and `pi`, add that Auto uses that agent's workspace safety mode, and that leaving the workspace, asking for a bypass, or a clearly destructive instruction still asks. For `hermes`, `grok` and `opencode`: "Auto does not confine this agent. It has no safety mode, so Auto runs it unconfined."
- Bypass sticks only after a second sheet: "Always run {tool}?", every call runs, including deletes and commands outside the workspace, and only the person can put it back. The button reads "Set to Bypass".

A header states the defaults: workspace file tools and the known agent tools start on Auto; everything else starts on Manual. The hub row says hub threads have file tools only. `POST /v1/devices/{id}/call` is a closed disclosure on Mac only.

**States.** Empty: no devices yet; run `grouper connect`, or turn this Mac on from the menu bar. A failed PUT snaps the segment back. Offline shows the last list, with its fetch time, and disables writes. Refresh when the scene opens and every 15 seconds while Devices or the menu is visible. Presence is absent from `/v1/events`, so the screen uses this HTTP list even though the Mac node also receives `Presence`.

### Model registry

One screen. The More badge counts pending proposals.

**Models.** `GET /v1/models`: registry id, provider model, roles, Free, Local, context, prices per million. Detail adds cache prices, modalities and use cases. `PUT` and `DELETE /v1/models/{id}` edit and delete (delete confirms). Import is `POST /v1/models/import` (provider id and the provider's model name). A returned `proposal_id` opens that proposal. Refresh prices is `POST /v1/models/{id}/refresh-prices`. Empty state links to Settings for a key, then import.

**Stats.** `GET /v1/registry/stats` is read-only. A segment switches models and devices. Per task type: done, failed, stopped, success rate, median duration, mean cost, mean grade, net rating. A null reads "No finished threads", because a zero would invent a record. A note under the table says routing waits for three finished threads of that type.

**Proposals.** `GET /v1/registry/proposals?status=pending` (also `accepted`, `rejected`, `all`). Show field (today `use_cases`), current value, proposed text, reason, source (`stats`, `orchestrator`, `catalog`). `POST /v1/registry/proposals/{id}` with `{"accept": true|false}`, then refetch the model. A 409 means it was decided.

**Rules.** `GET /v1/registry/rules`: the person's sentence, a short structured summary, and an enabled switch (`POST /v1/registry/rules/{id}` with `{"enabled": bool}`). Add is a form: sentence, optional tag, optional task type, require or forbid (`local`, `remote`, `free`, `paid`), prefer (`cheapest`, `local`, `free`). That posts a `RuleInput`. Delete confirms, then `DELETE`. The empty state fills one example, "Sensitive data stays on this machine," with require `local`. When a thread's `reason` says a rule blocked the choice, the thread sheet shows that sentence.

**States.** Each section has its own skeleton and its own error banner. Offline: cached models and rules, writes disabled, stats marked with the fetch time.

### Usage and budgets

The segment sets `group` to `day`, `provider`, `model`, `conversation` or `thread`. `GET /v1/usage?since=&until=&group=` returns calls, input, output, cache read, cache write and cost. Default range is 30 UTC days. Opening from a chat passes `conversation`.

Snapshots from `GET /v1/usage/snapshots`: account, provider, kind, window, used amount or percent, limit, `resets_at`, source, `device_id` and age from `fetched_at`. Pull to refresh calls `POST /v1/usage/refresh`, then refetches. Age over an hour is stale. Source `estimate` is labelled estimated.

The hub enforces no spend cap. Settings stores a daily USD figure in the App Group under "Warn me at". Crossing today's `day` row shows a line on Chat and one passive notification that day. Send still sends.

**States.** Empty explains that only calls through Grouper are metered. Offline shows the last totals and snapshots with their fetch time.

### Memory and skills

Admin-only, reads included. A 403 links to Settings.

Memory search is `GET /v1/memories?q=&limit=` (default 50, max 1000). No query lists recently changed rows: content, tags, updated, `last_used` as "Last shown to a model", and a link when `conversation_id` is set. The form posts `POST /v1/memories` or `PUT /v1/memories/{id}` (omit tags on update to keep them). Forget confirms, then `DELETE`.

Skills: `GET /v1/skills?q=` and `GET /v1/skills/{id}`. Row: name, description, `success_count`, `last_used`. Detail renders `steps` and lists triggers, tools, models, devices. `PUT /v1/skills/{id}` sends a partial `SkillInput` (lists replace). `DELETE` forgets. There is no create route; the orchestrator saves skills. The empty state says so, and there is no New button.

Offline: cached lists, search against the cache, writes disabled.

### Settings

Hub URL, checked with `GET /healthz`. iPhone has the admin field. Mac has both fields, verified as in section 2. Secrets stay in the Keychain. The cache may store the device id (the text before the first dot) and must not store the secret. Forget removes the item on this device and says the hub still accepts the token until that device is removed there.

`GET /v1/providers` shows id, kind, `base_url` and `has_key`. The key is write-only: the field reads "Stored" or "Not set". Save sends `PUT /v1/providers/{id}` (`anthropic`, `openai` or `scripted`, optional base URL, key and options). Scripted sits under "For tests".

Also: appearance, Lock Screen thread titles (on), Lock Screen thread text (off), the warning amount, notification and Live Activity permission. On Mac: workspace path (default `~/Grouper/workspaces`), Allow shell (on, with a line that the registry still applies), Pause, and the Swift tools this Mac offers (`mac_info` first).

## 4. iOS system integration

**Action Button.** `AskGrouperIntent` is the Shortcut for the Action Button and the Lock Screen. With text, it opens the pinned project chat, fills the composer and sends. Without text, it opens the composer and starts keyboard dictation. The hub has no push channel, so the intent opens the app and keeps the socket up. `OpenApprovalsIntent` opens Threads on Needs you. `StopThreadIntent` is the island's Stop button.

**Live Activities.** One activity per `running` or `waiting_device` thread: title, device, status, and a phase line ("Running on MacBook", "Checkpoint on the hub", "Waiting for MacBook"). The streamed line is included only when Settings allows it, cut to about 80 characters, with no tool arguments. Compact island: glyph and device. Expanded: title, phase, Stop. Past two threads the island shows a count, while the Lock Screen keeps one activity per thread up to the system cap. End the activity a minute after `done`, `failed` or `stopped`.

**Notifications.** `grouper.approval` is time-sensitive: tool, device, reason. Approve and Deny POST in the background; Edit opens the sheet and does not POST. A 404 dismisses it. If the system will not run the background action, it opens the app. `grouper.thread` is passive for a checkpoint and a normal banner when a thread finishes or fails. `grouper.budget` is passive, once a day. The extension shares the Keychain access group and logs neither the token nor the tool input.

**Share sheet.** Text and URLs go to the App Group and open the composer of the pinned chat. The person sends. URLs become their string. Images and files are refused: "Messages are text." A share never auto-sends.

**Widgets.** After each usage fetch the app writes a non-secret App Group snapshot: today's cost, seven daily totals, running count, needs-you count, fetch time. The widget holds no token. Small: today's cost and running count, opens Usage. Medium: today, seven bars, needs-you count; opens Threads when that count is non-zero, otherwise Usage. Lock Screen circular: running count, or today's cost when nothing is running. A snapshot older than a day shows its time.

## 5. Typography, colour, motion, accessibility, dark mode

The system font. Chat uses the large title that collapses on scroll. Body style for the transcript, so Dynamic Type restyles it. Monospaced body for tool I/O and figures. Chip titles wrap to two lines and, at the largest sizes, the chip row stacks vertically. The approval reason does not truncate. The composer grows to four lines, then scrolls inside.

Semantic colours. The person's text sits on `secondarySystemBackground`. Accent is system teal, for Send, running and links. Status, always with a word and an icon: running teal, waiting orange, checkpoint teal with a flag icon, failed red, done and stopped in the secondary label. The Bypass confirm is the only red button on that sheet. The unconfined-agent line is orange. Avoid quaternary labels on thin materials.

Dark mode follows the system, with the Settings override. Check the approval sheet and the Bypass confirm in light, dark and Increase Contrast.

Motion is the system tab-bar shrink, a caret at 1 Hz (hidden under Reduce Motion; text still appends), and a fade on "New messages". No extra spring and no parallax.

VoiceOver: a chip's label is "{title}, {status}, on {device}", and its value is the preview. The streaming draft is one element labelled with the text so far. Announce on a sentence boundary or after eight seconds, and once when the stored message lands ("Reply finished"). Skip per-delta announcements. The open thread uses the same rule. Buttons read "Approve {tool} on {device}" and "Deny {tool} on {device}". A permission segment's hint is its policy line. The status word is in the label.

Reduce Transparency turns system glass solid. Add no private blur, and check the composer, tab bar and Mac sidebar with it on. Controls are at least 44 points. Stop is on the sheet. A narrow Mac window turns the inspector into a sheet.

## 6. Wireframes

### 1. iPhone project chat

```
┌────────────────────────────┐
│ Haiku task            ···  │  glass nav
│ Project · Default          │
├────────────────────────────┤
│ 2 running · MacBook offline│  content strip
├────────────────────────────┤
│ Write two haiku.           │
│                            │
│ Started both.              │
│ ┌──────────┐ ┌──────────┐  │
│ │Haiku Mac │ │Haiku hub │  │
│ │Waiting   │ │Running   │  │
│ │MacBook   │ │hub       │  │
│ └──────────┘ └──────────┘  │
│ ▌                          │
├────────────────────────────┤
│ Message          Model  ↑  │  glass composer
├────────────────────────────┤
│ Chat   Threads  Devices ·· │  glass tab bar
└────────────────────────────┘
```

### 2. Thread sheet, waiting with a checkpoint

```
┌────────────────────────────┐
│ Haiku on Mac               │
│ Waiting · MacBook          │
│ opus · Mac was offline     │
├────────────────────────────┤
│ MacBook offline · 2h ago   │
│ Checkpoint                 │
│ Drafted haiku.txt. Waiting │
│ to copy 1 file.            │
├────────────────────────────┤
│ Output                     │
│ The haiku is in the file.  │
│ ▸ write_file          ok   │
│ Show events                │
├────────────────────────────┤
│ Message this thread        │
│ [ Stop thread ]            │
└────────────────────────────┘
```

### 3. Approval

```
┌────────────────────────────┐
│ Run this tool?             │
│ run_shell · MacBook        │
│ Thread: Haiku on Mac       │
├────────────────────────────┤
│ Command reaches outside    │
│ the workspace.             │
├────────────────────────────┤
│ { "command": "rm -rf ~/old"│
│ }                          │
├────────────────────────────┤
│ Waits until you answer or  │
│ stop the thread.           │
│ [ Approve ]     [ Deny ]   │
│ Change and tell the thread │
└────────────────────────────┘
```

### 4. Mac menu and window

```
┌─ 1 waiting · 1 to approve ───────────┐
│ Hub connected · this Mac online      │
│ run_shell · leaves workspace         │
│        [Approve]  [Deny]             │
│ Haiku on Mac · Waiting               │
│ Open Chat          Pause this Mac    │
└──────────────────────────────────────┘
┌──────────┬──────────────────┬──────────────┐
│ Chats    │ Haiku task       │ Haiku on Mac │
│ ▸ Haiku  │ Project · Default│ Waiting      │
│ Threads  │                  │ Checkpoint   │
│ ! shell  │ You              │ haiku.txt    │
│ · Mac    │ Write two haiku… │ Output…      │
│ Devices  │ Grouper          │ Message…     │
│ Models   │ Started both.    │ [Stop]       │
│ Usage    │ [Wait] [Run]     │              │
│          │ Message       ↑  │              │
└──────────┴──────────────────┴──────────────┘
```

### 5. Device tools

```
┌────────────────────────────┐
│ MacBook · Online · macos   │
│ Files and known agents     │
│ start on Auto.             │
├────────────────────────────┤
│ read_file                  │
│ [ Manual | Auto | Bypass ] │
│ Runs unless a rule asks.   │
├────────────────────────────┤
│ codex__codex_chat          │
│ [ Manual | Auto | Bypass ] │
│ Auto uses workspace-write. │
│ Leaving it still asks.     │
├────────────────────────────┤
│ grok__grok_chat            │
│ [ Manual | Auto | Bypass ] │
│ Auto runs it unconfined.   │
└────────────────────────────┘
```

## 7. Build plan

iPhone calls go through URLSession. `GrouperKit` arrives at M7. Views from M1 on are what the Mac window hosts.

Admin token: chat, approvals, permission writes, providers, model writes, import, usage, memories, skills, proposals, rules, ratings, thread message and stop, conversation model, and `POST /v1/devices/{id}/call`. Any valid token may GET devices, tools, models, conversations, events, threads and `/v1/registry/stats`. Only the device token opens `/v1/connect`, and only inside `GrouperClient`.

**M0 — Client.** Keychain, hub URL, bearer header, REST, the events socket, and a reducer (draft for `seq` 0, record for `seq` > 0). Check with `GET /healthz` and `GET /v1/providers`. Cache stored events only.

**M1 — Chat.** List, project and standard transcripts, composer, model menu, streaming, reconnect. `POST /v1/chat`, `GET /v1/conversations`, `GET /v1/conversations/{id}/events`, `GET /v1/events`, `POST /v1/conversations/{id}/model`, `GET /v1/models`.

**M2 — Threads.** Chips, strip, sheet, step-in, stop, rating. `GET /v1/threads`, `GET /v1/threads/{id}`, `POST .../message`, `POST .../stop`, `POST .../rating`, and events with `thread`.

**M3 — Approvals and the phone's system hooks.** Banner, sheet, notification actions, Live Activities, the three intents. `POST /v1/approvals/{id}` plus the approval events on the socket.

**M4 — Devices.** List, detail, the three levels, the warnings, the 15-second refresh. `GET /v1/devices`, `GET /v1/tools`, `PUT /v1/devices/{id}/tools/{tool}`.

**M5 — Registry.** `GET`/`PUT`/`DELETE /v1/models` and `/v1/models/{id}`, `POST /v1/models/import`, `POST /v1/models/{id}/refresh-prices`, `GET /v1/registry/stats`, `GET` and `POST /v1/registry/proposals` and `/proposals/{id}`, `GET`/`POST /v1/registry/rules`, `POST` and `DELETE /v1/registry/rules/{id}`.

**M6 — Account.** Usage and the local warning, memory, skills, Settings and keys, widgets, share extension. `GET /v1/usage`, `GET /v1/usage/snapshots`, `POST /v1/usage/refresh`, memories `GET`/`POST` and `PUT`/`DELETE /v1/memories/{id}`, skills `GET /v1/skills` and `GET`/`PUT`/`DELETE /v1/skills/{id}`, `GET`/`PUT /v1/providers` and `/v1/providers/{id}`.

**M7 — Mac.** Menu bar, the three-column window, the device half of Settings. Link `apple/GrouperKit` as `grouper-mac` does: `GrouperClient` with the device token, workspace, `allowShell`, a `ToolHandler` (start with `mac_info`) and a `ClientListener` for status, presence and the menu's on-device thread lines. Build the static library with `scripts/build-apple.sh` until an XCFramework exists. Includes `POST /v1/devices/{id}/call`.

**Later, and absent from the v1 controls.** An iOS `GrouperKit` slice (XCFramework, non-admin token, shell off). Hub spend limits and an approval timeout. Push. Image attachments. A web client. `grouper-mac` stays the small demo of the Swift to Rust seam.
