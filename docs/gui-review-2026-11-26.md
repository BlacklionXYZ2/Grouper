# GUI review: web build against the art and the vision

26 November 2026, `harness` branch. Reviewed: the art, the icon, seven screenshots of the merged web app
(scripted model "fake"), and targeted reads of `crates/hub/web/`. Read-only; nothing run. Not seen, so not
judged: the approval dock, the Apple apps, contrast and transparency modes, several threads on different
families, a full sidebar. No contrast ratio was measured here; quoted ones come from code comments.

## 1. Verdicts

### Principles

| | Verdict | Evidence |
|---|---|---|
| V1 Navy, not grey | holds | Dark tokens in `app.css:74-116` match the plan; no neutral grey in any dark shot. |
| V2 Colourful accents | partly | Sign-in and the thread panel pass. Chat, new chat, phone and light are still mostly navy plus teal (section 2). |
| V3 Depth by tone | holds | Three tonal steps with hairlines; no card shadows in dark (`--shadow: none`). |
| V4 Dense, single-line | holds | Sidebar is 240px, rows one line at about 32px. |
| V5 Threads are a group | holds | One card, "1 thread running", one row per thread, "View details" footer. |
| V6 Row names its model | holds | "fake" chip as text on every row. |
| V7 Right panel (as amended by the owner: chat-style, live plan on top) | holds | Status with elapsed time, device, model, pinned plan "2 of 4 done" with ticks, then transcript, composer, Stop. |
| V8 Composer is one card | holds | Model chip left, round send right, placeholder below, nothing else. |
| V9 Approvals | not seen | No screenshot. |
| V10 Phone | partly | Same bubbles, row, composer, floating tab bar. The header carries seven items and truncates the title to "Show me a pl…"; the art has three. |
| V11 Glass is chrome only | partly | Transcript and cards are opaque. The model picker looks translucent in shot 7 (section 3, D2). |
| V12 Fish are brand | partly | Placement is right (sidebar, avatar, welcome, sign-in). At 28-32px the 13-fish mark is a dark blue smudge, not the art's bright mark. |

### Checkpoints

| | Verdict | Evidence |
|---|---|---|
| 1 Squint | partly | Read under the rewritten V2, not its old "one hotspot, no second colour" wording. See section 2. |
| 2 Sidebar count | holds, untested at load | Everything fits on one line with room to spare, but the shots have one chat, not six. |
| 3 Threads card | holds | Dot, title, model chip, pill in order; dot and pill share the family hue. A plan chip ("1/4") sits after the title, which is a good addition. |
| 4 Composer | holds | No segmented control, no dead buttons. Send is grey when empty; the teal state was not captured. |
| 5 Thread panel | holds (amended) | Five-second read works: status, time, device, model, plan progress. The transcript below is the owner's choice. |
| 6 Approval | not seen | |
| 7 Phone | partly | Tab bar, composer and thread row are right. Header is crowded. Home-indicator clearance can't be judged from a screenshot. |
| 8 Model rows | partly | One mark, one name. The second line carries three items ("scripted", "No price set", "orchestrator default") and no right-hand badge appears. Free and Local badges were not on screen. |
| 9 Light mode | holds | Cool neutrals, canvas lighter than sidebar, white cards, no beige. It is pale, though (section 2). |
| 10 Flatness | holds | No shadows under cards in dark. |
| 11 Accessibility | partly, unverified | Status pills carry word plus icon. The picker's fade and the stacked blue rings (D2, D9) need a look. Modes were not shown. |
| 12 Brand restraint | holds | Tagline on sign-in only; fish only in mark, avatar and welcome. |

## 2. Colour

**Squint test, per screen:**

- Pass: sign-in (teal and purple glows, three coloured icons) and the thread panel (purple plan row and
  bar, mint ticks, teal brief bubble).
- Borderline: chat on desktop and phone. Teal New chat, a blue avatar disc, a 17px purple icon and the
  "fake" family's khaki; the purple is too small to survive a thumbnail. Real families would help, but the
  chrome should not depend on them. Light mode is the same, and pale.
- Fail: new chat. Teal only, plus a blue focus ring round the composer; the welcome mark is small and dark.

**Root cause.** Purple exists only as two private, different sets: `--chat-purple*` in `css/chat.css:8-23`
(`#b79cff` text) and `--purple*` in `css/thread.css:16-47` (`#a78bfa` text). The shell (`app.css`) has no
purple and no brand gradient, so the sidebar, bar, pills and picker cannot use them.

