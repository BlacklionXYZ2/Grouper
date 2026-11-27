# What's waiting on Oscar

Everything in `Grouper — Proposal v3.docx` that could be built and checked without you is
built, tested and running at `https://grouper.clockmillbarn.uk` (branch `harness`). This
page lists what can't go further without you, with the options for each. Status as of
2026-11-26.

## Hardware

### Build the iPhone and Mac apps on a Mac with Xcode 26

- Run `scripts/build-apple.sh` (regenerates the Rust bindings; the Rust side changed since
  they were generated), then follow `apple/GrouperApp/README.md` (XcodeGen, then build).
- What's already verified: the `GrouperAPI` data layer (unit tests, and live tests against
  a real hub: streaming, reconnect, approvals, folding) and a syntax check of every
  SwiftUI file. A static review fixed about 36 API and concurrency issues; five points only
  Xcode can settle are marked `VERIFY ON MAC` in the code.

## GUI decisions

Decided on 26 Nov (recorded in `docs/gui-spec.md`, "Owner decisions (26 November 2026)"):
phone tabs stay Chat / Devices / More; More opens to a plain list; "Needs you" becomes a
system notification that opens an approval popup in the chat it came from (in place of the
composer, like Claude's permission prompts); the desktop command palette is being built;
no concept art yet. `harness` is pushed to GitHub, so `apple.yml` CI compiles the apps.

Still open:

1. **A chat's threads on phone:** the header button (built) plus thread cards in the chat
   (built); keep both, or one.
2. **Concept art:** colours, shapes and brand are marked provisional in the spec until it
   arrives.
3. **Radium prices:** `radium-tycho` (your test dummy) has no prices, so its cost shows $0 and
   the picker says "No price set". Send prices if cost tracking on Tycho matters.
4. **OpenRouter credits:** without them the key allows 50 free-model requests a day across
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

## Local-first: what only Oscar can do (27 November 2026)

The hub can now run inside the Windows app on your PC (see `docs/local-first-design.md`,
`docs/local-first-review-2026-11-27.md`, and the step-by-step `deploy/MOVE-TO-PC.md`). These steps
need you:

1. **Google Cloud (project `grouper-515912`):** create an OAuth client of type **Desktop app**. Send
   the client ID (it is public); keep the client secret in a file, never in chat. An agent then puts
   the ID in `DEFAULT_DESKTOP_CLIENT_ID` (`apps/desktop/src-tauri/src/oauth.rs`) and
   `DEFAULT_CLIENT_IDS` (`crates/hub/src/google_auth.rs`).
2. **Google Auth Platform → Branding:** point the privacy policy link at
   `https://grouper.clockmillbarn.uk/privacy`.
3. **Cloudflare → Caching → Configuration → Browser Cache TTL → "Respect Existing Headers"**, so a
   deploy shows up without a hard refresh.
4. **Move the hub to the PC:** follow `deploy/MOVE-TO-PC.md` (VM backup, copy `master.key`
   separately, stop the VM hub and its `cloudflared`, restore on the PC, point the tunnel at
   `http://127.0.0.1:47600`, sign in with Google in the Windows app, paste the tunnel token in
   Settings → Remote access).
5. **The PC's CLI sign-in was lost** (an agent's test overwrote `%APPDATA%\grouper\credentials.json`
   on 26 Nov): run `grouper login` once.
6. **Try on real devices:** the Windows installer (CI artifact `Grouper-windows-x64-setup`), the Mac
   app (`Grouper-macOS-arm64`), and the iPhone app (keyboard dismissal, Google-only sign-in, plan
   checklist in threads).

Open design points (no action yet):

- **A second desktop joining a PC hub through the tunnel:** the desktop app hands its window a
  session by trading its admin token at `POST /v1/session`, but a hub in local mode accepts token
  sign-in only from its own machine, so a second desktop in client mode must sign in with Google in
  the window instead. Fine for now; revisit when a second computer joins.
- **Default domain for other users:** apps default to `grouper.clockmillbarn.uk`; anyone else would
  reach your hub and be refused. Choose per-user discovery (e.g. the Drive record) before anyone
  else gets the app.
- **Mac as hub** (design package 11) waits until a real PC → other-machine move has worked.

## Models and harnesses (27 November 2026)

Built as `docs/models-and-harnesses-design.md` describes. Open points, each with the recommendation the build follows:

- **May the orchestrator give threads to a harness on its own?** Today it only does when asked by
  name. Recommendation: keep it off by default; add a per-harness switch "use for threads
  automatically", skipped when that subscription's plan window is above 90 %.
- **Harness detection reads whether Claude/Codex credential files exist and are unexpired** on every
  device, without the usage-probe opt-in (only "signed in / expired / not signed in" leaves the
  device). Recommendation: keep.
- **Try it on your PC:** with Claude Code signed in on the hub's computer, pick "Claude Code" as a
  project chat's orchestrator. Check that it only uses Grouper's tools, that a second message
  continues the same session, and that threads start. Many CLI flags are marked "VERIFY" in
  `crates/core-client/src/harness.rs`.

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

## Usage tab (27 November 2026)

Built per `docs/usage-tab-spec.md` (Fable's spec with your decisions at the top). Waiting on you:

- **Look at it:** sign in at `https://grouper.clockmillbarn.uk/#/usage` (or the new Usage tab on iPhone
  once the Apple build is installed). Subscription meters appear only for devices running a Grouper node
  with a harness signed in; this PC's node needs `grouper login` (or the Windows app) first.
- **Grok meter and sign-in expiry:** Grok's stored token lasts about an hour and only the Grok CLI renews
  it, so the card often says "sign-in expired". Options: leave it (it refreshes whenever you use Grok), or
  let Grouper renew the token itself, which writes Grok's sign-in file and could collide with the CLI.
  Recommendation: leave it.
- **Grok's on-demand and prepaid amounts** are left out until their unit is known.

