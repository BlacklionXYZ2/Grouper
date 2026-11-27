# Design research profiles (27 November 2026)

Eight reference sites, read in a 1440×900 browser on 27 November 2026. Every token below was read
from the live page's computed styles or `:root` custom properties (not guessed). Screenshots are in this
folder. They are research material only: borrow structure and principles, never pixels or brand assets.

Coverage limits, stated honestly:

- **Claude:** the app (claude.ai) is behind sign-in; profiled from the sign-in page and claude.com's
  product page. The token set is the shared Claude design system loaded on claude.ai.
- **ChatGPT:** the logged-out chat home (real app shell: sidebar, composer, empty state).
- **Hermes:** the official Hermes Desktop page; it shows no app UI, so this profile is the brand.
- **Orca and Linear:** their home pages embed interactive mock-ups of the real app; the app profiles
  come from those.
- The design-research plugin failed to connect, so this was done by hand in the browser pane.

---

## 1. Claude (claude.ai, claude.com) — `claude-signin.jpg`, `claude-product.jpg`

- **Layout:** calm and centred for entry points (a single sign-in card), editorial on the product
  site: generous whitespace, numbered three-column feature rows ("01 / 02 / 03") with hairline
  separators, a thin sub-navigation bar under the main header.
- **Type:** three voices. `anthropic-serif` for big headlines (56px, weight 330, line-height 1.15),
  `anthropic-sans` for UI (body 14–15px, weights 400 and 435/577 — a variable font used at
  in-between weights), `anthropic-sans-display` for section titles (22px/580). Tokens:
  `--font-size-body .875rem`, `--font-size-footnote .8125rem`, `--font-size-caption .75rem`.
- **Colour:** warm greys, not neutral: `--gray-20 #f9f9f7`, `--gray-50 #f0efec`, `--gray-100 #e1e0d9`,
  `--gray-500 #6d6b67`, `--gray-800 #20201f`, `--gray-900 #0b0b0b`. One brand accent, clay
  `#d97757` (`--clay-emphasized #ba5b3c`). Text is black at 90% and 60% alpha. A full semantic ramp
  exists (red, orange, yellow, green, aqua, blue, violet, magenta) but appears only where it means
  something.
- **Shape:** `--radius .5rem`; controls 8px, cards 10px, larger panels 16–24px. Borders are
  `1px rgba(0,0,0,.08)` hairlines. Control height token `--h-control 2rem`; spacing tokens
  `--gap-xs .5rem … --gap-lg 1.75rem`, `--pad-sm .5rem`, `--pad-md .75rem`.
- **Motion:** `--ease-out cubic-bezier(.165,.84,.44,1)`, `--ease-snap cubic-bezier(.32,.72,0,1)`,
  `--dur-fast 60ms`.
- **Signatures:** the serif "voice" headline beside a plain sans UI; warm paper neutrals; a single
  clay accent and the asterisk mark; a near-black primary button.
- **Borrow:** the idea of a *voice* typeface reserved for the product speaking (headlines, empty
  states); a token naming scheme with control heights and gaps; very fast (60ms) feedback
  transitions; hairline borders at 8% alpha.
- **Avoid:** the warm beige paper palette (Grouper is navy and cool); copying the serif look.

## 2. ChatGPT (chatgpt.com) — `chatgpt-home.jpg`

- **Layout:** the canonical chat shell. A 260px left sidebar (new chat, sections, settings, a sign-up
  card pinned at the bottom), a centred 768px reading column, and one dominant element on an empty
  screen: the composer with a single question above it ("Where should we begin?").
- **Type:** the system font only (`-apple-system`, Segoe UI on Windows). Body 14px/400, secondary
  12px, headline 24px/400 with +0.07px tracking. Weights 400/500/600, nothing heavier except bold
  section headings.
- **Colour:** nearly no colour at all: page `#fcfcfc`, text `#0d0d0d`, secondary `#5d5d5d`, fills
  `#f3f3f3`/`#e8e8e8`. The only colour on the empty screen is the send button's tint.