**Step 0 (enables the rest):** in `app.css`, in every theme and contrast block, add `--purple`
(`#a78bfa` dark / `#5b3fc4` light), `--purple-fill` (`#8b5cf6` / `#7c4de8`), `--purple-soft`
(`#251f3f` / `#ece7fc`), taken from `thread.css`, whose comments say they were measured; and
`--brand-gradient-text: linear-gradient(90deg, #58ece2, #7fb0ff, #a78bfa)` for dark (light needs darker
stops, to be measured). `chat.css` and `thread.css` then delete their private sets.

**Places to add or strengthen colour.** None adds a filled teal button; none uses white text on purple
(white on `#8b5cf6` is about 4.2:1, below 4.5).

1. **Selected chat indicator**, `app.css:736` `.side-item[aria-current="page"]`: add
   `box-shadow: inset 3px 0 0 var(--purple-fill)`. V2 names this as purple; today only the icon turns teal.
2. **Needs you count**, `app.css:1575` `.count` inside `.side-needs` (and the phone tab's count): `--purple`
   text on `--purple-soft` with a `--purple-fill` 1px edge. Keep the amber dot on the age for "waiting".
3. **Plan chip in the threads card**, `css/chat.css` `.thread-plan` (about line 405): `--purple` on
   `--purple-soft`, switching to `--ok` on `--ok-soft` at n/n. Today it is grey, while the same number is
   purple in the threads panel (`.tp-plan`, `css/thread.css:677`).
4. **Threads card head**, `css/chat.css:372`: put the tree icon in a 24px `--purple-soft` tile with the
   icon in `--purple`. This is the purple that should survive the squint.
5. **Done means mint**, `.status` rules at `app.css:2152-2200`: give the done state `--ok` on `--ok-soft`
   with its tick. It appears to render as `.status.muted` (grey) in shots 4-7; V2 says mint is "good state".
6. **Welcome empty state**, `css/chat.css:471-500` `.welcome`: mark at 64px over a soft radial glow in the
   icon's teal and purple (the same recipe as `css/brand.css:20-24`, lower alpha), and the "What should we
   work on?" heading filled with `--brand-gradient-text` (`background-clip: text`; solid `--text` fallback
   and in forced colours).
7. **Mode cards**, `css/chat.css:553` `.mode-card-icon`: Project icon `--purple`, Standard icon
   `--fam-gpt`-style blue (`#7fb0ff` dark / `#1f5fcc` light), always, not only when checked. The checked
   border stays teal.
8. **Thread panel head**, `css/thread.css` header block: a 3px top bar in the thread's family colour
   (`var(--fam)`), so the panel visibly belongs to the row that opened it.
9. **Unknown models get a hue**, `lib/family.js`: when no family matches, hash the model id onto four
   existing vivid tokens (`--fam-claude` pink, `--fam-nemotron` green, `--fam-ernie` amber,
   `--fam-radium` indigo) instead of grey `--fam-unknown`. The two-letter mark and the name stay the facts.
   `--fam-scripted` khaki can stay; it is test-only.
10. **Brand mark at small sizes** (see D4): the bright 7-fish glyph in the sidebar, the phone bar and the
    assistant avatar adds teal-to-purple to every screen, as the art does.

## 3. Defects and drift

- **D1. "1 running" strip duplicates the threads card.** `views/chat.js:452-470` `renderStrip` shows the
  strip for running threads. Fix: build the strip only from waiting and offline parts (they need attention
  and may be off-screen); a running-only state hides it. Style what remains with `--warn`.
- **D2. See-through model picker.** At rest the CSS is opaque: `dialog.sheet-dialog` has
  `background: var(--surface)` (`app.css:2588`), and nothing in `css/models.css` or the picker rules
  (`app.css:5301+`) overrides it. The only translucency is the shared `rise` keyframe (`app.css:2610`,
  from `opacity: 0`, 0.22s) on `dialog.sheet-dialog[open]`. A comment at `app.css:3092` already records this
  exact problem for the threads sheet, which was switched to a slide-only animation; the phone picker got
  the same fix (`app.css:5499`), the desktop picker did not. So the likeliest cause is a screenshot taken
  mid-fade, which I could not confirm without running it. It is still a defect: text fails contrast during
  the fade. Fix in `app.css`: make `rise` transform-only (drop `opacity`), which also covers the command
  palette (`app.css:6204`) and plain sheets. If the panel is still see-through at rest after that, look for
  a `.glass` or `opacity` rule reaching the dialog.
