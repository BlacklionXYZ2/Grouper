# Grouper navigation concepts

Design brainstorm; no concept changes the owner decisions: no Threads tab, threads belong to a project chat, model choices are grouped by provider connection, Usage keeps API costs separate from subscription quota, and the picker covers the screen. Thread detail is chat-style with its chosen model fixed in the header.

Notation: `N` = Needs you, `D` = Devices, `M` = management (Models, Usage & limits, Memory, Skills, Settings), `T` = scoped threads panel, `P` = full-screen model picker. Desktop wireframes are wide layouts; phone sheets become pushed full-screen destinations where appropriate. Counts and status must also have text/icons, never color alone.

## 1. Four-tab task bar (baseline, refined)

Phone
```
┌────────────────────────┐
│ ☰ Chats   Project   T·2│
│ conversation / cards   │
│                        │
│ [message] [Model ▾] ➤  │
├────────────────────────┤
│ Chat   N·2   Devices More│
└────────────────────────┘
```
Desktop
```
┌──────────────┬──────────────────────┬─────────────────┐
│ Grouper  +   │ Chat title       T·2 │ Thread inspector│
│ Chats        │ transcript            │ selected thread │
│ N·2          │ composer [Model ▾]    │ activity/files  │
│ Devices      │                       │                 │
│ Models       │                       │                 │
│ Usage…       │                       │                 │
└──────────────┴──────────────────────┴─────────────────┘
```
One tap: Chat, N inbox, Devices, More; open T from the chat header; select a visible recent chat in the history drawer. Two taps: management destination via More; older/search chat via drawer; thread via T then row. Needs you has persistent badge and sorted approval/waiting inbox. Approval opens an explicit review sheet. Thread row opens the child chat view. Model control opens P over full screen on phone / large overlay on desktop, searchable and provider-grouped. Pros: predictable, highly discoverable, faithful to spec and native tab conventions. Cons: More hides several destinations; four tabs consume phone space; T can be overlooked if header is crowded. Reference: spec's Orca attention-first idea, ChatGPT's dismissible history drawer, Antigravity's adjacent inspector; Liquid Glass only for navigation chrome.

## 2. Floating attention control

Phone
```
┌────────────────────────┐
│ ☰ Chats       Project  │
│ transcript              │
│                    ┌───┐│
│                    │N·2││  floating, safe-area aware
│ composer [Model ▾] ➤   │
├────────────────────────┤
│ Chat       Devices More │
└────────────────────────┘
```
Desktop
```
┌──────────────┬──────────────────────┬─────────────────┐
│ Chats        │ Chat / transcript    │ Thread inspector│
│ Devices      │                      │                 │
│ More…        │                  [N·2]│                 │
└──────────────┴──────────────────────┴─────────────────┘
```
One tap: Chat, Devices, More, floating N; recent chats via drawer. Two taps: older chat through searchable history; management through More; thread via T then row. Needs you floats on every route, carries count and accessible label, and opens inbox (not auto-approve). In compact/keyboard states it docks above the keyboard or becomes a toolbar item. Thread/model behavior: same scoped T and P as concept 1; thread is pushed full-screen on phone. Pros: attention is globally prominent and available while managing or reading a thread; frees a bottom slot. Cons: can obscure content/composer, risks being mistaken for a chat action, requires careful safe-area/keyboard placement and an alternate accessible navigation entry. Reference: Orca's “Needs you” priority and Muse's durable interruption model; Liquid Glass-style floating control is optional, but must preserve contrast.

## 3. Chat home, everything else in a sheet

Phone
```
┌────────────────────────┐
│ Grouper ▾    Chat list +│  top menu opens hub sheet
│ project chat            │
│ transcript              │
│ [message] [Model ▾] ➤  │
└────────────────────────┘
```
Desktop
```
┌──────────────┬──────────────────────┬─────────────────┐
│ Chats        │ Chat / transcript    │ Thread inspector│
│ N·2          │                      │                 │
│ Devices      │                      │                 │
│ Models…      │                      │                 │
└──────────────┴──────────────────────┴─────────────────┘
```
Phone hub sheet
```
┌──── Navigation ────────┐
│ Needs you · 2           │
│ Chats / Search / New    │
│ Devices                 │
│ Models · Usage…         │
│ Memory · Skills · Setup │
└─────────────────────────┘
```
One tap: current chat; sheet's visible N, chat history, Devices, or management entry after opening sheet is one tap from the sheet (two from chat). Two taps: N, Devices or management from chat; older chat via search then row. N is top of the sheet and shows a persistent badge on the menu trigger; approval opens review sheet. Thread via header T; P covers screen; thread detail is a pushed chat-style child. Pros: clean, immersive conversation; easy to implement with a reusable native/web sheet; good for one-owner, chat-first use. Cons: all non-chat tasks take an extra gesture; N is not continuously visible; menu trigger may be ambiguous. Reference: ChatGPT's floating mobile history/sidebar and Claude's conversation-first surface. Use opaque content sheets; glass only for sheet chrome.