- **Shape:** pills everywhere (`999px` on 11 elements), 10px for list rows, composer radius 28px.
  Elevation is soft: `0 0 0 1px rgba(0,0,0,.04), 0 2px 8px rgba(0,0,0,.04)` and `0 4px 16px
  rgba(0,0,0,.05)`. Minimum touch target `max(44px, …)` in the tokens.
- **Signatures:** the floating pill composer with `+`, mic and a round send button; suggestion chips
  under it; the left rail of plain icon + label rows.
- **Borrow:** composer anatomy (attach left, model/voice right, round send), the 768px column, the
  restraint of an empty state with one question, soft 1px-ring-plus-blur elevation.
- **Avoid:** colourlessness — Grouper's owner explicitly wants coloured accents.

## 3. Hermes (hermes-agent.nousresearch.com/desktop) — `hermes-hero.jpg`, `hermes-downloads.jpg`

- **Layout:** poster-like. Full-bleed electric blue, a big condensed headline left, a classical
  engraving (rendered in the same blue) right, a desktop "terminal" window floating over a Monet
  painting, then three equal platform tiles.
- **Type:** `Oswald` condensed at weight **300** for display (80px/88px, −1.6px tracking);
  `Mona Sans Condensed` for body; `Aeonik Fono` (a mono-flavoured grotesk) for **uppercase 12–15px
  labels** — the dominant text style on the page (45 uses).
- **Colour:** one colour, total commitment: background `rgb(0,0,242)`, text `#f2f2f2`, darker blues
  `rgb(0,0,194)` and `rgb(0,0,145)` for depth, a light grey section to end the page.
- **Shape:** almost square: radii 1.8–3px.
- **Signatures:** monochrome blue; condensed light display type; uppercase mono labels; fine-art
  imagery as brand.
- **Borrow:** uppercase small-caps-style labels in a mono/tabular face for metadata and section
  eyebrows (it reads as "instrument", fits a tool for agents); committing to one brand colour on
  brand surfaces (sign-in, onboarding) rather than everywhere.
- **Avoid:** the full-bleed single-hue UI for the app itself (unreadable for long sessions); square
  corners clash with Grouper's rounded glass chrome.

## 4. Orca (orca.dev) — `orca-hero.jpg`, `orca-app.jpg`

The closest product to Grouper: a desktop app that runs Claude Code, Codex, Grok and others in
parallel worktrees, with a coordinator chat and worker panes, plus a phone companion.

