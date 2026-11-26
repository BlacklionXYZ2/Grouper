# What's waiting on Oscar

Everything in `Grouper — Proposal v3.docx` that could be built and checked without you is
built, tested and running at `https://grouper.clockmillbarn.uk` (branch `harness`). This
page lists what can't go further without you, with the options for each. Status as of
2026-11-26.

## Permissions

### Push `harness` to GitHub

- **Why it matters:** the macOS job in `.github/workflows/apple.yml` is the only way to
  compile the SwiftUI apps without your Mac. The Rust jobs already pass locally on Windows
  and Linux.
- **Why it's yours:** a push publishes the code to your GitHub repository.
- **Options:** push `harness` as a branch (nothing merges into `main`), or keep it local until
  you're on the Mac.

## Hardware

### Build the iPhone and Mac apps on a Mac with Xcode 26

- Run `scripts/build-apple.sh` (regenerates the Rust bindings; the Rust side changed since
  they were generated), then follow `apple/GrouperApp/README.md` (XcodeGen, then build).
- What's already verified: the `GrouperAPI` data layer (unit tests, and live tests against
  a real hub: streaming, reconnect, approvals, folding) and a syntax check of every
  SwiftUI file. A static review fixed about 36 API and concurrency issues; five points only
  Xcode can settle are marked `VERIFY ON MAC` in the code.

## GUI decisions (you said you'd decide these on 26 Nov)

Done and live: threads as a panel on their chat (no Threads tab), models grouped by provider,
thread rows with elapsed time and model-family colour, the chat-style thread view with the
model fixed, the covering model picker, the "Needs you" inbox, and the usage page as two
tracks (API costs per key; subscription usage/quota). The research and spec are in
`docs/gui-spec.md`; the navigation ideas are in `docs/gui-nav-ideas.md`.

1. **Phone tabs.** Fable recommends Chat / Needs you / Devices / More. Today: Chat / Devices /
   More, with "Needs you" as the first item in More and in the desktop sidebar. On the inbox
   page no phone tab is highlighted until this is settled.
2. **What More opens to:** a plain list of destinations, or a dashboard with the API-cost and
   subscription cards.
3. **A chat's threads on phone:** the header button (built) plus thread cards in the chat
   (built); keep both, or one.
4. **Desktop command palette** (Ctrl/⌘K): now, or later when there are more destinations.
5. **Concept art:** colours, shapes and brand are marked provisional in the spec until it
   arrives.
6. **Radium prices:** `radium-tycho` (your test dummy) has no prices, so its cost shows $0 and
   the picker says "No price set". Send prices if cost tracking on Tycho matters.
7. **OpenRouter credits:** without them the key allows 50 free-model requests a day across
   all models; OpenRouter says adding 10 credits raises it to 1,000. Spending money is yours.

## Design decisions

### 1. Offline writes on the phone

The phone caches chats and reads them offline (`core-sync`). Sending while offline isn't
built, because it needs a merge policy:

- **Queue and send later:** messages typed offline are sent in order when the hub is back.
  Simple; nothing is answered until then.
- **Answer on the device:** an API model answers on the phone, and the exchange is merged
  into the hub's log on reconnect. That needs rules for ordering against events that happened
  meanwhile, and for which provider keys a phone may hold.
- Also: whether memory edits made offline merge or last-writer-wins.

### 2. Limits, budgets and timeouts

All exist and are off (`grouper limits`). Values to choose:

- running threads, overall and per device;
- budget per thread, per conversation and per UTC day, and the warning share (default 80%);
- how long an approval waits, and whether it's then denied or the thread stopped;
- how long `ask_orchestrator` waits (30 minutes now).

### 3. Corvus

The proposal reserves Corvus as Grouper's own always-on worker agent, with scope "to be
decided": what it does, what it may do without asking, and whether it waits for an
always-on (VPS) hub.

### 4. A shell for hub threads

Hub threads get file tools only. A sandboxed shell means running each thread's commands in
a throwaway container, which needs the hub container to control Docker (mounting the
Docker socket, or a separate runner). Yes or no, and if yes, which approach.

## Housekeeping (no decision needed, just actions)

- **Hub model:** the online hub now uses free OpenRouter models, swapping to another free
  model when one is rate limited (`deploy/README.md` section 4). LM Studio is stopped and
  `gemma-local` is removed.
- **Port 1234:** with Hyper-V on, Windows reserves ports 1228–1327, so LM Studio's default
  port 1234 fails, which likely breaks pi's `lmstudio` provider. Use another port (45123
  works).
- **Test registrations** on the VM hub ("Oscar CLI", "Hub VM Linux") can be revoked:
  `docker compose -f deploy/compose.yaml exec hub grouper-hub device revoke <id>`.
  "Gaming PC" is this PC's device token; "Browser" is the web sign-in token; "Claude admin
  (PC)" is the admin login Claude uses from this PC's CLI (saved with DPAPI).
- **The sign-in token file** on your desktop (`grouper-sign-in-token.txt`) also holds the
  OpenRouter key on its last line. The key is now in the hub's encrypted store, so the
  file can be deleted once you've signed in.
- **Radium key:** it's in the hub's encrypted store; the two uploaded `Radium.md` files in
  this session's uploads folder can be deleted (the first key was invalid).
- **Don't run `cargo fmt` on the workspace:** it isn't rustfmt-formatted; fmt rewrites ~70
  files. Adopt a formatter only on purpose, with a `rustfmt.toml`, in its own commit.
- **Back up the master key** (`/secrets/master.key` in the `grouper-secrets` volume)
  somewhere safe; backups don't include it (`deploy/README.md` section 5).
