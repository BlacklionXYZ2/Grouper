# Grouper GUI specification

Status: design target, 25 November 2026. This is a build specification, not a description of the current client. The web app exists and supplies most of the behaviours and routes below; the target reorganizes its navigation and makes the provider/key hierarchy and usage accounting first-class. Apple apps are planned SwiftUI clients and should preserve the same information architecture.

## Owner decisions (25 November 2026)

These override anything below that disagrees.

- **Usage tracks two separate things:** API costs per provider key, and subscription usage/quota (plan windows such as Claude's or ChatGPT's, read on devices). They are separate sections and are never added together.
- **One key per provider.** A provider *is* a connection: "Fable through OpenRouter is different to Fable through Claude". Models are grouped by provider only; there is no per-key level beneath it, and no connection nicknames.
- **The model picker is a covering panel** (full-height sheet on phone, large overlay panel on desktop) with search and provider groups, because there are many candidates. Not a small popover.
- **A thread opens as a chat-style view:** the same transcript layout as a chat, with the model fixed to the one the orchestrator chose (shown in the header, not changeable), so the owner can see which model is doing what.
- **Thread rows in the threads panel** show the title, time elapsed (live while running, total once finished) and the model, with a colour accent by model family.
- **Navigation:** no Threads tab. Phone tab design is being brainstormed separately (`docs/gui-nav-ideas.md`).

## Owner decisions (26 November 2026)

These override anything below that disagrees, including the 25 November list.

- **Phone tabs stay Chat / Devices / More** for now. No "Needs you" tab.
- **More opens to a plain list** of destinations (not a dashboard).
- **"Needs you" is a system notification, not a destination you go and check.** A new approval raises an OS notification (push); tapping it opens the chat (or thread) the request came from, with the approval shown as a popup that takes the composer's place at the bottom, like Claude's permission prompts, which replace the keyboard with what's being asked and the options. The inbox page stays reachable from More and the sidebar as a list.
- **Desktop command palette (Ctrl/⌘K):** build it. A search box that jumps to any chat, thread, model, device or page and runs common actions from the keyboard.
- **No concept art yet;** visual details stay provisional.

## Scope and reading guide

- Grouper is a single-owner, multi-device AI orchestrator. The hub owns account state; devices run threads and expose tools; the owner chats with an orchestrator in project chats or directly with one model in standard chats.
- A **chat** is the conversation. A **thread** is delegated work attached to exactly one project chat. It is never a peer conversation or a global destination detached from its parent.
- Structure and behaviour in sections 2–5 are normative. Visual details marked **PROVISIONAL** are defaults for implementation and may be replaced by owner concept art without changing information architecture, safety semantics, or interaction contracts.
- The reference section describes observable/reference-documented patterns, not a mandate to copy any product wholesale. URLs are included inline for review.
- Current web client routes use hash navigation. The proposed route table retains compatible chat/thread paths and removes the standalone thread destination as the normal way to browse.
- No code is changed by this document. Unresolved product choices are collected at the end.

## 1. Reference research

### Claude (claude.ai and Claude desktop)