- **Layout (app mock):** left sidebar of projects → worktrees → running agents (each with a status
  icon and model chip, e.g. "gpt-6-astra 2m"); the main area is the **coordinator chat** with a
  split of **worker panes** to the right, each a mini chat with its own composer and model picker
  ("Opus 5.5", "Grok 4.7"). Tabs across the top ("Orchestrate agents · Chat or terminal · Review AI
  output · Git integration"). A **status bar** at the bottom shows subscription meters inline:
  "42% 2h 48m", "59% 4d 6h", "46% 2h 12m", "24% 3d 4h".
- **Type:** a system-ish app font (`orcaAppFont`) at 10–14px (dense: 13px body, 11–12px meta),
  `SF Mono` for paths and code, `Inter` 72px/500 with −2.16px tracking for marketing headlines.
- **Colour:** near-black stack: `--background #08090a`, `--surface #0e0f11`, `--surface-2 #141517`,
  `--secondary #1a1b1e`, text `#f7f8f8`, muted `#8a8f98`, borders `#ffffff14` (8% white),
  `--input #ffffff1a`, `--ring #ffffff59`. Colour comes only from agent/provider icons and status.
- **Shape:** `--radius 12px`; 8px controls, 4px chips, 16px panels.
- **Signatures:** coordinator + worker panes side by side; "Worked for 4s", "Started worker
  tax-rules ↗" activity lines in the transcript; per-pane composers with "Full access" permission
  chips; usage meters as tiny text in the status bar.
- **Borrow:** worker activity lines inside the chat (Grouper's thread cards), a per-thread mini
  transcript in the inspector, permission chips on the composer, meters in a status line on desktop,
  the 8%-white hairline border on dark.
- **Avoid:** IDE density on phone; tabs-as-modes (Grouper keeps one conversation).

## 5. Apple (apple.com/macos) — `apple-macos.jpg`

- **Layout:** a 44px global nav, then a **pill segmented control** of OS names (selected = white
  pill on black), centred product headline, device renders. Sections alternate black and
  `#f5f5f7`.
- **Type:** `SF Pro Text` ≤ 19px, `SF Pro Display` above. Every size has its own tracking: 12px/
  −0.12px, 14px/−0.224px, 17px/−0.374px, 21px/+0.231px, 24px/+0.216px, 48px/−0.144px. Weights 400 and
  600 only.
- **Colour:** ink `#1d1d1f`, secondary `#6e6e73` and `#86868b`, light surface `#f5f5f7`, dark
  surfaces `#161617`/`#1d1d1f`, on-dark text `#e8e8ed`; translucent nav `rgba(245,245,247,.8)` and
  fills `rgba(210,210,215,.64)`.
- **Shape:** 28px for content cards (33 uses), 36px for big tiles, 18px for pills, 10–11px for small
  controls.
- **Signatures:** optical tracking per size; only two weights; huge-radius cards; segmented pills for
  switching scope; translucent chrome over content.
- **Borrow:** per-size tracking values, two-weight discipline, the segmented pill for Day / Week /
  Month and scope switches, Liquid Glass only on chrome (the owner's reference).
- **Avoid:** product-render marketing layouts inside an app.

## 6. Linear (linear.app) — `linear-app.jpg`

- **Layout (app mock):** a slim sidebar (Inbox, My issues, Reviews, then workspace and favourites),
  an issue page with an **activity timeline** where agent actions are plain past-tense lines
  ("Linear created the issue via Slack on behalf of Karri · 2 min ago"), and an **agent chat panel
  floating over the page** (model chip "Opus 5.5", "Thinking…", a context chip "DRV-384 added to
  context", reply composer with Skills).
- **Type:** `Inter Variable` at weight **510** (a between-weight), headings 64px/−1.408px and
  48px/−1.056px; UI 12–15px with −0.13 to −0.165px tracking; `Berkeley Mono` for code and IDs.
- **Colour:** background `#08090a`, panels `#0f1011`, text tiers `#f7f8f8` → `#d0d6e0` → `#8a8f98`
  → `#62666d`; borders `rgba(255,255,255,.08)` at **0.667px** (hairline on 1.5× screens). Accent set
  as named tokens: blue `#4ea7fc`, red `#eb5757`, green `#27a644`, orange `#fc7840`, yellow
  `#f0bf00`, indigo `#5e6ad2`, teal `#00b8cc`, plus product colours (plan `#68cc58`, build
  `#d4b144`).
- **Shape:** pills (`9999px`) for buttons and filters, 8–12px for panels, 4px for chips.
- **Motion:** a complete named easing set (`--ease-out-quart cubic-bezier(.165,.84,.44,1)` — the
  same curve as Claude's `--ease-out` — `--ease-out-expo cubic-bezier(.19,1,.22,1)`, in-out
  variants), toolbar fades at 40ms.
- **Signatures:** four text tiers on near-black, sub-pixel hairlines, named semantic accents used as
  small dots and icons, agent work recorded in the timeline.
- **Borrow:** the four-tier text ramp, 0.5–0.667px hairlines on high-DPI, named accent tokens with
  one job each, agent events as timeline lines, the easing set.
- **Avoid:** nothing major; Linear is the density reference for desktop.

## 7. Raycast (raycast.com) — `raycast-hero.jpg`, `raycast-hero-art.jpg`

- **Layout:** a floating rounded nav bar (a pill-shaped container, not a full-width header), a
  centred hero over a dramatic diagonal light-streak artwork, then the launcher window itself.
- **Type:** `Inter` 64px/600 headline; UI 13–16px with slight positive tracking (14px/+0.2px,
  16px/+0.3px); the platform font for app chrome (SF Pro, Segoe UI Variable).
- **Colour:** `#07080a` base, white text with alpha tiers (100/60/40%), greys `#9c9c9d`, `#6a6b6c`,
  `#434345`; surfaces are **white at 2–10% alpha**; one brand red (`#ff6363` borders).
- **Shape:** 11px for buttons (157 uses), 6px for keys and small items, 20px for panels.
- **Depth:** the signature **bevelled button**: `0 1.5px .5px 2.5px rgba(0,0,0,.4), 0 0 .5px 1px
  #000, …` plus inset `0 0 0 1px rgba(255,255,255,.2)` — light catches the top edge like a physical
  key.
- **Borrow:** keyboard-key styling for shortcuts in the command palette, translucent-white layered
  surfaces on dark, alpha-tier text, the floating nav container.
- **Avoid:** the streak artwork; red as a brand colour (Grouper's red means danger).

## 8. Vercel (vercel.com) — `vercel-hero.jpg`

- **Layout:** black canvas, a thin grid of hairlines framing the hero, left-aligned headline, two
  pill buttons (white primary, dark secondary), a logo row.
- **Type:** `Geist Sans` with very tight display tracking (64px/−3.84px, 56px/−3.36px, 24px/−0.96px)
  at weights 400–450; body 14–16px/400; `Geist Mono` for code.
- **Colour:** `#000`/`#0a0a0a`, text `#ededed`, secondary `#a1a1a1`. A complete 10-step scale per hue
  in tokens (`--ds-blue-100 … -1000`, red, amber, green: e.g. `--ds-blue-700 #0070f7`,
  `--ds-red-700 #fc0035`, `--ds-amber-700 #ffb200`, `--ds-green-700 #28a948`).
- **Shape:** 6px default radius (55 uses), pills for primary actions, 1px borders at 14% white.
- **Signatures:** extreme negative display tracking; hairline grid; monochrome with semantic colour
  only in status; 10-step scales.
- **Borrow:** the 100–1000 scale structure for each accent hue (backgrounds, borders, solid, text
  from one ramp), the discipline of colour-only-for-meaning in data views.
- **Avoid:** pure black (Grouper is navy), extreme tracking on body-size text.

---

## Comparison

| | Density | Colour stance | Type personality | Layout model | Radius |
|---|---|---|---|---|---|
| Claude | Airy | Warm neutrals + one clay accent | Serif voice + humanist sans | Centred column, editorial | 8–10 / 16–24 |
| ChatGPT | Airy | Near-monochrome | System font, plain | Sidebar + 768px column | Pills, 28 composer |
| Hermes | Poster | One saturated blue, total | Condensed light display + mono caps | Full-bleed poster | 2–3 |
| Orca | Dense | Near-black, colour from agents | System UI + mono | Sidebar + coordinator + worker panes + status bar | 4 / 8 / 12 / 16 |
| Apple | Airy | Neutral, translucent chrome | SF, per-size tracking, 2 weights | Segmented scopes, big cards | 28 / 36 |
| Linear | Dense | Near-black + named accents | Inter 510, tight | Sidebar + timeline + floating agent panel | Pills / 8–12 |
| Raycast | Medium | Dark + translucent white, red brand | Inter, slight + tracking | Floating nav, launcher | 6 / 11 / 20 |
| Vercel | Medium | Black + 10-step semantic scales | Geist, extreme − tracking | Hairline grid | 6, pills |

Shared ground across the strongest products: one dominant element per screen; four text tiers;
hairline borders at 6–14% alpha; colour reserved for meaning (status, identity), not decoration; a
named easing set around `cubic-bezier(.165,.84,.44,1)`; a mono face for IDs, paths and numbers.
