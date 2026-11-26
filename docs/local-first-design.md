# Local-first Grouper: design

Status: design, 26 November 2026 (late). Nothing here is built. It replaces the "Owner and
claim" and "Finding your hub" sections of `docs/accounts-design.md`; the rest of that
document (ID-token checks, `install_id` reuse, the device link flow, `grouper login`,
Google-only sign-in screens) still applies and is built on here.

Owner direction: no hub URL and no admin token at sign-in. You sign in with Google; the first
computer becomes the hub (a local node); other devices sign in with the same Google account and
sync with it. The cloud deployment stays in the docs until it can be justified as a business.

## Owner answers (26 November 2026, late)

- **Remote access:** move the Cloudflare tunnel from the VM to the hub PC. "Everything will still
  go through my domain by default (that should be how the app is configured) but the user could
  also change the domain in the settings." So the apps ship with `grouper.clockmillbarn.uk` as
  their default hub address (not shown at sign-in), changeable in Settings.
- **Picking the hub:** "could you have Fable come up with a way for a user to pick which device is
  the hub?" (design below, section 4a).
- **The hub is a device id (owner, 26 Nov, late):** "The hub isn't the domain. The hub is a device
  id. Since you already have a device id system, it should be pretty easy to attribute ids to an
  account and assign one as a hub." So the model is: **account** (the Google identity) → its
  **devices** (the existing device ids) → **`hub_device_id`**, one of them. Picking or moving the hub
  reassigns `hub_device_id` to another device id of the same account (and moves the data there).
  This supersedes the separate `hub_id` / `instance_id` identity used below: wherever this document
  says `hub_id`, read the account id; wherever it says `instance_id`, read `hub_device_id`. `epoch`
  stays only as a counter of hub assignments (it rises each time `hub_device_id` changes) so a stale
  hub can tell it was replaced. The domain is just the default address that reaches whichever
  device is the hub.
- **Windows app:** Tauri, as recommended.
- **Hub device to start:** the Windows PC.
- **Drive app-data record:** the owner asked what it is; not decided. With a default domain the
  apps can reach the hub without it, so the first build skips the extra Google scope and uses the
  domain plus mDNS at home; the Drive record stays an option for users without a domain.

## 1. The model

- **Account = one Google identity** (`sub`). There is no Grouper-run account database.
- **Hub = one of the account's computers.** It runs the same `grouper-hub` code as today
  (SQLite, orchestrator, threads, keys), inside an app or as a daemon.
