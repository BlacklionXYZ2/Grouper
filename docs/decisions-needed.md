# What's waiting on Oscar

Everything in `Grouper — Proposal v3.docx` that could be built and checked without you is
built, tested and running at `https://grouper.clockmillbarn.uk` (branch `harness`). This
page lists what can't go further without you, with the options for each. Status as of
2026-11-25.

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
- **Back up the master key** (`/secrets/master.key` in the `grouper-secrets` volume)
  somewhere safe; backups don't include it (`deploy/README.md` section 5).