## 4. Workspace switcher (Chat / Attention / Control)

Phone
```
┌────────────────────────┐
│ Workspace: Chat ▾       │  switcher opens destinations
│ project chat   T·2      │
│ transcript              │
│ [message] [Model ▾] ➤  │
├────────────────────────┤
│ Chat   N·2   Devices More│
└────────────────────────┘
```
Desktop
```
┌──────────────┬──────────────────────┬─────────────────┐
│ Workspace ▾ │ Current workspace     │ Context panel   │
│ Chats        │ chat / inbox / device│ thread inspector│
│ N·2          │                      │                 │
│ Devices      │                      │                 │
│ More…        │                      │                 │
└──────────────┴──────────────────────┴─────────────────┘
```
One tap: switch workspace to Chat/Needs you/Devices/More; current-chat T from header. Two taps: choose a workspace then its item (for example, Usage); switch to Chat then a chat. N appears as a count in the switcher and as an inbox destination; approval review is explicit. T opens from chat, never a workspace of its own. P overlays current content; thread child keeps parent chat in back stack. Pros: scales if more destinations emerge; communicates distinct modes and carries context. Cons: “workspace” is abstract for a single owner; adds a choice before navigation; risks implying separate data spaces. Reference: Antigravity's separation of conversation and work overview, with Claude's context continuity; do not copy IDE workspace metaphors literally.

## 5. Command palette plus quiet navigation

Phone
```
┌────────────────────────┐
│ ☰        Grouper   ⌕   │  search/command entry
│ chat transcript         │
│                  [N·2]  │  optional persistent badge
│ [message] [Model ▾] ➤  │
├────────────────────────┤
│ Chat       Devices More │
└────────────────────────┘
```
Desktop
```
┌──────────────┬──────────────────────┬─────────────────┐
│ Chats        │ Chat / transcript    │ Inspector       │
│ N·2          │                      │                 │
│ Devices      │                      │                 │
│ More…        │                      │                 │
└──────────────┴──────────────────────┴─────────────────┘
       Ctrl/⌘K → Search chats, Needs you, Devices, Models, Usage…
```
One tap: visible primary navigation; desktop palette shortcut / phone search opens searchable actions and destinations. Two taps: choose result after palette; select T then thread; open N then approval. N remains a visible count/control, not palette-only. Thread navigation is scoped T; P remains a distinct, full covering chooser so command search never replaces provider grouping or selection detail. Thread child is chat-style. Pros: fast for keyboard users and many destinations; search unifies navigation without another tab; plain JS and SwiftUI can implement a searchable overlay. Cons: discoverability and phone keyboard cost; commands cannot be the only route; search results must distinguish actions from destinations and never place risky approval behind an ambiguous command. Reference: ChatGPT history search and Antigravity progressive disclosure; palette is a familiar productivity pattern, not a claimed feature of those apps.

## 6. Paired chat/thread swipe

Phone
```
┌────────────────────────┐       swipe →
│ ← Chat A       Thread 1│  or tap T·2
│ transcript       model │
│                activity│
│                output  │
│ [message] [Model ▾] ➤  │  thread composer, if supported
├────────────────────────┤
│ Chat   N·2   Devices More│
└────────────────────────┘
```
Desktop
```
┌──────────────┬──────────────────────┬─────────────────┐
│ Chats        │ Parent chat          │ Thread child    │
│ N·2          │ transcript / cards   │ chat transcript │
│ Devices      │                      │ fixed model     │
└──────────────┴──────────────────────┴─────────────────┘
```
One tap: swipe or tap T to expose chat's thread panel; visible thread card can open child. Two taps: choose thread from panel then open it; return to another chat through history. N is always in navigation and approvals open the correct child/review sheet. P belongs to parent composer and covers the view; child header explicitly says chosen model and does not offer model change. Pros: makes parent/child relationship spatial and easy to compare; quick return to orchestrator; desktop can show both. Cons: swipe is undiscoverable and conflicts with system gestures; narrow phone cannot truly show both comfortably; thread composer may suggest an unsupported direct conversation. Reference: Claude's beside-conversation artifact, Orca's focus-one-subagent/return-to-parent, Antigravity's side panel. Treat swipe as enhancement only; provide explicit controls and full-screen fallback.