- **D3. Broken punctuation in the thread's reason line**: "…so it runs on the hub.; model fake: registry
  default…". `crates/hub/src/threads.rs:135` joins fragments with "; " and one already ends with a full
  stop. Fix there: trim trailing full stops before joining.
- **D4. The mark is unreadable small.** `logo.svg` is the 13-fish `mark.svg`; `docs/brand/glyph.svg` (made
  for 16-24px) is not shipped. Fix: add it as `crates/hub/web/logo-glyph.svg`, register it in `web.rs`
  ASSETS, use it for `.brand-mark` (`app.css:611`) and `.msg-avatar-assistant` (`css/chat.css:218`); keep
  `logo.svg` for sign-in, welcome and boot.
- **D5. Two purples.** See step 0.
- **D6. Inconsistent pills.** Plan count is a grey bordered chip in the card, bare purple text in the
  threads panel. The status pill has a border in the card (`css/chat.css:434`) and none in the panel
  header. The model chip is a bordered tag in the card and a mark-plus-name chip in the panel. Fix: one
  recipe each (plan chip as in colour item 3; status pill without border everywhere; model chip always with
  its family mark), shared from `app.css`.
- **D7. Phone header is crowded.** Menu, mark, title, chevron, threads count, plus, more; the title
  truncates. Fix in the ≤760px block of `css/chat.css` (about line 746): hide the mark in the bar, and
  move New chat into the ⋯ menu or the drawer, leaving menu, title, threads, ⋯.
- **D8. Upper-case section labels.** "FILES" in the thread panel and "SUGGESTED" in the picker; the type
  scale says 12.5/600 sentence case in `--text`, as "Chats" and "Needs you" are. Fix in `css/thread.css`
  and the `.mp-section` rule in `app.css`.
- **D9. Picker is all blue.** The search focus ring and the active row border are both `--focus` blue,
  stacked, on a blue-grey row. Fix: `.mp-option.active` (`css/models.css:28`) keeps `--surface-select` but
  takes a `--border-strong` edge; the selected row's check uses `--accent`. Title is 19px
  (`app.css:5318`); the scale says 15/600.
- **D10. Composer focus ring adds to the blue wash** (shot 2: a full blue outline on page load). Fix in
  `css/chat.css`: on `:focus-within` change the card's border to `--border-control`; keep the blue ring for
  `:focus-visible` on the buttons and for keyboard focus of the textarea only if a visible indicator
  otherwise goes missing. Check this one against spec section 7 before merging.
- **D11. Thread panel meta runs together.** "Started 19:29 (just now) 952 in · 60 out tokens" has no
  separator between two facts, and the routing reason is a long grey sentence above it. Fix in
  `views/thread.js`: separate with " · ", and put the reason behind a "Why here?" disclosure.

## 4. Fix list

Ordered by impact within each agent. A lands first (B and C use its tokens); B to E then run in parallel.
One owner per file.

- **A, shell** (`app.css`, `app.js`, `lib/ui.js`, `lib/family.js`, `crates/hub/src/web.rs`, new
  `logo-glyph.svg`): (1) step 0 tokens; (2) D2 `rise` transform-only; (3) D4 glyph for `.brand-mark`;
  (4) colour 1, 2, 5; (5) D6 shared recipes, D8 `.mp-section`, D9 title size; (6) colour 9.
- **B, chat** (`views/chat.js`, `css/chat.css`): (1) D1 strip; (2) colour 3, 4, drop the pill border,
  delete `--chat-purple*`; (3) colour 6, 7; (4) D7 phone header, D4 avatar glyph, D10 composer focus.
- **C, threads** (`views/thread.js`, `views/threads-panel.js`, `css/thread.css`): (1) delete the private
  `--purple*` tokens; (2) colour 8; (3) D8 "Files" label, D11.
- **D, picker** (`views/model-picker.js`, `css/models.css`): (1) D9 row colours; (2) checkpoint 8: one
  right-aligned badge per row ("orchestrator default" moves there; provider and price stay on line two).
- **E, hub** (`crates/hub/src/threads.rs`): D3.
- **Then, screenshots only:** the approval dock locked and unlocked, three threads on three families, six
  chats in the sidebar, send enabled, the picker one second after opening, increased contrast and reduced
  transparency.