- **Hub address:** every app ships with `https://grouper.clockmillbarn.uk` as its hub address.
  It is not shown at sign-in; Settings → Hub address changes it (the sign-in screen has a small
  "Hub address" link to the same field, for when you can't sign in). A Cloudflare tunnel on
  whichever device is the hub serves that address.
- **First sign-in on a computer** when no hub answers for this account: the user picks where
  the hub runs (section 4a). **Later sign-ins** elsewhere join it. **The hub can move** (4a).

| Data | Lives | Notes |
|---|---|---|
| Conversations, threads, events, memory, skills, registry, usage, limits | Hub only (`grouper.db`) | One writer, as now. |
| Provider keys, VAPID key, push subscriptions | Hub only, sealed under the master key | Devices hold provider keys in memory while connected, as now. |
| Master key, hub identity key, tunnel token | Hub only: Keychain (Mac), DPAPI file (Windows), 0600 file (Linux) | Never in a backup archive. Move with the hub (4a). |
| Owner (`sub`, email, name), device rows, token hashes | Hub only | |
| Hub identity seen by clients: `hub_id`, `epoch`, `instance_id`, `key_sha256` | Hub; cached on each device | `GET /v1/hub`. `epoch` rises by one on each move; `instance_id` is random per hub install. |
| Read cache of chats and threads | Each device (`core-sync`) | Read-only when the hub is away. |
| `install_id`, device token, hub address setting | Each device's secure store / settings | |

Not replicated: one hub, and devices are clients with a read cache. Multi-writer replication
needs the merge policy `docs/decisions-needed.md` lists as open; nothing here needs it.

## 2. Finding and reaching the hub

| Option | Gives | Costs and limits | Grouper server? | Status |
|---|---|---|---|---|
| **Default domain + Cloudflare tunnel on the hub** | One address that works at home and away, a real certificate, browsers and Web Push work | Someone must own a domain and a tunnel. Today that is only Oscar: for anyone else the default address is somebody else's hub (section 7, Q1). All traffic passes through Cloudflare, as it does today. | No | **First build** |
| **LAN discovery** (mDNS `_grouper._tcp`) | Reach at home when the internet or tunnel is down; faster local traffic | Same network only. Gives addresses, not trust, so it is only used for a hub whose key the device already cached. iOS shows a Local Network prompt. | No | **Second wave** |
| **Tailscale** | Remote reach without a domain | A second product on every device; its name goes in Settings → Hub address | No | Works already, undocumented |
| **Hub record in the owner's Google Drive app-data folder** | Per-account "where is my hub" with no domain typed | Extra Google scope (`drive.appdata`: Google lists it as non-sensitive, basic verification only, allowed in the device flow; whether adding it to a published consent screen triggers re-review is unchecked). A directory only: it can't carry traffic. | No | **Optional, later** |
| **Built-in peer-to-peer** | Remote reach with no setup | Needs relay servers someone runs (a third party's, or ours = cloud env); needs an iOS Rust slice | Maybe | Spike later |
| **Grouper-run directory or relay** | Smoothest; the only route to real iPhone push | A cloud env | **Yes** | Parked |

A phone away from home can't reach a home PC without a third machine helping. In this design
that machine is Cloudflare, through the owner's tunnel.

**How a device resolves the hub:** (1) the cached LAN address, if the hub there presents the
cached key; (2) mDNS for the cached `hub_id`, same key check; (3) the hub address from
Settings. Steps 1 and 2 arrive with the LAN package; until then every device uses step 3. A
device with no cached identity (first sign-in) only uses step 3, so a stranger's machine on the
LAN can never pose as the hub to a new install.

**The tunnel runs on the hub.** The hub keeps the Cloudflare tunnel token as one of its
secrets (entered once in the hub's Settings → Remote access, sealed like provider keys) and
runs `cloudflared` as a supervised child process. The tunnel's public hostname points at
`http://localhost:47600`, the hub's fixed port on every computer, so nothing in Cloudflare
changes when the hub moves. Before starting `cloudflared`, a hub always asks the address who
is serving it (4a, "step-down"): Cloudflare spreads traffic across every connector of a
tunnel, so two hubs must never both run it.

**Drive record (optional later).** A small `hub.json` (hub id, epoch, key hash, addresses; no
secrets) written by the hub to the owner's Drive app-data folder and read by devices at
sign-in. It would replace the default domain as the *finder* for users without one. Not built
now; nothing below depends on it.

## 3. Trust and sign-in without admin tokens

**Joining device → hub (unchanged rule).** The hub accepts a Google ID token only if the
signature, issuer, audience, times and `email_verified` pass, `sub` is the owner, **and the
`nonce` is one this hub issued in the last 10 minutes, unused.** The device knows its hub
address before it asks Google, so sign-in is one prompt: `GET /v1/auth/status` and
`POST /v1/auth/google/nonce` at the hub address, Google sign-in with that nonce (scopes
`openid email profile` only), `POST /v1/auth/google`.

Outcomes at the hub address:
- **Accepted:** joined. The device caches `hub_id`, `epoch`, `key_sha256`.
- **Refused, another account owns that hub:** "The hub at grouper.clockmillbarn.uk belongs to
  a different Google account." On a computer the app goes to "Where should Grouper run?"
  (4a); on a phone it shows the Hub address field.
- **No answer:** on a computer, "Where should Grouper run?"; on a phone, the waiting screen.

**Device keys per install.** As in the accounts contract: each install keeps a random
`install_id`; `POST /v1/auth/google {id_token, client, install_id, device_name}` returns that
install's device (same row if `(install_id, client kind)` exists, fresh token, old one
revoked). A Mac or Windows install has two rows: the admin client (the UI) and the non-admin
node that runs threads, so a thread shell never holds an admin credential. Tokens stay
`<device-id>.<secret>`, stored hashed, never shown.

**Why no setup code.** A setup code stops a stranger claiming a reachable, ownerless hub. A
local hub is created by the app the owner is sitting at: the app starts the hub in its own
process, listening on loopback only, and claims it by function call (`claim_local`) with the
ID token it just got from Google. That call checks signature, issuer, audience and times but
not the nonce, because the token never crossed a network on its way to this hub. The HTTP
claim path is off in local mode, and the tunnel and LAN listener start only once the hub has
an owner. Setup codes remain for server mode only (section 6).

**Keeping other Google accounts out.** One owner `sub` per hub; any other account is refused,
at a rate-limited endpoint. A token minted for one hub's nonce is useless at another, so
pointing an app at a wrong or hostile address leaks nothing reusable. Whoever controls the
Google account controls the Grouper account; recovery stays `grouper-hub owner reset` on the
hub machine.

**If the hub is offline:** devices show "Your hub (Oscar's PC) is offline. Last seen 21:40."
Cached chats stay readable; sending, approvals and settings are disabled. Nothing takes over
automatically: two hubs with different histories is worse than a pause. The offline screen on
a computer offers the recovery path in 4a. The hub app offers "Start at login" and "Keep awake
while plugged in", and warns when the chosen hub is a laptop.

## 4. Which devices can be the hub

| Device | Hub? | How |
|---|---|---|
| Windows app (new) | Yes | Tauri 2 app embedding the hub crate. **The first hub: Oscar's PC.** |
| Mac app | Yes | Hub embedded in the app process through `crates/ffi` |
| Linux, any headless machine | Yes | `grouper hub run` + a systemd user unit |
| iPhone, browser | No | Clients |

**Windows: Tauri 2 (decided).** The Rust host links the hub and the node (`core-client`) in
one process; the window is the existing web UI, so there is no third UI to write. When this PC
is the hub the window loads `http://localhost:47600`; when it isn't, it loads the hub address.
Either way the app signs in with Google through the system browser and a loopback redirect,
then trades its admin token for a session cookie (`POST /v1/session`) so the window opens
signed in. Tray icon, autostart, single instance. WebView2 does not deliver Web Push (reported
by Tauri users, untested here), so notifications use native toasts. Rejected: Electron (ships
a browser, hub becomes a sidecar), WinUI (a third UI), a tray program that opens the default
browser (no window of its own).

**Mac: embed, not a helper.** `ffi` gains a `HubHost` object (`start`, `stop`, `claim_local`,
`status`). The Mac app is a menu-bar app, so the hub lives as long as the menu-bar item;
"Start at login" registers the app (`SMAppService.mainApp`). A separate helper would survive
app crashes better but doubles signing and packaging; move to one only if needed.

**Linux:** `grouper hub init` signs in (browser loopback, or `--device` for Google's device
flow), claims in process and writes a systemd unit; `grouper hub status|move|take` mirror 4a.

## 4a. Picking the hub device

### Who is asked, and when

- **First computer** (sign-in found no hub for this account): a screen, "Where should Grouper
  run?" with three choices. **This computer** (default; "Grouper runs here and your other
  devices connect to it. Pick a computer that stays on."; a laptop on battery adds "This is a
  laptop: when it sleeps, your other devices can't use Grouper."). **Another computer, already
  set up** (shows the hub address and Try again; for a hub that is merely off). **Restore from
  a backup** (recovery, below). No device list here: only a hub knows the account's devices.
- **A second computer joining:** no question. It joins, and Devices shows a one-time,
  dismissible card: "Oscar's PC is your hub. Move it to this Mac?"
- **Later, from any signed-in device, phone included:** Devices → **Hub** card at the top
  (name, platform, Online, "Hub since 26 Nov") → **Change hub…**.
- **Phone first:** sign-in can't create a hub. The app shows "Grouper runs on one of your
  computers. Install it on a Mac or PC and sign in with this Google account", rechecks the hub
  address every few seconds while open, and continues by itself. A phone can start a move but
  never appears as a target.
- **CLI:** `grouper hub status`, `grouper hub move <device>`, `grouper hub take`.

### The list

Change hub… lists **installs**, not rows (an app's client and node rows share an `install_id`
and show as one device). Eligible: platform `macos`, `windows` or `linux` whose node reports
it can host. The node's hello frame gains `host: {can_host, app_version, free_bytes,
cloudflared}`.

| Row | Shown as |
|---|---|
| Current hub | "Oscar's PC · Windows · Hub", ticked, not selectable |
| Eligible and online | Selectable; "Online"; a note if `cloudflared` is missing ("Remote access will stop until cloudflared is installed there") or disk is short |
| Eligible, offline | Disabled; "Offline · last seen 21:40. Turn it on to move the hub there." |
| Older app version | Disabled; "Update Grouper on this device first." |
| Phones, browsers, CLI-only tokens | Under "Can't be the hub", greyed |

Choosing a row opens a confirmation: what moves, how long (from the snapshot size), "Grouper
pauses for everyone during the move", and, if hub threads are running, **Wait for them** or
**Stop them and move now**. Confirming needs a fresh Google sign-in on the device you're
holding (a hub nonce, not older than 5 minutes), so a stolen unlocked session can't move the
hub. `POST /v1/hub/move {target_device, id_token, threads: "wait"|"stop"}` (admin).

### The move

Both machines must be online. The old hub drives it; state is `idle → preparing → copying →
switching → done | rolled_back`, in `GET /v1/hub/move` and as events so every client shows
"Moving your hub to Oscar's Mac… copying (62%)".

1. **Preflight** (old hub): target online and can host, same or newer app version, free space
   at least twice the snapshot, no move running. Any failure ends here with a plain message.
2. **Pause.** The old hub refuses new chats, threads and settings writes (503 "Your hub is
   moving"). Hub threads finish or are stopped as chosen (stopped ones end "Stopped: the hub
   moved"). Threads running *on devices* are not touched; they report to the new hub when they
   reconnect. Pending approvals are data and move with it.
3. **Copy.** The old hub sends the target's node a one-time ticket over its device connection.
   The target downloads `GET /v1/hub/move/{ticket}/snapshot`: the existing backup archive
   (`grouper.db` by SQLite online backup, workspaces, manifest with SHA-256s), which includes
   device rows, sessions, push subscriptions and the sealed provider keys. It verifies every
   hash into `<data>/incoming-<time>` and touches nothing else yet.
4. **Secrets.** The target sends a one-time public key; the old hub returns the master key,
   hub identity key and tunnel token encrypted to it. The target stores them in its own secure
   store and runs the existing restore check (the master key decrypts the provider keys). This
   keeps secrets out of logs and caches; it does not hide them from the tunnel operator, which
   already carries provider keys to devices today.
5. **Switch** (the commit point). The target reports ready. The old hub stops `cloudflared`
   and its listeners. The target installs the staged data, sets `epoch + 1` and a new
   `instance_id`, starts the hub still paused, starts `cloudflared` and mDNS, and then fetches
   the hub address until it sees its own `instance_id` (up to 2 minutes). Then it unpauses.
6. **Old hub becomes a device.** It polls the hub address; on seeing the higher epoch it
   reconnects its app as an ordinary client and node using the tokens it already has (their
   rows moved with the database). Its old data and secrets are kept as `pre-move-<time>` for 7
   days, then deleted; Settings shows "Delete the old copy now".

Same `hub_id`, same tokens, same domain, same identity key: nobody signs in again and Web Push
subscriptions keep working. Existing backup files stay where they are on the old machine and
stay restorable (same master key); the scheduled-backup setting moves, and its folder is
checked on the target (if missing, backups pause with a warning in Settings).

**Failure and rollback.** Before step 5, any failure (target lost, hash mismatch, key check,
disk full, user cancels) deletes the staging folder, unpauses the old hub, and reports why;
nothing changed. At step 5, if the target doesn't confirm in 2 minutes the old hub restarts
from its untouched data with `epoch + 2` and restarts the tunnel; the target sees a higher
epoch than its own, discards its copy and stays a device. The target takes no writes until it
has confirmed, so the two copies can't diverge. With no remote address configured (LAN only)
the confirmation is a direct call between the two machines instead of the domain check.

**Step-down rule.** Whenever a hub starts, and every minute while it runs, it asks the hub
address (and mDNS) who is serving. Same `hub_id`, another `instance_id`, higher epoch: it
stops serving, never starts `cloudflared`, and becomes a device. Lower epoch elsewhere: it
keeps serving and the other one steps down. So a hub that was asleep through a move or a
recovery can't come back and split traffic.

**Recovery, when the old hub is gone.** On a computer: "Where should Grouper run?" → Restore
from a backup: pick a backup archive and the master key file (as `grouper-hub restore` needs
today); the hub starts with `epoch + 1`; the tunnel token is pasted again from Cloudflare.
With no backup, **This computer** starts an empty hub with a new `hub_id`; other devices see
the change and say "Your hub was replaced on 27 Nov. Sign in again." (cached chats stay
readable, marked as from the old hub).

### How other devices learn the new hub

- **Domain:** nothing to learn. The address is the same and now ends at the new hub; clients
  reconnect with their existing tokens, see `epoch + 1`, and update the cached hub name.
- **mDNS:** the new hub advertises the same `hub_id`; clients re-query, check the cached key
  (the identity key moved, so the pin still passes) and store the new LAN address.
- **Cached identity:** each device remembers the highest epoch it has seen and refuses a hub
  offering a lower one, so a stale old hub on the LAN is ignored even before it steps down.
- A changed `hub_id` at the address is never accepted silently (see Recovery).

## 5. The web UI

The web UI stays the hub's own UI and is the Windows app's whole front end.

- **At the hub address** (`https://grouper.clockmillbarn.uk`): any browser, as today, with
  Google sign-in through the return page and Web Push.
- **On the hub machine:** also at `http://localhost:47600` (a secure context; Google returns
  to `http://localhost:47600/oauth/google/return`, registered on the web client).
- **LAN browsers on other devices** are not supported: the LAN listener uses a self-made
  certificate that apps pin and browsers would warn about. They use the hub address.

Changes: Google-only sign-in (no token form or setup-code field in local mode); hub-offline
and hub-moving states; the Hub card, Change hub… list and move progress in Devices; hub
Settings gains Remote access (address, tunnel token, `cloudflared` status), Start at login and
backups; the approve screen for the device link flow.

## 6. Keep, change, drop

| Today | Verdict |
|---|---|
| **VM + Docker deployment** (`deploy/`) | **Keep as documentation** ("server mode", parked). Oscar's data moves VM → PC with the existing `backup` / `restore` plus the master key. |
| **Cloudflare tunnel** | **Change:** runs on the hub device, started by the hub, and follows it. The VM's connector is removed once the PC serves. |
| **Hub URL field in apps** | **Change:** gone from sign-in; default built in; Settings → Hub address. |
| **Setup codes** | **Drop from local mode** (in-process claim). Server mode only. |
| **Admin tokens** | **Keep internally, never typed.** `GROUPER_TOKEN` and `grouper-hub device add` stay for tests, automation and recovery. |
| **`#/token-signin`** | **Keep, unlinked, loopback only by default.** e2e tests run on loopback. |
| **Device link flow** | **Keep:** how a headless CLI joins; approvable from any signed-in app. |
| **`grouper login`** | **Change:** `--hub` becomes optional, defaulting to the built-in address. |
| **Return page and privacy page** | **Keep on the hub address for now** (they move with the hub and are down when it is). Move both to a static host before anyone else relies on them. |
| **Web Push** | **Keep:** works at the hub address and on localhost. Windows app: native toasts. |
| **iPhone notifications** | **Unchanged and limited:** local notifications while the app is alive. Real iPhone push needs an Apple push key on a server (a cloud env). |

## 7. Owner questions

Answered: remote access through your domain, Tauri, the PC as first hub (top of this page).

1. **The default address for anyone who isn't you.** Every install will ask *your* hub whether
   it knows them and be refused; your hub's log sees their sign-in attempt. *Recommend:* accept
   for now, while the users are you; before giving the app to anyone else, pick per-user
   discovery (the Drive record or a small directory) and ship an empty default.
2. **`cloudflared` on the hub:** use an installed copy and show a one-line install command when
   it is missing, or have the app download it? *Recommend installed copy first;* bundling needs
   a licence and update check.
3. **A recovery file:** one passphrase-protected file holding a backup, the master key and the
   tunnel token, written on a schedule to a folder you choose, so a dead hub is replaceable
   without hunting for the key. *Recommend yes, after the move works.*
4. **Drive record:** still undecided. *Recommend leaving it* until question 1 forces a choice.
5. **Actions only you can do:** create a Desktop OAuth client (Windows app and CLI); register
   `http://localhost:47600/oauth/google/return` on the web client; change the tunnel's service
   to `http://localhost:47600`; install `cloudflared` on the PC.

## 8. Build plan

Wave 1 packages run in parallel and share no files. Later packages name what they wait for.
"CI" is `rust.yml` unless stated.

| # | Package | Owns | Verify | Needs |
|---|---|---|---|---|
| **Wave 1** | | | | |
| 1 | **Embeddable hub and local claim.** Serving moves from `main.rs` into `host` (`start(data, binds) → handle`); `backup` joins the library; `claim_local`; local mode (HTTP claim off, token sign-in loopback only); accounts own device ids and one is the hub (`hub_device_id`; owner answer above); `/v1/hub` gains `account`, `hub_device_id`, `epoch`, `key_sha256`. | `crates/hub/src/{host.rs (new), main.rs, lib.rs, backup.rs, google_auth.rs, session.rs, db.rs}`, `server.rs` (`hub_info` only), `crates/core-protocol` | `cargo test --workspace`, `scripts/e2e.*` unchanged; new tests: local claim, HTTP claim refused in local mode, token sign-in refused off loopback | — |
| 2 | **Web sign-in for local mode.** Google-only screen, localhost return, hub-offline state. | `crates/hub/web/views/login.js`, `app.js`, `css/brand.css` | Local hub + headless browser; `sw.test.mjs` | Q5 for a real sign-in |
| 3 | **Apple apps: no URL, no token.** Built-in hub address, sign-in is one Google button, Settings → Hub address, refused / no-answer / phone waiting screens. | `apple/GrouperApp/Shared/{SignInView,SettingsView,AppModel}.swift`, `apple/GrouperKit/Sources/GrouperAPI/HubAddress.swift` (new) + its tests | `swift test`; **macOS CI** (`apple.yml`); **real iPhone and Mac** against today's hub at the domain | — |
| 4 | **CLI default address.** `--hub` optional everywhere; `grouper login` works with no arguments. | `crates/cli/src/{main.rs,credentials.rs}` | Unit tests for address resolution; manual `grouper login` on Windows | — |
| 5 | **Docs and Oscar's move VM → PC.** Native Windows hub run + `cloudflared` on the PC, restore from the VM backup, `deploy/` marked server mode, README. | `README.md`, `deploy/**`, `docs/accounts-design.md`, `docs/decisions-needed.md` | `curl https://grouper.clockmillbarn.uk/healthz` served from the PC; phone still signed in | Owner at the PC |
| **Wave 2** | | | | |
| 6 | **Tunnel manager.** Sealed tunnel token, find and supervise `cloudflared`, ask-before-start, step-down loop. | `crates/hub/src/tunnel.rs`, `stepdown.rs` (new) | Tests with a fake `cloudflared` binary and a fake peer hub; manual on the PC | 1 |
| 7 | **Windows app.** Tauri shell: hub host + node, "Where should Grouper run?", tray, autostart, Google sign-in, session hand-off, toasts, installer. | `apps/desktop/**`, `.github/workflows/desktop.yml` (new) | CI build on `windows-latest`; **real Windows PC** | 1, 2; Q5 |
| 8 | **Hub move, hub side.** `host` field in the hello frame, install-level device list, move state machine, ticketed snapshot, secrets hand-over, rollback. | `crates/hub/src/migrate.rs` (new), `devices.rs`, `crates/core-client/src/lib.rs` | Integration test with two hubs in one process: move, old one steps down, a device token still works; forced failure at each step rolls back | 1, 6 |
| 9 | **Hub picking, web.** Hub card, Change hub… list, confirmation, progress, moving and replaced states; Remote access settings. | `crates/hub/web/views/{devices.js,settings.js}`, `css/` new file | Headless browser against a hub with fake devices; axe | 8's API shape (can start from this document) |
| 10 | **Hub picking, Apple.** The same in SwiftUI, including the phone as initiator. | `apple/GrouperApp/Shared/DevicesView.swift`, `GrouperAPI/HubMove.swift` (new) | `swift test`; **macOS CI**; **real iPhone** | 8's API shape; after 3 |
| 11 | **Mac as hub.** `HubHost` in ffi, hub controller, Keychain secrets, start at login, first-computer screen. | `crates/ffi/src/lib.rs`, `scripts/build-apple.sh`, `apple/GrouperApp/macOS/**`, `apple/GrouperKit/Sources/GrouperKit/**` | **macOS CI**; **real Mac** (sleep, firewall, login item); a real PC → Mac move | 1, 6, 8 |
| 12 | **Linux hub commands.** `grouper hub init/run/status/move/take`, systemd unit. | `crates/cli/src/hub.rs` (new) | Tests with a fake Google; manual on Linux | 1, 8; after 4 |
| **Later** | | | | |
| 13 | **LAN path.** Identity certificate, LAN TLS listener, mDNS, pinned clients (Rust and Swift). | `crates/hub/src/lan.rs` (new), `crates/core-sync/src/client.rs`, `GrouperAPI/HubResolver.swift` (new) | Pinned client connects, wrong pin fails; **real iPhone** for the Local Network prompt | 1 |
| 14 | **Recovery file** (Q3). 15 **Drive record** (Q4). 16 **Peer-to-peer spike.** | to be scoped | | Owner answers |

Package 3 alone removes the hub URL and admin token from the Apple sign-in screens, against
the hub that runs today. Package 5 puts the hub on the PC before the Windows app exists.

### Not verified in this design

- That a thread running on a device survives its hub connection dropping and reports to the
  hub afterwards (the README describes continuing after a reconnect; the move relies on it).
- How fast Cloudflare stops routing to a stopped connector (the 2-minute confirmation window
  is a guess), and `cloudflared`'s licence terms for bundling.
- That today's hub at the domain is linked to Oscar's Google account, which package 3 needs.
- That WebView2 inside Tauri cannot receive Web Push (reported by Tauri users, not tested).
- Whether iOS accepts a pinned self-made certificate on a LAN address without extra App
  Transport Security exceptions (package 13).
- Google Console behaviour when a scope is added to a published app (only matters for the
  Drive record).
