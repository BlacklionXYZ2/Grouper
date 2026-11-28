# Grouper GUI concept plan

Status: plan, 26 November 2026. Source: `docs/concept-art/2026-11-26-overview.png` (1536×1024, dark only).
Where this plan and `docs/gui-spec.md` disagree on look, this plan wins; the spec's safety, accessibility
and routing contracts (sections 4, 5.13, 5.14, 7) still stand. No code was changed to write this.

Hex values below were sampled from the art's pixels (flat fills are exact to ±2; text and thin-line colours
are estimates because of anti-aliasing). Contrast ratios quoted were computed with the WCAG 2 formula.

## 1. Vision

The art shows a calm, dense, dark-navy workspace where one conversation is the centre and delegated work
is visible at a glance as coloured, named, per-model rows. Checkable principles:

- **V1. Navy, not grey.** Every dark surface has a blue cast (blue channel is 8–14 above red). No neutral
  or warm greys anywhere in dark mode.
- **V2. Colourful accents on a navy base (owner, 26 Nov evening: "there should still be coloured
  accents rather than being a monotonous blend of blues").** Teal stays the primary action colour (New
  chat, send, Approve, selected tab, links) and a screen has at most one filled teal button. But every
  screen carries visible colour beyond blue: the icon's purple (`#8B5CF6` family) is a second brand
  accent for highlights (Needs you, the selected chat's indicator, plan progress, focus of attention);
  model-family colours (teal, purple, orange, pink, amber, green…) are strong and visible on dots, pills,
  marks, avatars and thread rows; brand surfaces (sign-in, empty states, the mark) use the icon's
  teal→blue→purple gradient. Mint green means "good state". The squint test: a dark screenshot shows
  navy plus at least two clearly different accent hues, never a single blue wash. Contrast rules still
  hold.
- **V3. Depth by tone, not shadow.** Three steps: main canvas (darkest), sidebar and panels (one step up),
  cards/bubbles/composer (two steps up), each with a 1px hairline. Shadows only on things that float
  (palette, model picker, toasts).
- **V4. Dense and single-line.** Sidebar rows are one line, icon plus label, about 30px tall on desktop
  (44px touch targets on phone). Nine nav rows plus four chats fit a 580px-tall window without scrolling.
- **V5. Threads are a group, not loose chips.** Delegation appears in the transcript as one card,
  "N threads running", with one row per thread: colour dot, title, model chip, status pill, chevron.
- **V6. Every thread row names its model in text.** Colour is the quick cue, the model chip is the fact.
- **V7. The right panel is a summary.** Status chip, device, model, a short timeline, Tool calls (n),
  Files (n). It reads in five seconds; depth is one click away.
- **V8. The composer is one rounded card.** Model chip top-left, tool icons and a round teal send button
  top-right, placeholder text below. Nothing else sits in it.
- **V9. Approvals are short and plain.** Shield, "Run this tool?", tool · device, thread, one sentence of
  reason, "View exact input", then Deny and Approve side by side at equal size. It sits where the composer
  was (owner decision), never as a floating modal.
- **V10. Phone is the same product, fewer columns.** Same bubbles, same thread row, same composer card;
  a floating rounded tab bar with Chat / Devices / More and a tinted selected tab.
- **V11. Glass is chrome only.** Tab bar, sidebar and composer chrome may be translucent; transcript,
  cards, approval text and code are opaque (already the rule; keep it).
- **V12. The fish are brand, not UI.** The school-of-fish mark appears as the sidebar/header mark, the
  assistant avatar, the app icon and the sign-in screen. No illustration inside working screens.

## 2. Design tokens

Token names are the existing `app.css` custom properties unless marked *new*. Dark values are from the art.
Light values are proposed (the art has none) and keep the same structure: cool blue-grey neutrals instead
of today's warm `#f5f5f2`, same teal fill with dark ink.

### Surfaces and borders

| Token | Dark (art) | Light (proposed) | Used for |
|---|---|---|---|
| `--bg` | `#10141D` | `#F4F6F9` | Main canvas behind the transcript |
| `--bg-tint` | `#13171F` | `#EEF2F6` | Thread panel, inspector |
| `--glass-solid` (sidebar, bars) | `#141821` | `#E9EEF4` | Sidebar, title bar, tab bar |
| `--surface` | `#171C25` | `#FFFFFF` | Cards, user bubble, composer, dock |
| `--surface-2` | `#1D222C` | `#EEF2F6` | Thinking chip, device chip, "Add to context" |
| `--surface-3` | `#20242D` | `#E3E8EF` | Hover, pressed |
| `--surface-select` *new* | `#1F2D41` | `#DCE6F3` | Selected sidebar row, model chips |
| `--border` | `#272D36` | `#DDE3EA` | Card and divider hairline |
| `--border-strong` | `#3D434D` | `#C3CBD6` | Composer, dock, window edge, Deny outline |
| `--border-control` *new* | `#5A6472` | `#8A94A3` | Inputs that need a 3:1 boundary |
| `--backdrop` *new* | `#071323` → `#0A111C` | `#E8EEF5` → `#F4F6F9` | Sign-in page only (gradient) |

The art's control outline (`#3D434D` on `#10141D`) is 1.8:1, below the 3:1 the spec requires for control
boundaries. Keep it for cards; use `--border-control` (3.1:1) or a filled surface for text inputs.

### Text

| Token | Dark | Light | Contrast (dark / light) |
|---|---|---|---|
| `--text` | `#F3F6F9` | `#0F1722` | 17.0 on bg / 16.6 on bg |
| `--text-2` | `#C5CFDC` | `#46505E` | 11.3 on sidebar / 8.2 on surface |
| `--text-3` (timestamps, placeholder, meta) | `#A4ADBB` | `#5E6875` | 7.5 on surface / 4.8 on sidebar |

### Accent and status

| Token | Dark | Light | Notes |
|---|---|---|---|
| `--accent-fill` *new* | `#12C4BF` | `#12C4BF` | Approve, send, primary buttons. Same in both modes |
| `--accent-ink` | `#06262A` | `#06262A` | Text on the fill: 7.3:1. White on it is 2.2:1, never use |
| `--accent` (text, icons, links) | `#22D3CE` | `#0B7372` | 9.9 on bg / 5.2 on bg |
| `--accent-soft` (New chat, selected tab) | `#152A38` | `#DDF3F2` | Accent text on it: 7.9 / 4.9 |
| `--ok` | `#2EE6B0` | `#0B6B4F` | Running chip, Free, On device, done tick, online |
| `--ok-soft` *new* | `#102E27` | `#D9F3E8` | 9.1 / 5.6 with `--ok` text |
| `--warn` / `--warn-soft` | `#FBBF24` / `#3D3016` | `#8A5A00` / `#FBEFD0` | Waiting for device, needs you. Not in art; proposed (7.7 / 5.2) |
| `--danger` / `--danger-soft` | `#F87171` / `#3F1D22` | `#B42318` / `#FBE4E1` | Failed, destructive. Not in art; proposed (5.4 / 5.4) |
| `--focus` | `#7CB4FF` | `#1A73E8` | Unchanged; blue so it never reads as the accent |
| `--user-avatar` *new* | `#5986F4` | `#3F6FE0` | Owner's initial disc |

### Thread and model colours

The art uses three: teal `#1DD0CB`, purple `#9759F5`, orange `#F8A360`, each with a tinted pill.

| Name | Dot (dark) | Pill bg / text (dark) | Dot (light) | Pill bg / text (light) |
|---|---|---|---|---|
| teal | `#1DD0CB` | `#133D45` / `#4FE3E6` (7.6) | `#0E9A96` | `#D6F1F2` / `#0B6B73` (5.3) |
| purple | `#9759F5` | `#272341` / `#A78BFA` (5.5) | `#7C4DE8` | `#E9E4FB` / `#5B3FC4` (5.7) |
| orange | `#F8A360` | `#442E20` / `#F9A85A` (6.5) | `#D9772B` | `#FBE9D7` / `#9A4A06` (5.3) |
| indigo (the "Ra" badge) | `#8891F0` | outline badge | `#4D4FC9` | outline badge |

How these map onto the existing thirteen `--fam-*` family colours is owner question Q4. Until answered,
keep colour-by-family and retune the family hues so the common ones land on the art's set.

### Type, radii, sizes

- Font: keep the system stack (the art is a system-style grotesque; no web font, no build step).
- Scale (px / weight): brand wordmark 18/600; view and panel title 15/600; body and bubbles 15/400,
  line-height 1.5; sidebar and list rows 13.5/500; section label ("Chats", "Needs you") 12.5/600 in
  `--text`; chips and pills 12.5/500; timestamps and meta 12/400 in `--text-3`; dock title 17/600;
  dock buttons 15/600. Phone bumps body to 16 to avoid iOS input zoom. Mono unchanged.
- Radii: window and phone sheet 16; cards, bubbles, composer, dock `--radius` 12; buttons, rows, model
  chips, status pills `--radius-sm` 8; badges 6; send button and avatars full circle (32px);
  tab bar container 18 with a 12 selected-tab tile. Status pills are rounded rectangles, not capsules.
- Layout: `--sidebar-w` 240 (today 284; art is about 18% of the window); inspector 384–420 (today
  420–500); bar height 52 (today 60); transcript max width 720; composer and dock align to the transcript.
- Light mode shadows stay as today; dark mode drops card shadows entirely (V3).

`app.css` repeats the dark tokens in two places (`[data-theme="dark"]` and the `prefers-color-scheme`
block) and again for reduced transparency, increased contrast and forced colours. All copies must change
together. Also update `theme-color` in `index.html` and `manifest.webmanifest`.

## 3. Gap list

| Screen | Today | Change |
|---|---|---|
| **Desktop sidebar** (`app.js` `buildNav`, `app.css` shell) | 284px. Head: three-dot placeholder mark, "Grouper", plus icon, "Search ⌘K" button. Then Needs you row with count, then chats as two-line rows (title + "Project · 2 running"), then six nav links, then "Connected · name". | 240px. Head: fish mark + wordmark. Full-width "New chat" button on `--accent-soft`. "Chats" label, one-line rows with a chat icon; running/approval state becomes a dot or count at the row's end, not a second line. Show about 8, then "More chats…" (opens the palette on chats). "Needs you" section header with count and age of the newest item, under the chats. Nav rows as now with filled icons. Footer status row (Q8). Search moves to the chat header as an icon (keeps ⌘K). |
| **Chat header** (`chat.js`) | Title, Threads button with count, menu. | Title with a chevron that opens the chat menu, search icon, ⋯ menu. Threads button stays (decisions-needed #1 is still open) but as an icon with count. |
| **Messages** (`chat.js`) | User bubble right-aligned, assistant plain. No avatars, no names, no times. "Replying" pulse. | User message: initial disc + bubble on `--surface`, time at the row's right. Assistant: fish avatar, "Grouper" label, state chip ("Thinking…" while a reply streams, replacing "Replying"), text on the canvas with no bubble. Timestamps need an event time; check the event schema before promising them. |
| **Threads card** (`chat.js` `.chips`) | A horizontal row of separate chip buttons, one per thread, each with title, status pill, device, family mark, mode and a live line. | One card per delegation burst: header "N threads running" (or "2 running · 1 done"), one row per thread (dot, title, model chip, status pill, chevron → opens the thread), footer "View details" → opens the threads panel. Device and the live line move to the thread panel. Rows keep an accessible name with title, status, device. |
| **Composer** (`chat.js`) | Glass form: textarea on top, then a row with Project/Standard segmented control, model button, send. | One card: model chip (family mark + name + caret, opens the covering picker) top-left; attach, image, code icons and round send top-right; textarea below with "Ask Grouper anything…". The Project/Standard choice is not in the art: on a new chat, show it as two cards in the empty state above the composer; it is fixed after the first message anyway. Attach/image/code depend on Q6. |
| **Thread panel** (`thread.js`, `threads-panel.js`) | Inspector 420–500px showing a full chat-style transcript (owner decision 25 Nov), inline tool rows, waiting card, result, files behind "Show files", raw events, step-in composer and Stop. | Art shows a 384px summary: title, pop-out and close; status chip "Running · 2m"; device and model chips; a three-step timeline; "Tool calls (2)" card with ticks; "Files (1)" card with size and download; "Add to context". Conflicts with the 25 Nov decision; see Q3. Recommended build: panel = art's summary plus the step-in composer and Stop; pop-out = the existing chat-style transcript, full width. |
| **Phone chat** (web ≤760px, `ChatView.swift`) | Web: glass tab bar, bar with title and Threads button, chips row, composer with segmented control. iOS: system `TabView`, `ThreadChip` cards, system colours, `.tint(.teal)`. | Header: mark, "Grouper" (or chat title), plus. Same message rows as desktop. Thread row inline (icon, title, pill, chevron). Composer: single-line card "Message Grouper…" with round send; model chip moves into the chat menu or a row above on focus. Tab bar: floating rounded container, selected tab on `--accent-soft`. iOS keeps the native `TabView` (it is already Liquid Glass) with the brand tint. |
| **Phone Models page** (`model-picker.js`, `models.js`, `ModelPickerSheet.swift`) | Picker: covering sheet, search, "Default model", then role models and "Other models" grouped by provider, two-letter family marks, badges Free / Paid / No price set / Local / Role default. Registry page: provider groups with tables and admin actions. | Art: back arrow, "Models", search icon; "Popular" then "Local" sections; provider logo, name, right-aligned badge "Free" or "On device"; "More models…". This is the picker, not the registry. Adopt the row design (mark, display name, one right-aligned badge) and put provider id and prices on a second line or behind the row. Section names and logos are Q5 and Q7. Registry page only inherits tokens. |
| **Approval** (`approvals.js`, `ApprovalDock.swift`) | Inline dock replacing the composer. Title, tool on device, thread + model, reason, "Review exact input before approving" disclosure, timeout, queue controls, Approve (locked until expanded) / Deny, "Change it", "Stop thread". | Art shows a floating dialog with a close X and Approve already live. Owner decision stands: dock, no X, Approve locked. Take the art's look only: shield icon in teal, 17px title, "Web Search · via N100 Server", "Thread: …" in `--text-3`, hairline, reason at body size, "View exact input" as a full-width row with chevron, Deny (outline) and Approve (teal fill) equal width at 44px. Locked Approve is visibly disabled with the hint under it. For risk-rule requests both buttons stay outline (spec 4.3). Queue, timeout, Change it and Stop stay, as small text controls under the buttons. |
| **Brand** (`icon.svg`, `.brand-mark`, `.boot-mark`, `login.js`, app icons) | Teal rounded square with three white circles. Sign-in: one card with the placeholder mark. No tagline. | Fish-school mark (Q1) in sidebar head, assistant avatar, favicon, PWA and app icons, boot screen. Sign-in becomes two columns on desktop: brand panel (mark, wordmark, tagline, three value lines) on the `--backdrop` gradient, sign-in card on the right; one column on phone. The art's right-hand marketing panel is for the README/landing page only. |
| **Everything else** (devices, usage, memory, skills, settings, inbox, palette) | Not in the art. | Inherit tokens, radii and row density only. No layout work in this round. |
| **Mac app** (`GrouperMacApp.swift`) | Sidebar has Chats, Devices, Models, Settings only; system colours. | Same sidebar order and look as the web. Needs you, Usage, Memory and Skills pages do not exist on Mac yet; adding them is feature work outside this plan (listed in drift). |

## Owner answers (26 November 2026, evening)

These replace the recommendations below where they differ.

- **Logo (Q1):** the owner supplied the app icon (`docs/concept-art/2026-11-26-app-icon.jpg`: a large fish
  surrounded by a school, teal to blue to purple on a navy rounded square). It is being vectorised and
  replaces the placeholder mark everywhere.
- **Threads (Q3):** threads behave like a chat (the side panel and the pop-out both show the chat-style
  transcript; no summary cards). Each thread shows a **live plan checklist** pinned at its top, like
  Claude's task list: the thread breaks its task into steps that tick off as it works. Contract: threads
  have a built-in tool `update_plan` with input `{"items":[{"content", "status":
  "pending"|"in_progress"|"completed", "active_form"?}]}` (always the whole list); the latest call's input
  is the plan; clients don't show the call as a tool row.
- **Popular models (Q5):** not now; a possible feature if Grouper gets a cloud-hosted option.
- **"Add to context" (Q9):** on the roadmap for later.
- **Colour (V2):** coloured accents, not a monotonous blend of blues (V2 above was rewritten).

## Roadmap (owner, 26 November 2026)

- "Add to context": attach a thread's file to the next message to the orchestrator.
- "Popular" models section, if Grouper gets a cloud-hosted option that can see usage across users.
- Attach, image and code buttons in the composer once the hub has an attachment API.

## 4. Owner questions

Each has a recommended answer; packages proceed on the recommendation unless the owner says otherwise.

1. **The fish-school logo as a vector.** The art is a raster; a UI mark needs SVG and must read at 16px.
   *Recommend:* the owner supplies or commissions the vector (two groupers circling, small school between,
   teal→blue and purple gradients) plus a simplified one-fish glyph for 16–24px. Until then W6 ships an
   agent-drawn placeholder SVG behind the same file names, so swapping is a file replace.
2. **"Together stronger." and "A more collaborative AI experience."** *Recommend:* use "Together stronger."
   on sign-in, the PWA description and the README only; never inside the working app. Keep the second line
   for a landing page.
3. **Thread panel: summary (art) or chat-style transcript (25 Nov decision)?** *Recommend:* both. The side
   panel is the art's summary; the pop-out arrow opens the chat-style transcript full width. On phone the
   pushed thread screen is the transcript with the summary as its header.
4. **Thread colours: per thread or per model family?** The art's dots could be either (three threads,
   three models). The 25 Nov decision says by family. *Recommend:* stay by family, retune the family hues
   toward the art's teal/purple/orange/indigo, and tint the Running pill with the family colour as drawn.
   Waiting, failed and stopped pills use fixed status colours with icon and word, so an orange family
   never reads as a warning.
5. **"Popular" and "Local" sections in the model list.** A self-hosted hub has no popularity data, and
   25 Nov says "grouped by provider only". *Recommend:* top section "Suggested" (role default plus the
   last few used), then "Local", then "More models…" expanding the provider groups. Badge "On device"
   becomes "Local" with the host device's name when the model lives on another device.
6. **Attach, image and code buttons.** The hub has no message-attachment API (the catalog only records
   input modalities). *Recommend:* do not ship dead buttons. Build the composer with the slot; add each
   button when its backend exists. If the owner wants them now, that is a hub feature package first.
7. **Provider logos (Google, Anthropic, OpenAI, Qwen) and trademarks.** *Recommend:* keep the two-letter
   family marks (the art's own "Ra" badge is that style) and ship no third-party logos. If logos are
   wanted, the owner confirms each brand's usage terms; Google's "G" is already used for sign-in only.
8. **"All systems online".** *Recommend:* derive it from real state: hub reachable and every registered
   device online → "All systems online"; otherwise "2 of 3 devices online" or "Offline · reconnecting".
   The chevron opens Devices.
9. **"Add to context" on a file.** No such action exists. *Recommend:* leave it out now; the natural
   meaning (attach the thread's file to the next message to the orchestrator) depends on Q6.
10. **Memory and Skills.** They exist on the web; the Mac and iPhone apps lack them. *Recommend:* keep
    them in the sidebar and More list as drawn; Apple pages are separate feature work.
11. **Default appearance.** The art is dark only. *Recommend:* keep System as the default, and use dark
    for all brand imagery and screenshots.
12. **Project/Standard switch** has no place in the art's composer. *Recommend:* empty-state cards on a
    new chat (see gap list); confirm.
13. **Model names in the art** (Gemini 4 Flash, GPT 6.1, Qwen 3.8 8B…) are illustration. *Recommend:*
    show the registry's own display names; no hard-coded lists.

## 5. Work packages

Rule: one owner per file. To let the web packages run in parallel without sharing `app.css`, W1 splits
the stylesheet: it creates empty per-area files, links them in `index.html` after `app.css`, and registers
them in the `ASSETS` list in `crates/hub/src/web.rs` (the CSP's `style-src 'self'` allows this). Later
packages put all their new rules in their own file and never touch `app.css`, `index.html`, `web.rs`,
`lib/dom.js` or `lib/ui.js`. W1 lands first; W2–W6 then run in parallel; W7 starts once W1's token table
is merged. Every package appends its own entry to `Changelogs/YYYY-MM-DD.md` (append-only; the merger keeps
all entries). Nobody runs `cargo fmt`.

Web verification, all web packages: `node --check` on each changed JS file; `node crates/hub/web/tests/sw.test.mjs`;
`cargo build -p hub` (the assets are compiled in, so a missing file fails here); then a local hub with the
scripted model (`README.md`, "Build and run", `examples/scripted-demo.json`) checked in a browser at
1440×900 and 390×844, in light and dark (Settings > Appearance), plus `prefers-contrast: more` and
`prefers-reduced-transparency` emulation once. Attach four screenshots per package to the changelog entry.

### W1. Tokens, shell and shared pieces (frontier: Opus) — lands first

- Owns: `app.css`, `index.html`, `manifest.webmanifest`, `crates/hub/src/web.rs` (ASSETS only), `app.js`,
  `lib/dom.js`, `lib/ui.js`, `lib/family.js`, `views/more.js`; creates stubs `css/chat.css`,
  `css/thread.css`, `css/approval.css`, `css/models.css`, `css/brand.css` and placeholder `logo.svg`.
- Does: section 2 tokens in every theme/contrast block; sidebar, bar, tab bar and base controls (buttons,
  badges, pills, list rows, inputs) per sections 2–3; `statusPill` as a rounded-rect pill that accepts a
  family tint; family hues retuned; new icons (paperclip, image, code, pop-out, thread-tree, bolt,
  filled nav set); search icon in the bar; footer status text (Q8).
- Done: V1–V4 and V10's tab bar hold on every existing view with no JS behaviour change; all text tokens
  ≥4.5:1 in both themes (list the measured pairs in the changelog); no view is broken at 320px.
- After W2–W6 merge: a short sweep deleting rules in `app.css` that the per-area files replaced.

### W2. Chat: messages, threads card, composer (frontier: Gemini Neon)

- Owns: `views/chat.js`, `css/chat.css`.
- Does: message rows with avatars, name and state chip; grouped threads card (V5, V6); composer card (V8)
  without attach/image/code unless Q6 says build; header per gap list; new-chat empty state with the
  Project/Standard cards; phone layout of all of it.
- Done: a scripted project chat that starts three threads shows one card with three rows whose pills
  update in place; focus, scroll position and draft survive live updates as today; composer hides and
  restores correctly when the approval dock mounts.

### W3. Thread panel and threads list (frontier: Opus)

- Owns: `views/thread.js`, `views/threads-panel.js`, `css/thread.css`.
- Does: summary panel per V7 (status chip with live elapsed time, device and model chips, timeline built
  from stored thread events, Tool calls and Files cards, step-in composer and Stop kept); pop-out to the
  existing transcript at full width (Q3); threads list rows restyled to match the threads card rows.
- Done: tool input/output is still reachable and selectable; files download as before; waiting-device,
  failed and stopped states each have a designed summary; deep link `#/chat/<id>/thread/<t>` still works.

### W4. Approval dock restyle (frontier: Gemini Neon, because the safety rules must not regress)

- Owns: `views/approvals.js`, `views/inbox.js`, `css/approval.css`.
- Does: the art's look on the existing dock (V9), nothing else. No logic changes to gating, ordering,
  reconciliation or keyboard handling.
- Done: Approve is disabled until the input is expanded; Enter never approves; no close control; queue
  "1 of N" works; settled states render; risk-rule requests show equal-weight buttons. Verify each by
  hand against a scripted approval and say so in the changelog. Phone: both buttons visible above the
  home indicator without scrolling.

### W5. Model picker and registry rows (routine: Gemini Hydrogen)

- Owns: `views/model-picker.js`, `views/models.js`, `css/models.css`.
- Does: picker rows per the art (mark, name, one right badge, second line for provider and price);
  sections per Q5's recommendation; registry page picks up the row and badge styles only.
- Done: keyboard behaviour of the picker unchanged (type to filter, arrows, Enter, Escape); a model
  reached through two providers still shows twice; phone sheet is full height with a back/close control.

### W6. Brand and sign-in (routine: Grok)

- Owns: `icon.svg`, `logo.svg`, `views/login.js`, `css/brand.css`, `privacy.css`, `privacy.html`.
- Does: placeholder vector mark (Q1) in the files W1 registered; two-column sign-in with brand panel and
  tagline (Q2); boot screen mark; favicon and PWA icon.
- Done: sign-in works by Google and by token exactly as before (run both against a local hub); the mark
  is legible at 16, 32 and 180px; replacing `logo.svg`/`icon.svg` with the owner's files needs no code.

### W7. Apple apps (frontier: Opus) — after W1 merges

- Owns: everything under `apple/GrouperApp/Shared`, `iOS`, `macOS`, `iOSWidgets`, plus a new
  `Shared/Theme.swift` holding every colour, radius and type size from section 2 (light, dark, increased
  contrast), in the pattern `ModelFamilyStyle.swift` already uses.
- Does: replace system colours and `.tint(.teal)` with Theme tokens; threads card, message rows, composer
  card, thread summary and dock restyled to match W2–W4's structure; Mac sidebar order and New chat
  button; iPhone tab bar tint; remove the stale "tabs still being decided" comment in
  `GrouperIOSApp.swift`. Native `TabView`, `NavigationSplitView` and system glass stay.
- Done: the `Apple` workflow (`.github/workflows/apple.yml`, Xcode 27 runner) builds `Grouper-iOS` and
  `Grouper-macOS` with no new warnings and the GrouperAPI tests pass. Nothing can be checked visually
  until the owner runs it on a Mac or phone, so the changelog must list what was not seen.

## 6. Vision checkpoints

Review each package's screenshots beside the art and ask:

1. **Squint test.** At thumbnail size, is the dark screenshot navy with exactly one teal hotspot per
   region (New chat, send or Approve)? Any grey or second filled colour fails V1/V2.
2. **Sidebar count.** At 1440×900, are the wordmark, New chat, at least six chats, the Needs you header,
   six nav rows and the status row all visible without scrolling, each on one line?
3. **Threads card.** Three running threads show as one card, three rows, each with dot, title, model
   chip and pill in that order; the dot and pill share a hue; the model name is readable text.
4. **Composer.** One card; model chip left; send is a 32px teal circle; no segmented control inside it;
   no button that does nothing.
5. **Thread panel.** Readable in five seconds: status and elapsed time, device, model, last three steps,
   tool and file counts. No wall of transcript in the 384px column.
6. **Approval.** Sits in the composer's place, has no X, Approve is visibly locked until "View exact
   input" is opened, Deny and Approve are the same size. If it looks like the art's floating dialog, it
   is wrong.
7. **Phone.** At 390px: floating rounded tab bar with three tabs and a tinted selected tile; composer is
   a single-line card; the thread row fits on one line; nothing sits under the home indicator.
8. **Model rows.** One mark, one name, at most one badge on the right; "Free" and local badges are mint
   on dark mint; no third-party logo unless Q7 was answered yes.
9. **Light mode.** Same hierarchy with cool neutrals: canvas lighter than sidebar, cards white, the same
   teal fill with dark ink. No warm beige left over from the old palette.
10. **Flatness.** No shadow under any card in dark mode; borders are hairlines; only palette, picker
    and toasts float.
11. **Accessibility did not pay for the look.** Status is always word plus icon; focus rings are visible
    on every new control; increased-contrast and reduced-transparency modes still work; text pairs
    measure ≥4.5:1.
12. **Brand restraint.** Fish appear in the mark and avatar only; the tagline appears on sign-in only.