## Comparison (approximate taps from a typical open chat)

A tap counts a deliberate tap/selection, not opening the app, typing search, or system Back. “Answer approval” means open request and choose a decision; safety review is not skipped. Best case assumes visible recent item; search can add a step.

| Concept | Answer approval | Open running thread | Switch chat | Change model | Check spend / quota |
|---|---:|---:|---:|---:|---:|
| 1 Four-tab | 2 (N, decision) | 2 (T, row) | 2 (history, chat) | 2 (picker, model) | 2 (More, Usage) |
| 2 Floating N | 2 (N, decision) | 2 (T, row) | 2 (history, chat) | 2 (picker, model) | 2 (More, Usage) |
| 3 Everything sheet | 3 (menu, N, decision) | 2 (T, row) | 2 (menu/history, chat) | 2 (picker, model) | 3 (menu, Usage, view) |
| 4 Workspace switcher | 3 (switch, N, decision) | 2 (T, row) | 3 (switch, Chat, chat) | 2 (picker, model) | 3 (switch, More, Usage) |
| 5 Command palette | 3 (palette, N, decision) | 2 (T, row) | 2 (history/search, chat) | 2 (picker, model) | 3 (palette, Usage, view) |
| 6 Paired swipe | 2 (N, decision) | 1 swipe / 2 taps (T, row) | 2 (history, chat) | 2 (picker, model) | 2 (More, Usage) |

All concepts can keep N persistent; concepts 3–5 need particular care that it is not hidden behind an extra discovery step. Every approval route opens the exact request with tool, device, owning chat/thread, reason, input, and explicit decision actions. Thread rows in T show title, elapsed time (live/total), model, status and accessible family accent.

## Recommendation: concept 1, with a small concept 5 enhancement

Keep the four-tab phone structure from the spec (Chat / Needs you / Devices / More), desktop persistent sidebar, searchable chat drawer, and chat-scoped T panel. Add a keyboard shortcut/desktop command palette as a secondary accelerator, never as the only route; consider a floating N affordance only on detail screens if testing shows the persistent tab is too easy to miss. This is the best balance of transparent safety access, chat ownership, one-handed reach and predictable cross-platform navigation. It avoids an abstract workspace switcher and avoids hiding urgent work in a general sheet. The current sidebar's global Threads area and phone Threads tab should give way to chat-scoped T plus global Needs you; do not make a global thread catalog the replacement.

On phone, opening a thread pushes a full-screen chat-style child with “Back to [chat]”, fixed selected-model label, activity/transcript and any supported step-in controls; the panel row supplies elapsed time and model. The picker is a full-height searchable panel with provider groups and availability/cost context. Desktop opens the same detail in the inspector beside its owning chat, and the picker in a large overlay. Keep the two Usage tracks adjacent but distinct: API spend per provider connection, and device-read subscription plan windows; never add unlike/overlapping amounts into a single total.

Questions for the owner:
1. Should “More” open a compact destination list or land on a dashboard that separates the API-cost and subscription-quota cards?
2. On phone, should T be a visible header button with count, or a thread card/list affordance that expands into a sheet? (Recommend header button plus inline cards.)
3. Is a command palette valuable on desktop at launch, or should it wait until navigation destinations grow?

## Thread-row model-family colour ideas

Use a narrow accent stripe/dot plus a short model label; never tint the whole row or rely on hue for identity. Always show family/model text (for example, “Claude · Sonnet”), provider connection where relevant, status label and icon. Use semantic tokens with tested light/dark contrast, distinguishable luminance and at least one non-colour cue (family glyph/initial or stripe pattern). Validate with common color-vision simulations and Increase Contrast; avoid red/green as the only distinction and ensure selected/focused states remain obvious.

Initial family palette candidates, subject to testing: Claude/Anthropic — muted violet; GPT/OpenAI — teal; Gemini/Google — blue; Llama/Meta — warm orange; Qwen/Alibaba — gold; DeepSeek — cyan; Mistral — coral; local/other open models — slate. These are suggestions, not provider-brand claims; check adjacent hues and contrast in both themes. The same family color should persist across providers only if the model-family metadata is reliable; otherwise prefer provider-neutral until classified. Unknown family: neutral slate accent plus a generic model glyph and the actual model name—never guess a family from its display label. Family color is supplemental to the fixed-model header in thread view and must not imply quality, availability, or approval state.