- **Borrow — one conversation, work beside it.** Claude's artifact experience opens a work surface beside the conversation; created artifacts can be found from a dedicated sidebar area and edited through conversation or directly in the artifact. This is a good model for keeping Grouper's conversation primary while presenting the thread inspector as adjacent supporting work. [Claude Help: artifacts](https://support.claude.com/en/articles/17153992-what-are-artifacts-and-how-do-i-use-them)
- **Borrow — projects as durable context.** Projects group files, instructions, memory and work, and are available across surfaces; in Cowork, a project can start either a chat or a work session. Grouper's project chat is not the same entity as Claude's project container, but its stable context boundary is a useful analogy. [Claude Help: projects in Cowork](https://support.claude.com/en/articles/14116274-organize-your-tasks-with-projects-in-claude-cowork)
- **Borrow — cross-surface continuation.** Claude describes starting, steering and reviewing tasks across desktop, web and mobile, including resuming a session elsewhere. Grouper should make a thread's owning device and current reachability explicit while allowing the owner to inspect and step in from another client. [Claude Help: Cowork surfaces](https://support.claude.com/en/articles/15520349-use-claude-cowork-on-web-desktop-and-mobile)
- **Borrow — clear generated-output navigation.** Artifacts remain discoverable outside the turn and open alongside chat. For Grouper, apply that to thread output files and completion summaries, without turning every file into a new top-level destination.
- **Model picker.** Current reliable documentation located for this research did not establish a stable, detailed Claude model-picker layout. Do not infer a specific design; use the general principle of a concise in-composer selector with a clear current choice.
- **Settings.** Claude documentation distinguishes project settings/context from account-wide settings. Grouper should likewise separate chat-scoped model pinning from global provider credentials, device access and budgets.
- **Mobile.** The same task can be started, steered, reviewed, resumed, and have files previewed across supported surfaces. Borrow the task-continuation principle, not any unverified tab-bar details.
- **Materials.** Claude is a product reference for calm, content-led surfaces; no claim is made here about its use of Apple's Liquid Glass.
- **Avoid.** Do not split Grouper's one orchestrator conversation into product-mode tabs, hide device provenance, or imply that a generated file is a thread itself. Do not copy undocumented UI details from screenshots or third-party descriptions as fixed requirements.

### ChatGPT

- **Borrow — history and projects are navigational structure.** Projects keep related chats, files and instructions together; existing chats can be moved into a project where supported. This supports a clear chat history with project chats differentiated from standard chats, while avoiding an extra Threads destination. [OpenAI Help: Projects in ChatGPT](https://help.openai.com/en/articles/10169521-projects-in-chatgpt)
- **Borrow — model choice belongs near composition.** Keep the selected/default model visible beside the composer and make changing it discoverable without interrupting the conversation. This spec groups choices by provider/API-key identity instead of flattening them into an arbitrary model list.
- **Borrow — artifacts/canvas as a sibling work surface.** ChatGPT's project help describes Canvas as a tool for drafting documents, code or layouts. Grouper has no equivalent general canvas today, but its inspector should provide an adjacent, navigable surface for a selected thread's activity, files and result.
- **Borrow — responsive history.** OpenAI's release notes describe a floating mobile sidebar that closes on navigation and improved mobile keyboard/composer behaviour. Use a dismissible history drawer on phone, and avoid stealing focus into the composer when the owner switches chats. [ChatGPT release notes](https://help.openai.com/en/articles/6825453-chatgpt-release-notes)
- **Borrow — don't force scroll-follow.** The release notes describe not auto-scrolling a newly initiated conversation to the bottom while it generates. Grouper should preserve the reader's position and show a “New activity” affordance when the owner has scrolled up.
- **Settings.** Use the predictable split between conversation controls and account settings; keep provider credentials and security-sensitive controls in Settings, not in the chat model menu.
- **Avoid.** No “Explore” tool grid, arbitrary user-pinned bottom tabs, or pseudo-project abstraction that duplicates Grouper's single conversation. A standard chat remains a chat, but clearly states that it has no threads.

### Google Antigravity

- **Borrow — progressive agent/task visibility.** Antigravity's documented Agent side panel shows step-by-step progress, edited files and a bottom toolbar for artifacts, terminal processes and review changes. Its App overview panel organizes generated work into sections and lets the owner open it in context. Grouper should similarly offer a compact thread summary in the chat and detailed activity only in the thread inspector. [Antigravity Docs: Agent side panel](https://antigravity.google/docs/agent-side-panel) and [Side panel and Overview panel](https://antigravity.google/docs/side-panel)
- **Borrow — task-specific inspector.** The overview-panel pattern is a strong analogue for a selected thread's files, output, tool activity and device/checkpoint status. Remember whether the inspector is open per chat only if that does not obscure new approval attention.
- **Borrow — model selection in the task context.** Antigravity documents model selection in the Agent side panel's prompt input. Grouper's picker belongs with the composer, scoped to the role appropriate to the chat mode.
- **Borrow — progressive disclosure.** Hide empty categories, surface active/relevant work first, and let the owner expand raw details (tool input/output, event log) on demand.
- **Mobile layout.** The current Antigravity documentation distinguishes conversation from side panel and describes opening work alongside it. No reliable mobile-specific layout details were established for this research; do not guess from desktop screenshots.
- **Settings.** Antigravity documents settings and project-scoped resources/permissions. Borrow the separation of project context from global defaults; Grouper permissions remain per device/tool and only the owner can edit them.
- **Avoid.** No Board or Graph view as the default: Grouper's event protocol records a parent chat and independent threads, not a task dependency graph. Avoid IDE affordances (diff acceptance, terminal tabs, workspace Git controls) unless Grouper later implements their underlying capabilities.

### Orca (orca.dev agent workspace; not other products named Orca)

- **Borrow — focus one subagent and return to the parent.** Orca's 14 November 2026 changelog says a subagent can open in its own view with task, live steps and final answer, then return to the main conversation. This maps directly to Grouper's thread inspector and parent-chat relationship. [Orca 1.4.259 changelog](https://orca.dev/changelog/1-4-259)
- **Borrow — owner attention first.** Orca's official changelog describes phone “Needs you” handling, agent requests across paired computers, and on 22 November phone handoff of a session from History to another agent. Put approvals and waiting-device conditions before routine running/completed items.
- **Borrow — mobile management breadth.** Orca changelog notes phone management of memory and skills and a home that separates Feed, Usage, Automations and Settings. Grouper should expose memory and skills on the phone, but its compact bottom navigation must not add a Threads tab; thread lists belong to a chat or the global attention inbox.
- **Borrow — state and interruption continuity.** Recent changelog entries describe session continuation, needs-input alerts, accurate waiting states and phone controls. Grouper should make pending owner decisions durable and visible across connected clients.
- **Model picker.** The reviewed changelog does not specify a stable model-picker layout. Borrow no particular control; keep provider/key grouping as Grouper's own product rule.
- **Settings.** Use Orca as evidence that deep agent preferences can be managed on mobile, not as a reason to bury safety settings.
- **Avoid.** Do not reproduce an IDE or Git workflow. Orca's changelog is fast-moving; treat individual features as time-stamped evidence, not a permanent interface contract. This research identifies Orca by orca.dev; other “Orca” products are out of scope.

### Muse (Meta's Muse personal agent, not the older Mac app)

- **Borrow — human-readable action accountability.** Meta describes Muse showing what it has done and plans to do, and asking before important actions. Grouper must show the exact tool, device, safety reason and arguments before an approval, and keep the history accessible after the decision. [Meta Newsroom: Muse in the UK, 23 November 2026](https://about.fb.com/news/2026/11/introducing-muse-in-the-uk-a-personal-ai-agent-you-control/)
- **Borrow — side chats for separate tasks, not detached delegated work.** Muse's side chats keep different user tasks organized. In Grouper, the analogous organizing object is the chat; the delegated thread stays attached to that chat and does not become an unrelated conversation.
- **Borrow — long-running work may outlive the screen.** Muse describes work continuing after closing the app and returning when it needs approval or has a result. Grouper should retain live status in navigation and make “Needs you” reachable from every screen.
- **Borrow — memory is owner-controlled.** Meta describes the owner being able to ask the agent to remember or forget something. Grouper's memory screen must make each item searchable, editable, attributable and forgettable.
- **Borrow — settings for access.** Muse describes choosing connected apps and their access. Grouper's device page should make scoped folders, tool permissions, and their effective reach legible.
- **Mobile layout/materials.** Meta's product announcement establishes availability on mobile but does not document detailed layout or material behavior. No visual pattern is asserted here.
- **Avoid.** Do not copy the mascot/personality layer, generated mini-app “Spaces”, proactive suggestions, or a cloud virtual-computer assumption. Grouper's defining promise is explicit devices, providers, permissions and owner-controlled infrastructure.

### Apple's Liquid Glass

- **Borrow — a functional navigation/control layer.** Apple's HIG says Liquid Glass is for controls and navigation floating above content, with content allowed to scroll beneath; it explicitly says not to use it in the content layer. Use that as a material rule, not as decoration. [Apple HIG: Materials](https://developer.apple.com/design/human-interface-guidelines/materials)
- **Borrow — regular glass for text-bearing navigation.** Apple describes regular Liquid Glass as adapting luminosity/blur for legibility and recommends it where backgrounds could reduce legibility or components contain substantial text. Use regular glass for sidebars, toolbars, sheets' navigation chrome and floating controls.
- **Borrow — clear glass only over rich media.** Apple reserves clear glass for visually rich backgrounds, with a possible 35% dark dim on bright media. Grouper's transcript is not a media background; do not use clear glass.
- **Borrow — solid/content-layer distinction.** Use standard materials or opaque surfaces for transcripts, tables, approval reasons, thread output, file lists and settings forms.
- **Borrow — adapt to settings.** Follow Reduce Transparency and Increase Contrast; system materials may change, so use semantic system colors and test actual combinations. [Apple HIG: Color](https://developer.apple.com/design/human-interface-guidelines/color)
- **Avoid.** Avoid applying blur to every card, translucent text-heavy tables, custom glass replicas that fight SwiftUI/AppKit/UIKit system controls, and relying on translucency for status or contrast.

### Reference synthesis

1. The conversation remains the primary surface; delegated work opens beside it or as a full-screen child on narrow screens.
2. Show a concise task-state preview inline, but put tool arguments, activity history and files in the inspector.
3. “Needs you” outranks “running”; a pending approval must remain globally discoverable.
4. Mobile changes presentation, not the parent-child relationship between chat and thread.
5. Model selection is contextual, provider/key-grouped and explicit about free/local/paid and availability.
6. Safety, provider attribution and usage estimates must not be hidden behind decorative materials.

## 2. Product principles

1. **Chat owns threads.** A thread has one parent project chat. Open it from a chat card, chat menu/panel, approval, or deep link; never make a persistent bottom tab called Threads.
2. **Conversation first, work second.** Keep the transcript readable and the composer reachable. Thread activity is a compact card in the transcript and a rich inspector on request.
3. **Attention before activity.** Sort approvals first, then waiting for device/checkpoint, then running, then completed. Within each group sort newest activity first.
4. **Trust is visible.** Always name the device, model, provider/key, permission level, approval reason and estimate source when relevant. Never suggest estimated cost is provider-billed truth.
5. **Provider identity is a real hierarchy.** Every model list is grouped first by provider, then by configured API-key/account label (provider connection), then model. No alphabetical “random soup” across providers.
6. **Safe defaults are explainable.** Manual/Auto/Bypass must be shown with plain-language consequences. Bypass is a dangerous account-level choice and requires explicit confirmation.
7. **One event stream, honest states.** Distinguish live deltas from persisted events. A cache is readable offline, but never pretend an offline write succeeded.
8. **Progressive disclosure, not hidden capability.** Summary first; details expandable; raw event and tool payloads available, selectable, and clearly labeled.
9. **Use system conventions.** Keyboard and screen reader support are part of the design. Native Apple clients should use native navigation, materials and controls.
10. **Concept art changes skin, not semantics.** New illustration, color, shape or brand treatment cannot weaken legibility, provenance, focus order or safety.

## 3. Information architecture

### 3.1 Desktop web layout

- Three regions when viewport width permits: navigation sidebar (240–280 px), main view (flexible, transcript capped at a readable width), optional thread inspector (360–440 px).
- Sidebar is persistent on wide desktop. It contains:
  - Grouper mark/name and New chat action.
  - Chats: recent project and standard chats, with mode and small active-thread/approval indicators. Search/history opens a larger searchable list.
  - Needs you: a global count/entry for pending approvals and `waiting_device` threads. This is an attention filter, not a standalone thread catalog; selecting an item opens its owning chat and thread inspector.
  - Management destinations: Devices, Models, Usage & limits, Memory, Skills, Settings.
  - Connection state at the bottom; admin/device-token access level is visible in Settings, not inferred from color.
- No “Threads” tab at the bottom and no peer-level thread area in the navigation hierarchy.
- On a chat route, the thread panel is available from the chat header (“Threads · N”) and from the inspector toggle. Panel contents are scoped to the currently open project chat.
- On a standard chat, thread controls are omitted or disabled with an explanatory label; there are no threads by contract.

### 3.2 Phone layout

- No persistent Threads bottom tab. Use a four-item bottom bar: Chat, Needs you, Devices, More. “More” contains Models, Usage & limits, Memory, Skills and Settings.
- Chat tab opens the pinned/recent chat. The top navigation menu opens searchable chat history and New project / New standard actions. Switching chat closes the history drawer.
- Needs you is a global attention inbox of approval and waiting items. Every row displays the owning chat title; selecting it navigates to that chat and opens the thread inspector or approval sheet.
- A thread opened from a chat is a full-height sheet/navigation destination on phone with a clear Back to chat action. The parent chat route remains in navigation state.
- Management lists are one-column, content-first rows. Settings forms stack; wide tables become summary cards plus an optional detail sheet.
- The bottom bar does not cover the composer or action buttons. Respect safe areas and software keyboard; composer remains above the keyboard.

### 3.3 Inside a chat

- Chat header: title, mode, current/default model, chat menu, and thread-panel button for project chats.
- Transcript: owner messages, orchestrator messages, inline thread cards, approval notice, budget warning and errors, in event order.
- Thread cards summarize title, status, device, model, latest useful line and any pending approval. No raw tool arguments in the card.
- Thread panel: list scoped to this chat; groups Needs you / Running / Finished; opening an item selects it in the inspector.
- Thread inspector: task brief/provenance, current state, checkpoint/hand-off, streamed activity, tool calls/results, files, final summary/rating, step-in composer, Stop.
- Chat menu: rename only if/when supported; chat usage shortcut; New chat; mode/model details. Do not use it as the only thread entry point.

### 3.4 Destinations and routes

Routes below are canonical intent; web hash routes can retain their existing encoding. Route state should support browser Back/Forward and direct deep links.

| Destination | Web route | Phone destination | Scope |
|---|---|---|---|
| Current/recent chat | `#/chat` | Chat tab | Last opened chat, otherwise new chat |
| New chat | `#/chat/new` | New action from chat history | Choose Project or Standard before first send |
| Conversation | `#/chat/{conversationId}` | Chat | One project or standard transcript |
| Thread inspector | `#/chat/{conversationId}/thread/{threadId}` | Thread sheet/child | Must verify thread belongs to conversation |
| Thread panel | `#/chat/{conversationId}/threads` | Chat header panel or Needs you filter | Scoped to parent chat; not a global thread route |
| Needs you inbox | `#/inbox` (proposed) | Needs you tab | Approvals and waiting-device items across chats |
| Devices | `#/devices`, `#/devices/{deviceId}` | Devices tab/detail | Presence, folders, tool permissions |
| Models | `#/models` | More > Models | Provider/key-grouped registry, stats, proposals, rules, import |
| Usage and limits | `#/usage`, optional `#/usage?conversation={id}` | More > Usage & limits | Account or chat-specific metering |
| Memory | `#/memory` | More > Memory | Shared long-term memories |
| Skills | `#/skills` | More > Skills | Learned procedures |
| Settings | `#/settings` | More > Settings | Session, hub, providers/keys, appearance |
| Sign-in | `#/login` or unauthenticated shell | Sign-in | Exchange hub token for session |

- Existing `#/threads` links/deep links should redirect to `#/inbox` or resolve a thread to its owning chat. They must not reintroduce a permanent Threads tab.
- Existing `#/chat/{id}/threads` remains valid as a panel/full-screen list under that chat.
- Opening a thread from a push/notification resolves its conversation first, then opens the inspector. If lookup fails, show “Thread not found on this hub” plus a safe route back.
- Usage links from a chat preserve conversation scope; navigating back returns to that chat.

### 3.5 Where capabilities live

| Capability | Primary home | Secondary entry |
|---|---|---|
| Threads | Current project chat panel and inspector | Needs you inbox; chat cards; notifications |
| Approvals | Needs you inbox; explicit approval sheet | Chat banner; thread inspector; notifications |
| Files | Thread inspector > Files | Final thread result; chat card link when outputs exist |
| Models | Models destination, grouped by provider/key | Role-filtered model picker in composer |
| Devices | Devices destination and per-device detail | Thread header/provenance; device picker when starting work |
| Usage/cost | Usage & limits | Chat menu/chat-scoped usage; provider/model details |
| Memory | Memory destination | Settings link; no need to expose memory contents in transcript by default |
| Skills | Skills destination | Read-only “matched skill” detail in thread activity if used |
| Settings | Settings destination | contextual links from empty/error states |

## 4. Shared interaction and state contracts

### 4.1 Status vocabulary

- `running`: thread is executing on its named device or the hub. Show a visible text label and non-color icon.
- `waiting_device`: target device is offline or unavailable. Show device name, last-seen time when known, and checkpoint status. Do not conflate with a pending approval.
- Checkpoint: a descriptive substate while waiting; keep checkpoint summary visible when handoff occurs.
- `done`: completed; display final summary and available files.
- `failed`: stopped by error; show error and any partial output/files.
- `stopped`: owner stopped it; say so plainly and retain partial output where available.
- Approval pending is an overlay/badge on the thread, not a replacement for the thread's own lifecycle status.
- Never communicate state with color alone. Use a short label and icon; color is supplemental.

### 4.2 Loading, errors, live and offline

- Loading skeletons match the final content hierarchy. Do not replace a full screen with a spinner if cached content is available.
- Inline errors show the hub's useful error text and a retry action. Preserve typed form text and selected state on failures.
- On connection loss, show a persistent “Offline · showing last synced copy” status. Stored chat, thread and management data remain readable where cached. Disable server mutations, approvals and sends; keep their draft text where safe.
- Reconnect by fetching stored conversation events after the last stored sequence, then resubscribing to the event stream. Deduplicate by persisted sequence. Stream deltas are ephemeral and must not be presented as durable history until the stored event replaces them.
- Show last refreshed/fetched time for provider snapshots and cached management data. Stale and estimated labels remain visible in offline mode.
- On reconnect, merge server state and report conflicts rather than silently claiming queued writes completed. No offline send queue is specified until its merge policy is decided.
- Live changes should update components in place to preserve focus, scroll and drafts. When the owner is away from the bottom, don't yank scroll; show a “New activity” button.

### 4.3 Approval safety contract

- Approval sheet order: “Run this tool?”; tool and device; owning thread/chat; human-readable reason (most prominent); exact input/command in selectable monospace block; waiting/timeout consequence; actions.
- Actions: Approve, Deny, “Change it” (deny then open step-in with a draft), “Stop thread”. Do not rewrite arguments when the owner chooses Change.
- On Manual, Approve may be the primary action. When a risk rule triggers, Approve and Deny have equal visual weight. Never use a bright primary style to nudge approval of a risky call.
- A request answered on another client becomes a settled message; do not leave active buttons. A thread ending invalidates its pending approval.
- Tool input can contain sensitive content. Do not put it in the global inbox preview, notifications, telemetry, or screen-lock display by default.

### 4.4 Permissions contract

- Display three named levels with explanatory sentences:
  - Manual: “Every call waits for you.”
  - Auto: “Runs unless a risk rule asks.” Add tool-specific limits when relevant.
  - Bypass: “Every call runs, including deletes and commands outside the workspace.”
- For agent tools, disclose whether Auto confines the agent to the thread workspace; for unconfined tools say so next to the selector.
- Bypass confirmation must state device and tool, consequences, and a destructive action label. Require typed tool-name confirmation for shells and agent chat tools.
- Offline and device-token sessions cannot change permissions. A failed permission save restores the last confirmed value.

## 5. Screen specifications

### 5.1 Chat — project mode

**Purpose**

- Owner's conversation with the hub orchestrator. Orchestrator can delegate one or more threads to devices/hub; owner can follow and steer those threads without losing the parent conversation.

**Content hierarchy**

1. Header: chat title; “Project” mode marker; model label (pinned model or role default); chat actions; “Threads” panel toggle with active/needs-you count.
2. Attention strips: global/unread approval for this chat, then thread aggregate state (e.g. “2 running · MacBook waiting”). Keep banners compact and non-blocking.
3. Transcript, chronological: user and orchestrator messages; grouped thread cards at the event point; approval and budget notices; errors.
4. Composer, anchored at bottom: multiline prompt, role-appropriate model picker, send action. Project mode is fixed after first message.
5. Optional thread panel and inspector share the remaining width; do not make the transcript unusably narrow.

**Desktop layout**

- Default: navigation sidebar + transcript/composer. Thread panel opens from the header or shortcut; selected thread opens an inspector to its right. At widths below roughly 1100 px, choose one supporting panel at a time or overlay it; retain transcript width of at least 520 px when possible.
- Thread panel is a compact list (about 300 px); inspector is 380–440 px. The panel can collapse while inspector remains open, but then provide a “Back to threads” control.
- Keep composer fixed to transcript column, not spanning the inspector. Keep an unobtrusive count of running/waiting threads in the header.

**Phone layout**

- Single-column transcript. Thread cards use full-width rows with title, status, device and one preview line; tap opens thread sheet.
- Header “Threads” button opens a chat-scoped bottom/side sheet with counts and rows. The main bottom bar remains Chat / Needs you / Devices / More.
- The composer stays visible above the keyboard. It grows to four lines then scrolls internally. Do not autoselect/focus it on chat navigation.
- Opening a thread pushes a full-height screen; Back returns to the same chat and scroll position.

**States and interactions**

- Empty new chat: explain Project delegates to devices/hub; mode chooser before first send; composer ready.
- Loading existing chat: cached messages first if available, then skeleton tail; load failure is inline and retryable.
- Streaming: assistant draft updates smoothly; caret is decorative; “Replying” announced sparingly to assistive tech. Send remains available if hub semantics permit a new wake-up; don't invent a cancel control.
- Offline: transcript readable from cache; composer and model mutation disabled; typed draft preserved.
- New message while scrolled up: floating “New messages/activity” action jumps to latest on request.
- Approval: compact banner says what tool/device needs review and opens the exact approval sheet.
- Thread card opens inspector; card menus may expose rating only after completion. No shell input/output in the card.
- Chat menu provides chat usage, thread panel, and new chat. It is not the only route to threads.

### 5.2 Chat — standard mode

**Purpose**

- Single model conversation for a quick or low-cost turn, with shared memory recall, no orchestrator delegation and no threads.

**Content hierarchy and layout**

- Same transcript, header and composer geometry as project mode to avoid a second app mental model.
- Header says Standard and shows current/pinned model; no Threads button/count, no delegation strip, no thread cards.
- Model picker filters for `standard` role first and marks free/local/paid clearly. “Default” means standard-role default, not a hidden model.
- Desktop uses the same sidebar and transcript width. Phone uses same chat history drawer and composer behavior.

**States and interactions**

- Empty: say “One model answers; this chat cannot start threads. Memory is shared.”
- Loading/streaming/offline/errors follow project chat behavior.
- Switching mode after first message is unavailable; New chat offers mode selection.
- Chat usage shortcut opens Usage filtered to this conversation.

### 5.3 Threads panel (scoped to the open project chat)

**Purpose**

- Show delegated work belonging to the current chat and provide a quick route to its inspector. This replaces a bottom Threads tab.

**Content hierarchy**

1. Panel title: “Threads in {chat title}”; close/back button; total and needs-you count.
2. “Needs you”: pending approvals first, followed by `waiting_device`; each item includes thread title and reason/status, device and age.
3. “Running”: newest activity first; each row includes title, time elapsed (live), model with a model-family colour accent, device, and a short live preview/tool name. Finished rows show total duration instead of a live timer. The colour is a supplement: the model name is always shown as text.
4. “Finished”: recent done/failed/stopped entries, summary preview; collapsed or limited to recent 20 with “Show all”.
5. Empty state explains threads appear when the orchestrator delegates work; Standard mode explicitly says no threads.

**Desktop**

- Open as a 300–340 px panel adjacent to transcript. Selecting a row opens inspector. On narrow desktop, show panel as overlay and inspector as second route or sheet.

**Phone**

- Sheet/list opened from chat header; selecting a row pushes the inspector. Include the parent chat title in the navigation bar.

**States and interactions**

- Live reorder only when status category changes; avoid rows jumping during ordinary streaming. Announce new approval and waiting status, not each token.
- Loading skeleton groups; error with refresh; offline list uses cached state and labels its age.
- Row selection preserves parent chat and returns to its prior scroll offset.
- Close returns to chat. Escape closes panel, then inspector, in reverse opening order.

### 5.4 Thread inspector / thread detail

**Purpose**

- Reveal exactly what one delegated task is doing, where it runs, what it touched, and how to steer or stop it.
- **Presentation (owner decision):** the same chat-style transcript as a chat. The brief is the first “user” message, the thread's text is the assistant's, tool calls are compact collapsible rows inline, and owner/orchestrator messages appear as messages. The composer at the bottom messages the thread. The header shows the model the orchestrator chose, fixed (with its family colour); there is no model picker here.

**Content hierarchy**

1. Header: title, status word/icon, device, model, started time, routing reason (e.g. selected device/model rationale) if supplied.
2. Attention context: approval card or waiting-device/checkpoint card. Device `last_seen`; explain hub checkpoint and file handoff state.
3. Activity timeline: thread text; collapsible tool call rows with name/state; expandable exact input/output in monospace; approval resolution and owner messages; usage token summary.
4. Result: final summary and status; thumbs-up/down rating if admin; no summary placeholder for empty output.
5. Files: lazy-loaded list, path, size, source device, download action; empty/error/truncated states.
6. Footer while active: “Message this thread” field + Send; Stop thread action. When complete, step-in/stop unavailable and say to ask in parent chat.
7. Advanced: “Show raw events” disclosure with event count, sequence/time and JSON/text selectable. Not default-open.

**Desktop**

- Right inspector in 380–440 px column. Header and footer remain fixed; activity body independently scrolls. When no parent chat is visible (deep link), render thread detail as main content with “Open chat” link.

**Phone**

- Full-height child screen, not a cramped popover. Fixed header and composer/actions; body scrolls. Back returns to parent and preserves position.

**States and interactions**

- Loading thread and events: skeleton; ensure thread is verified to belong to route's conversation before showing it.
- Streaming: append live deltas to a draft; persisted text replaces it. Preserve scroll position if owner reads earlier events.
- Waiting device: show target offline, last seen, waiting reason, checkpoint summary, copied/skipped files after handoff. Keep checkpoint visible after resume.
- Tool row: collapsed by default; expanded state persists while inspector remains mounted. Inputs are selectable and never exposed in compact chat card.
- Message thread posts to thread and informs orchestrator; preserve draft on failure/409. Stop running thread requires confirmation that unsaved work may be lost; waiting thread can stop directly. Offline disables actions.
- On completion, reveal summary, output files and rating controls; no celebratory blocking animation.
- Not found / wrong parent / offline errors are distinct and offer route back to parent or retry.

### 5.5 Needs you inbox and approvals

**Purpose**

- One reliable place to find work that requires owner action across every conversation, without detaching a thread from its parent.

**Inbox content hierarchy**

1. Pending approvals, newest first, grouped by chat then thread; show tool, device, plain reason and age. Do not show payload/arguments in list.
2. Waiting for device, grouped by parent chat; show device, last seen, checkpoint/no checkpoint and latest update.
3. Optional active count summary; omit routine running threads unless owner switches to “All active”.
4. Each row includes chat title and is a deep link to parent chat + thread inspector/approval.

**Approval sheet content and actions**

- “Run this tool?” title; tool/device/thread/chat; risk reason in full; formatted exact input; waiting/timeout explanation; Approve, Deny, Change, Stop.
- Manual reason: Approve can be primary. Risk-rule reason: Approve and Deny equal weight.
- Change denies the call and opens thread step-in with a clear prefixed draft. Stop uses thread-stop confirmation semantics.

**Desktop and phone**

- Desktop inbox is a main destination with a dense, keyboard-navigable list and inspector/sheet on selection. Approval details in modal dialog; focus is trapped and restored.
- Phone inbox uses the Needs you tab; approval is a full-screen/bottom sheet with sticky actions and arguments in a scrollable opaque block.

**States and interactions**

- Empty inbox: “Nothing needs you right now”; link to current chat, not a blank dashboard.
- Loading/error/offline: cached rows with last sync; no answer actions offline.
- On answer elsewhere or expiration/thread-end: replace controls with settled status (“Answered on another device” / “Request no longer waiting”).
- Timeout copy reflects actual configured timeout/action. If API doesn't provide it and limit lookup fails, say “Waiting for an answer” rather than invent a deadline.
- Live additions update count and sort; do not steal keyboard focus. Use one announcement per new approval.

### 5.6 Model registry — grouped by provider/API key

**Purpose**

- Manage provider connections and models as a coherent inventory; inspect their roles, costs, free/local properties, learned results and routing configuration.

**Grouping rule**

- One group per provider. A provider is one connection with one key (owner decision): OpenRouter, Anthropic, Radium, a local LM Studio, the scripted test provider. The same model reached through two providers appears under each, as two entries.
- Each group header shows the provider id, kind, key status (“Key set”/“No key”), base URL host (if configured), hosted-via device and its online/offline state, model count, and expand/collapse. Never reveal or fingerprint the key.
- Sort providers by id; models by role, then name. No unstable random order.

**Content hierarchy**

1. Header with search, Add provider, Import model.
2. Optional pending proposals notice and count.
3. Provider groups; connection summary; nested models.
4. Model row/card: display name and provider model ID; roles (orchestrator/standard/thread); free/paid, local/remote; endpoint host/device if relevant; context window; input/output/cache prices per million; use cases.
5. Model detail/edit: roles, prices, context, modalities, use cases; refresh catalog prices where supported; probes and results; delete confirmation.
6. Secondary sections/tabs within Models: Registry, Stats, Proposals, Routing rules. Keep primary model inventory the default.

**Model picker inside chat**

- A covering panel opened from the composer's model button (owner decision): full-height sheet on phone, large overlay panel on desktop (about 640 px wide, most of the viewport tall) with a scrim. Search field focused on open. Header shows role (“Orchestrator model” or “Standard model”) and current pinned/default choice.
- Rows grouped under provider headers, with availability and device-host status. Mark “Free”, “Paid”, “Local”, “Default for this role”, and stale/disabled model accurately.
- Show estimated price per 1M input/output and context for each model in detail/secondary text; do not overload the closed picker.
- Project chats filter for orchestrator role; standard chats filter for standard. Allow compatible models with explicit role warning/confirmation only if supported by registry. Default selection is always present.
- A selected model pinned to a conversation appears in header. If unavailable/offline, keep selection visible and explain; do not silently fall back without a system event/message.

**Desktop and phone**

- Desktop: groups with collapsible provider headers and table/list model rows; detail in inspector or sheet. Preserve search and expanded groups.
- Phone: provider cards with connection state; tap to expand model cards. Model editing/import uses full-screen form sheet; actions remain reachable without horizontal tables.

**States and interactions**

- No providers: explain Add provider; no false empty model error. Provider configured but no models: Import model action.
- Loading per group; an individual provider failure does not erase other groups. Error includes retry.
- Offline: cached inventory with fetched time; writes/import/probes disabled.
- Add/edit provider keeps API key write-only: empty password field means retain existing key; never prefill or echo it. Warn that provider keys sync to devices per current architecture.
- Import model validates provider ID and external model name; show imported prices/context and proposed use cases separately. Owner explicitly accepts proposals.
- Stats distinguish actual finished-thread outcomes from probe results; null values render “No data”, not zero.

### 5.7 Devices

**Purpose**

- Show where work can run, what each device can access, and the owner-controlled permission policy for every tool.

**List content**

- Device name, platform, Online/Offline, last-seen age, number of available tools, scoped folders and folder modes, hosted model endpoints if any.
- Hub is a first-class device; explicitly state hub threads currently have file tools only (until sandbox support exists).
- Sort online first, then owner-configurable pinned/favorite devices, then offline by name. Surface offline devices used by active threads at top with “used by waiting thread” links.

**Detail hierarchy**

1. Device identity, platform, presence, last seen, connection instructions if never connected.
2. Scoped folders and read/read-write mode; clearly say paths remain on device and are represented by names/modes centrally.
3. Tools grouped by capability category and searchable; each has description, availability and Manual/Auto/Bypass choice with policy text.
4. Separate “Unavailable tools” disclosure preserving saved permission row but disabling changes until available.
5. Hosted provider connections and local model availability where configured.

**Desktop and phone**

- Desktop: device cards in a responsive grid, detail as main view with tool rows; tool list uses sticky category/search controls.
- Phone: list rows and single-column details; permissions are full-width controls with explanation directly below; no horizontal segmented-control clipping.

**States and interactions**

- Empty: instruct owner to run `grouper connect` with device token. Loading skeleton, error/retry, stale/offline list as above.
- Presence updates live without rebuilding focused controls. If device goes offline mid-edit, stop new writes and retain last confirmed policy.
- Changing permission updates optimistically but rolls back on error. Bypass always has explicit confirmation and typed tool name for shell/agent chat.
- Device revoke/credential rotation, if exposed in a later UI, must explain that API keys may have been held by the device and require rotation. Don't imply provider credentials are currently isolated per device if they sync account-wide.

### 5.8 Usage & limits — full redesign

**Purpose**

- **Two tracks, kept apart (owner decision):** (1) **API costs** per provider key: Grouper's metered estimate, provider-reported spend/balance, free allowances such as OpenRouter's daily requests; (2) **Subscription usage/quota**: plan windows (e.g. Claude, ChatGPT/Codex) read on devices, shown as used/remaining per window with reset time. The page leads with both, side by side on desktop, stacked on phone.
- Answer at a glance: “What did Grouper spend, where did it go, what is left in provider allowances, and what limit will stop or warn me next?” Clearly distinguish metered Grouper calls, provider-reported billing/balances, and unofficial subscription snapshots.

**Top-level dashboard hierarchy**

1. **Today** (UTC day, shown with local equivalent): estimated Grouper spend; total tokens/calls; budget used/remaining; daily reset countdown. If no daily cap, explicitly say “No daily budget set”.
2. **Provider/key spend**: one card per provider connection/API key. Show provider-reported spend/balance if supported; Grouper-metered estimate; discrepancy/unobserved-external-use note; snapshot age and source. Separate API keys/accounts; never sum unlike units without labeling currency.
3. **Free-tier allowances**: quota/credits per provider/key/window, including OpenRouter daily allowance where available/configured (e.g. 50 requests/day allowance only when the provider/account's actual current policy and snapshot provide it; never hard-code a universal quota). Show used/remaining, resets-at, fetched-at, measured vs estimated/unavailable.
4. **Usage breakdown**: selectable grouping by Provider/key, Model, Chat, Thread, Day; selectable date range; default 30 days. For each row show estimated cost, calls, input/output, cache-read/write tokens; link rows to model/chat/thread where possible.
5. **Limits & budgets**: daily, per-chat, per-thread budgets; running-thread caps global/per-device; warning threshold; approval/question timeouts and timeout action. Show effective value, on/off, current consumption/progress and consequence. Clearly mark all off defaults.
6. **Data quality and refresh**: source legend; last refreshed; stale snapshots; refresh provider data; list providers not measurable by official API and explain Grouper metering scope.

**Definitions and labels**

- “Grouper estimate”: tokens reported for calls made through Grouper multiplied by current registry prices. Show as estimated; prices may change and historical reports may be recalculated only if backend does so (otherwise preserve recorded estimate).
- “Provider reported”: provider's usage/billing endpoint or balance. It may include calls outside Grouper and may not reconcile by key if provider API does not expose that dimension.
- “Subscription allowance”: session-derived, unofficial/fragile snapshot if applicable. Mark “Unofficial snapshot” and its originating device; never present as a guaranteed provider limit.
- “Free” model means registry metadata; not a promise of unlimited capacity. Show actual current allowance only from available snapshot/API.
- OpenRouter example: show “50 requests/day” only as account-specific allowance data if returned/verified. Otherwise label “Allowance unavailable” and link to provider/account settings; never bake 50/day into UI logic.
- Amounts default USD, with explicit provider currency where not USD. A token count is not a dollar amount.

**Desktop layout**

- Top KPI cards (Today, remaining budget, running threads, providers with stale/low balance), then provider/key cards, then breakdown chart/table, then detailed limits.
- Date/group filters stay visible. Accessible data table is primary; optional chart is supplemental and has text/table equivalent. Select provider/key card to filter breakdown.
- Chat-scoped usage route replaces provider overview with chat total, per-model/thread breakdown and a return-to-chat link.

**Phone layout**

- Stacked KPIs, provider/key cards with progress and age, then collapsible breakdown and limit sections. Group/date filters open a sheet.
- No wide table; each row becomes a labeled card. Keep “estimated” attached to every spend figure, not only the page heading.
- Pull-to-refresh or explicit Refresh with in-progress state; no implied background freshness.

**States and interactions**

- No metered calls: explain that Grouper only sees its own routed calls; allow range/filter adjustments.
- No provider snapshots: identify which providers support data, provide manual refresh and show unavailable reasons. Do not fill unknown balances with zero.
- Stale > one hour (or provider-specific threshold): label stale and show timestamp; keep last value rather than blanking it.
- Error by source: preserve working source cards and isolate failures per provider/key. Retry only failed source when possible.
- Offline: show cached totals/snapshots and their age; disable refresh and limit edits.
- Live usage: increment the relevant in-session estimate on persisted usage events; animate numbers minimally and announce only meaningful budget threshold crossings.
- Budget warning at configured fraction: persistent non-modal warning with amount, cap, scope and link to limit. If budget blocks a turn, show exact refusal and limit link.
- Avoid misleading totals that add provider-reported bills to Grouper estimates (they overlap). Put them side by side, with a reconciliation note, not stacked into a grand total.
- Export/download is a future option, not required for first build. If added, export exact selected range and source metadata.

### 5.9 Memory

**Purpose**

- Owner-managed long-term facts shared by project and standard chats; expose exactly what may be recalled and provide a dependable forget path.

**Content hierarchy**

1. Search input and “Add memory” form (content and tags).
2. Recent/filtered memory rows: text, tags, updated time, last shown to a model, source chat when available.
3. Edit and Forget actions. Forget confirmation quotes the memory and says it will no longer be recalled.
4. Empty-state explanation: orchestrator can save relevant memories; owner can add, edit or forget.

**Desktop and phone**

- Desktop narrow readable column, inline edit or dialog; phone one-column cards and full-width edit sheet. Search is persistent at top.

**States and interactions**

- Loading/error/retry; no results vs no memories are distinct. Offline uses cached list/search and disables writes.
- Preserve search and draft on errors. Search debounce must cancel stale requests.
- Show timestamps in local time with UTC detail if needed. Do not expose memory contents in notifications or public app previews.
- Access requires admin token; device-token session sees an explanatory sign-in/access message, not an empty list.

### 5.10 Skills

**Purpose**

- Inspect and curate learned procedures that the orchestrator may offer on similar tasks.

**Content hierarchy**

- Search; list of skill name, description, success count and last-used time; expand to steps, triggers, tools, models and devices; Edit and Forget.
- No “Create skill” control in current contract: orchestrator creates saved skills. State this in empty state.

**Desktop and phone**

- Desktop narrow expandable list; phone cards with detail pushed/sheet. Markdown steps remain readable and code is selectable.

**States and interactions**

- Loading/error/empty/no-search-match; offline cached read-only.
- Editing preserves content and supports trigger list edits; deleting confirms the skill will no longer be offered.
- Show provenance and suitability metadata; don't claim a success count is a guarantee of future outcome.

### 5.11 Settings

**Purpose**

- Manage browser session, hub connection, account-wide provider connections/keys and appearance. Device tool permissions remain in Devices; limits remain in Usage & limits.

**Content hierarchy**

1. This session: device/session identity, admin vs read-only role, expiry/revoke/sign-out.
2. Hub: host, health, hub/protocol version, live-event connection.
3. Provider connections/keys: add/edit, kind, endpoint, via-device, key state, write-only secret. Include clear note about encryption at rest and current sync-to-devices behavior.
4. Appearance: System/Light/Dark; follow OS reduced motion/transparency/contrast.
5. Sessions: active browser sessions and revoke action; session revocation separate from device token revocation.
6. Advanced information: version, diagnostics without secrets; links to Devices, Models and Usage for related controls.

**Desktop and phone**

- Desktop grouped sections in a narrow settings content column; provider edit in modal sheet.
- Phone grouped navigation or scrollable sections with clear section headings; provider edit full-screen form; sticky Save/Cancel when keyboard is present.

**States and interactions**

- Loading skeleton per independent section. One failed section must not blank the rest. Offline shows cached settings and disables writes.
- Key field is always blank; “Stored—leave blank to keep current key.” Never log or echo key; on save clear field.
- Hub health unreachable, API auth expired and insufficient permission are separate. Sign-out confirms, revokes browser session, clears local session cache and returns to sign-in.
- Changing appearance is immediate and reversible. Respect System without forcing contrast or transparency preferences off.

### 5.12 Sign-in

**Purpose**

- Trade a hub-issued token once for a browser session cookie. Token is not retained in local storage, URL, logs, analytics or error strings.

**Content and layout**

- Grouper identity; hub URL/origin context; password-style token input; Show/Hide button; Sign in; concise directions to obtain token; note that admin token can act and device token is read-only.
- Desktop centered compact card. Phone card fits safe area and keyboard; submit remains accessible above keyboard.

**States and interactions**

- Initial focus on token field only when arriving at sign-in intentionally; don't steal focus on auth expiry if that would destroy a user's context without announcement.
- Signing in disables duplicate submit and announces progress. Invalid token focuses/selects field and gives a generic, safe error. Network error offers retry without clearing token. Success clears the input and enters app.
- Existing session detected silently enters app. Expired/revoked session clears app state and shows sign-in reason without exposing credential.
- No token in URL/query parameters. Password manager may fill token field; Show/Hide updates accessible name.

## 6. Visual system (provisional until concept art)

### 6.1 Material rules

- Functional/navigation layer only: desktop sidebar, top bars, transient popovers, modal navigation chrome, phone bottom navigation and floating “new activity” control may use regular Liquid Glass on native Apple platforms.
- Solid content layer: chat transcript, user/assistant messages, thread cards, inspector activity, approval reason/arguments, model/provider groups, cost tables/cards, memory/skill text and settings forms use opaque semantic surfaces. A standard material may separate a grouped section, but text/background contrast remains sufficient without relying on blur.
- No clear glass in Grouper v1: no full-bleed photo/video behind controls. No translucent surface behind approval JSON or budget numbers.
- Web client may approximate regular glass using backdrop blur only in the navigation/control layer. If unsupported, use a solid surface; do not use a transparent low-contrast fallback.
- Sidebar may be more opaque than a compact control because it contains labels and navigation. Composer is primarily a solid text field; its surrounding chrome may be lightly material-backed, but typed text/input surface remains opaque.
- Sheets/dialogs have opaque content body and system/material navigation chrome; scrim is separate.
- **PROVISIONAL:** exact brand mark, accent hue, shadow, surface warmth, glass thickness and decorative illustration await owner concept art.

### 6.2 Colour tokens

Use semantic tokens and verify WCAG 2.2 AA contrast: body text at least 4.5:1; large text at least 3:1; non-text controls/focus boundaries at least 3:1. Status must pair text/icon with color.

| Token | Light provisional | Dark provisional | Use |
|---|---|---|---|
| `canvas` | `#F5F5F2` | `#141516` | Main page background |
| `surface` | `#FFFFFF` | `#1C1D1F` | Transcript/cards/forms |
| `surface-raised` | `#EFEFEB` | `#26272A` | Grouped controls |
| `surface-strong` | `#E6E6E1` | `#2F3034` | Selected rows/dividers where needed |
| `text-primary` | `#1B1C1E` | `#ECECEC` | Primary content |
| `text-secondary` | `#4F535A` | `#A8ABB1` | Secondary labels (retest contrast at final sizes) |
| `accent` | `#0B7372` | `#34B3AE` | Links, selected navigation, send/running |
| `warning` | `#9A4A06` | `#F0A23B` | Waiting/attention; include icon and label |
| `danger` | `#B42318` | `#FF7A6E` | Error/risk label; destructive fill uses contrast-safe darker shade |
| `success` | `#1F7A3D` | `#5CCF86` | Completion; never sole status cue |
| `focus` | `#1A73E8` | `#7CB4FF` | Visible keyboard focus ring |
| `glass-control` | system regular | system regular | Navigation/control surfaces only |

- Status mapping: running = accent; waiting/approval = warning; done = success only as secondary accent; failed = danger; stopped = secondary text plus stop icon.
- **PROVISIONAL:** palette above carries forward the current web client's accessible base; concept art may change hue, not semantic mapping or contrast floor.
- Dark theme follows system by default. Explicit light/dark override is optional user setting.
- Increase Contrast strengthens borders, focus, status fills and text; it must not merely increase saturation.
- Reduced Transparency replaces all glass with opaque matching surfaces. Do not use blur if OS requests reduced transparency.

### 6.3 Typography

- Default system sans: native system font on Apple; system UI stack on web. Avoid branding type for long content.
- Type scale: 12 caption; 13 secondary; 14 control/list metadata; 16 body/default; 18 section heading; 22 screen title; 28 welcome/major metric. **PROVISIONAL** exact scale can adapt to concept art but body must remain readable and support Dynamic Type/zoom.
- Chat prose 16 px/1.6 line height baseline; transcript max width about 760 px on desktop. Body can grow with OS text scaling; no fixed-height message clipping.
- Tool input/output, IDs, model identifiers and token figures use a platform monospace face, 13–14 px minimum, wrap long lines or provide horizontal scroll in a labeled region.
- Numeric usage values use tabular figures; currency always includes symbol/code and estimate qualifier.

### 6.4 Spacing, radii, elevation

- Base spacing unit 4 px/pt. Common tokens: 4, 8, 12, 16, 24, 32, 40.
- **PROVISIONAL** radii: 8 for fields/compact controls; 12 for cards; 18 for panels/sheets; pill for status chips and segmented controls. Use fewer, consistent radii rather than per-component decoration.
- Maintain 16–24 px content gutters, 12–16 px between related items, 24–32 px between sections. Phone gutters at least 16 pt; honor safe areas.
- Elevation indicates overlay/order only: subtle line for cards, restrained shadow for floating panel/sheet. Avoid stacked card-on-card shadow piles.

### 6.5 Iconography and status

- Use one consistent outline icon family: chat, plus, device, model/provider, shield/approval, clock/waiting, running, check, warning, stop, file, settings, usage, memory, skills, search, close/back.
- Icons supplement visible text; use accessible labels for icon-only controls. Avoid provider logos as the only grouping label; show provider name.
- Motion: 120–220 ms for panel transitions, ease-out; no continuous shimmer beyond loading skeleton and no parallax. Streaming caret may blink gently but is hidden under Reduce Motion. Respect `prefers-reduced-motion` and native Reduce Motion by removing nonessential movement and using instant/fade changes.
- Live update animation must not move a focused row unexpectedly. New item insertion may use a short fade; reduce motion removes it.

## 7. Accessibility and input

### Keyboard

- Global web shortcuts: `Ctrl/Cmd+K` opens chat search/command palette; `Esc` closes topmost popover, then inspector, then panel/sheet; `Ctrl/Cmd+Enter` sends only if explicitly documented and does not conflict with IME. Shortcuts are discoverable and never the only way to act.
- Tab order follows visual order: sidebar, main header, transcript controls, composer, panel, inspector in a predictable roving/focus sequence. Provide Skip to main content.
- Chat model picker supports type-to-filter, arrow navigation, Enter selection and Escape return to trigger. Provider group headings are not selectable model rows.
- Thread panel supports arrow navigation between rows and Home/End; inspector retains its own normal tab order.
- Approval modal traps focus, starts at title or safe first action (not Approve for risky prompt), and returns focus to the opener when closed. Escape dismisses only if unanswered; it never approves/denies.
- Destructive actions require confirmation and place initial focus on cancel/safe choice, except typed-confirm dialog where text input receives focus.

### Screen readers and live regions

- Use semantic landmarks: navigation, main, complementary (inspector), named regions for transcript, thread list and activity.
- Every thread card has accessible name containing title, status, device; value/description contains concise latest preview. Approval count is textually available.
- Status has text and icon; color alone never encodes lifecycle or permission.
- Streaming text is not announced per token. Announce sentence boundaries or a bounded interval, then one “Reply finished”/thread status announcement. Allow OS screen reader to browse the accumulated text.
- New approval is announced once with tool/device and “needs review”; do not read tool arguments automatically. Budget threshold announcement is once per scope/period.
- Expanded tool row labels input and output and exposes exact text as selectable content; large JSON can be navigated without forced full announcement.
- Errors use an alert only when action is interrupted; background refresh failures are status messages, not repeated alerts.

### Contrast, touch, zoom, cognition

- Meet WCAG 2.2 AA. Check every status, warning, disabled control, glass-backed navigation label, focus ring and selected state in light/dark/high contrast.
- Minimum touch target 44×44 pt on Apple and 44×44 CSS px target on phone web; larger where practical. Maintain adequate gaps around destructive controls.
- Web reflows at 320 CSS px without two-dimensional page scrolling except labeled code/data regions. Support browser zoom to 200% and OS text scaling without clipped controls.
- Avoid unexplained jargon; tool names may be technical but pair with a human-readable description/reason. “Estimated”, “Unofficial”, “Stale”, “Offline”, and “Waiting for device” use plain text.
- Respect reduced motion, reduced transparency and increased contrast independently. User theme choice must not disable assistive technology display settings.

## 8. Native Apple implementation notes

### Shared product model

- SwiftUI macOS and iOS clients use the same route/state semantics, event reducer and labels as web. UI may adapt to platform conventions but must not invent a second thread relationship or status vocabulary.
- Use the Rust core/client and shared API models where available. Keep secrets in Keychain; never put admin/device tokens in app state that is logged, analytics, URLs, or widgets. Web sign-in uses session cookie; native clients retain their platform-specific credential contract.
- Permission and approval writes require admin credentials. Device token is read-only for UI mutations. Display effective permission per device/tool and its policy sentence.

### macOS

- Use `NavigationSplitView` or equivalent native split navigation: sidebar (Chats/Needs you/management), central transcript, optional thread inspector. Inspector collapses to a sheet at narrow window widths; standard chat hides thread controls.
- Use native toolbar/menu controls and regular Liquid Glass for sidebar/toolbar/navigation layer. Transcript and inspector activity remain opaque content surfaces. Prefer native SwiftUI materials/system colors; do not implement CSS-like blur effects over every card.
- Support multiple windows only if chat/inspector state is independent and event subscriptions are safely shared. Menu-bar node controls (if present) are secondary quick status/approval access, not a replacement for the main app.
- Keyboard: native command equivalents for new chat, search, close inspector, navigation; preserve standard text editing shortcuts. Show shortcut discoverability in menus.

### iPhone

- Use a native `TabView` with Chat, Needs you, Devices, More; no Threads tab. System tab bar can adopt platform material. More routes to Models, Usage & limits, Memory, Skills, Settings.
- Chat uses native navigation stack; chat-scoped thread panel as sheet; thread inspector as full-height pushed/sheet screen with parent title and Back. Approval actions stay visible while arguments scroll.
- Use native safe-area inset for composer; adjust for keyboard and dictation. Never place send/approval action under keyboard or home indicator.
- Dynamic Type: allow chat bubbles, thread rows and approval reasons to expand vertically. No fixed-height status cards. Use SF Symbols or system-equivalent iconography with text labels.
- Notifications/Live Activities, if implemented, show minimal title/status/device and never tool arguments or sensitive thread text unless the owner explicitly opts in. Approval notification deep link opens the parent chat/thread approval; background action must obey current auth and server result.

### Material and accessibility fallback

- Let system components adopt Liquid Glass automatically. Use `.glassEffect` sparingly for custom navigation/control surfaces only, not transcript or data content.
- Use regular glass for text-bearing navigation, not clear; clear variant is reserved for future full-bleed media, which is not part of this spec.
- Respect Reduce Transparency, Increase Contrast, Reduce Motion, VoiceOver, Switch Control, keyboard navigation on Mac and Dynamic Type on iOS. When glass is reduced/unsupported, opaque system background plus border replaces it.
- Test actual light/dark and accessibility appearance on supported OS versions; don't assume a fixed sampled glass color.

## 9. Open questions for the owner

1. What concept art/brand direction should replace the provisional warm-neutral/teal visual tokens, and should Grouper use a mascot/illustration at all?
2. Should the phone primary tabs be Chat / Needs you / Devices / More as specified, or should Devices move under More to give Chat more prominence? *(Being brainstormed: `docs/gui-nav-ideas.md`.)*
3. ~~Labels for multiple keys per provider~~ *Answered: one key per provider; a provider is a connection.*
4. Should a user be able to create multiple project chats and standard chats freely, and should chats be renameable/archivable/searchable beyond current behavior?
5. *Partly answered: two tracks, API costs per key and subscription usage/quota.* Which usage sources should be enabled in the first UI release: Grouper metering only, official provider billing/balance APIs, and/or device-local subscription snapshots? Which unofficial probes are acceptable to display?
6. What budget values and warning threshold should be the initial suggested setup? All current hub limits are off by default; UI must not imply a cap is active.
7. Should offline chat sends remain disabled, queue for later, or be answered locally and reconciled? Until chosen, preserve a draft and never pretend it was sent.
8. Should usage dashboard show UTC day only or offer local-day as an alternate view? Backend limit semantics are UTC; display must avoid a mismatch.
9. Should owner be able to customize notification content/privacy for thread completion and approvals on each Apple device?
10. Should provider API credentials continue syncing to all connected devices, or should future UI support per-device credential scopes? Current docs describe account keys syncing to devices.
