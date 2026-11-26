# Local-first Grouper: design

Status: design, 26 November 2026 (late). Nothing here is built. It replaces the "Owner and
claim" and "Finding your hub" sections of `docs/accounts-design.md`; the rest of that
document (ID-token checks, `install_id` reuse, the device link flow, `grouper login`,
Google-only sign-in screens) still applies and is built on here.

Owner direction: no hub URL and no admin token in any app. You sign in with Google; the first
device becomes the hub (a local node); other devices sign in with the same Google account and
sync with it. The cloud deployment stays in the docs until it can be justified as a business.

## Owner answers (26 November 2026, late)

- **Remote access:** move the Cloudflare tunnel from the VM to the hub PC. "Everything will still
  go through my domain by default (that should be how the app is configured) but the user could
  also change the domain in the settings." So the apps ship with `grouper.clockmillbarn.uk` as
  their default hub address (not shown at sign-in), changeable in Settings.
- **Picking the hub:** "could you have Fable come up with a way for a user to pick which device is
  the hub?" (design below, section 4a).
- **Windows app:** Tauri, as recommended.
- **Hub device to start:** the Windows PC.
- **Drive app-data record:** the owner asked what it is; not decided. With a default domain the
  apps can reach the hub without it, so the first build skips the extra Google scope and uses the
  domain plus mDNS at home; the Drive record stays an option for users without a domain.

## 1. The model

- **Account = one Google identity** (`sub`). There is no Grouper-run account database.
- **Hub = one of the account's devices.** It runs the same `grouper-hub` code as today
  (SQLite, orchestrator, threads, keys), inside an app or as a daemon, on the owner's machine.
- **First sign-in** on a device, when the account has no hub: that device becomes the hub
  (if it can host one, section 4). **Later sign-ins** elsewhere join that hub.
- **The hub can move** to another hub-capable device (section 4).

Where account data lives:

| Data | Lives | Notes |
|---|---|---|
| Conversations, threads, events, memory, skills, registry, usage, limits | Hub only (`grouper.db`) | One writer, as now. |
| Provider keys, VAPID key | Hub only, sealed under the master key | Devices hold provider keys in memory while connected, as now. |
| Master key, hub identity key | Hub only: Keychain (Mac), DPAPI file (Windows), 0600 file (Linux) | Never in Drive, never in a backup archive. |
| Owner (`sub`, email, name), device rows, token hashes | Hub only | |
| Read cache of chats and threads | Each device (`core-sync`) | Read-only when the hub is away. |
| `install_id`, device token, Google refresh token | Each device's secure store | |
| **Hub record** (below) | The owner's Google Drive app-data folder | Small, no secrets. The only thing outside the owner's devices. |

Not replicated: there is one hub and devices are clients with a read cache. Multi-writer
replication would need the merge policy `docs/decisions-needed.md` already lists as open, and
nothing in the new direction needs it.

## 2. Finding the hub with no URL

A joining device needs three things: which device is the hub, an address that works from
where it is now, and proof the thing at that address is really the owner's hub.

### Options

| Option | Gives | Costs and limits | Needs a Grouper server |
|---|---|---|---|
| **LAN discovery** (mDNS `_grouper._tcp`) | Addresses at home, no setup | Same network only. Anyone on the LAN can advertise a fake hub, so it gives addresses, not trust. iOS shows a Local Network prompt and needs `NSBonjourServices`. Some routers and guest Wi-Fi block mDNS. | No |
| **Hub record in the owner's Drive app-data folder** | "Which hub, what key, what addresses" from anywhere with internet, readable only by that Google account through Grouper's OAuth clients | One more consent line. It is a directory, not a connection: it cannot carry traffic through NAT. Depends on Google being reachable at sign-in (already true). | No |
| **User-run overlay or tunnel** (Tailscale, Cloudflare Tunnel) | Remote reach to a home PC. Tailscale: free personal plan, HTTPS name with a real certificate via `tailscale serve`. Cloudflare Tunnel: public HTTPS origin, no VPN on the phone. | The owner installs and signs in to a second product. Tailscale must be on on the phone. A tunnel needs a domain and puts the sign-in surface on the internet. | No (third party) |
| **Built-in peer-to-peer** (for example iroh: dial by public key, QUIC hole punching, relay fallback) | Remote reach with nothing for the user to set up | Hole punching always needs a rendezvous or relay server somebody runs: the library vendor's public relays (third party; terms and rate limits must be checked) or our own (a cloud env). The iPhone would need the Rust core as an iOS slice, which doesn't exist yet. Browsers can't use it. | No if a third party's relays are acceptable; **yes** otherwise |
| **Grouper-run directory or relay** | The smoothest experience, and the only way to send iPhone push (section 6) | It is a cloud env, with accounts' hub addresses or traffic passing through it. | **Yes** |

A phone away from home cannot reach a PC behind a home router with no third machine helping.
Every remote option above uses one: Google only helps you *find* the hub; Tailscale,
Cloudflare, a relay vendor or Grouper has to help you *reach* it.

### Recommendation

1. **Drive hub record for identity and addresses** (every platform).
2. **mDNS and the record's LAN addresses for reach at home.**
3. **Remote reach, now: a remote address in the record.** If the owner runs a Cloudflare
   Tunnel or Tailscale, the hub is told its remote address once (Settings on the hub, or
   detected from `tailscale status`) and publishes it in the record. Devices still never see a
   URL field. Oscar's existing tunnel (`grouper.clockmillbarn.uk`) moves from the VM to
   `cloudflared` on the PC.
4. **Remote reach, later: built-in peer-to-peer**, after a spike and an owner decision on
   third-party relays (section 7). Not in the first build.
5. **No Grouper-run server.** Two static pages stay on a static host (section 6); they hold
   no data.

Resolution order on a device: cached record → mDNS → record's LAN addresses → record's remote
addresses. The first address that answers and passes the identity check wins; the result is
cached. On failure the app re-reads the record (the hub may have moved).

### The hub record

One JSON file, `hub.json`, in the Drive `appDataFolder`, written by the hub and read by
devices:

```json
{ "v": 1, "hub_id": "…", "epoch": 3, "device_name": "Oscar's PC", "platform": "windows",
  "key_sha256": "…", "lan": ["192.168.1.20:47600"], "mdns": "grouper-ab12",
  "remote": ["https://grouper.clockmillbarn.uk"], "updated": 1795737600 }
```

- `key_sha256` pins the hub's TLS certificate on LAN addresses. `remote` addresses use
  ordinary web certificates, and the device checks `GET /v1/hub` returns the same `hub_id`.
- `epoch` rises by one each time the hub moves. A hub that reads a higher epoch than its own
  stops serving (section 4).
- The hub rewrites it when its addresses change and at least daily. Drive lets two files
  share a name, so readers list all `hub.json` files and take the highest epoch, then the
  oldest `createdTime`; a hub that loses that tie deletes its file and joins instead.
- Before a hub exists the file is absent. An iPhone-first sign-in writes nothing (section 4).

### The Drive scope: what is known, what to check

Known (Google's "Choose Google Drive API scopes" page, read 26 Nov 2026):
`https://www.googleapis.com/auth/drive.appdata` is listed under **non-sensitive** scopes,
which "only require basic OAuth App Verification". It is not sensitive or restricted, so no
security assessment. It only reaches a hidden per-app folder, not the user's files. Google's
device-flow page (updated 19 Nov 2026) lists `drive.appdata` among the scopes that flow allows,
so a headless CLI can read the record too.

To check in the Cloud Console before building on it:
- Whether adding a scope to the already published consent screen triggers a new review, and
  how long the app shows as unverified meanwhile.
- That the Drive API is enabled for the project, and the consent text
  ("See, create, and delete its own configuration data in your Google Drive") is acceptable.
- The privacy policy must say what is stored in Drive (the record above, nothing else).
- The app-data folder is per Cloud project, not per client: confirm the iOS, Mac, desktop and
  web clients all see the same folder (they should, as one project).
- Google's consent screen lets a user untick a scope. Without it the app falls back to the
  LAN and to "Add a device" (a QR or link shown by a signed-in device, carrying the record).

## 3. Trust and sign-in without admin tokens

**Joining device → hub (unchanged rule).** The hub accepts a Google ID token only if the
signature, issuer, audience, times and `email_verified` pass, `sub` is the owner, **and the
`nonce` is one this hub issued in the last 10 minutes, unused.** Nothing weaker is added.

The nonce has to be in the Google request, so the device must know its hub before asking
Google for the ID token. Order of work on a joining device:

1. Look for a hub first: cached record, then mDNS. If one is found, fetch its nonce and do
   **one** Google sign-in asking for `openid email profile drive.appdata` with that nonce.
   Then read the record and check the hub it names is the hub that issued the nonce
   (`hub_id` and key). If so, present the ID token. One prompt.
2. Nothing found (first sign-in away from home): sign in once for the Drive scope, read the
   record, reach the hub, fetch its nonce, then run a second short Google request with
   `login_hint` for the ID token. Two prompts, once per install. (Considered and rejected:
   accepting a device-made nonce. It would let any hub that sees a token replay it.)
3. The app keeps the Google refresh token to re-read the record later. It is never sent to
   the hub, except on the hub's own device, where the hub needs it to publish the record.

**Device keys per install.** As in the accounts contract: each app or CLI install keeps a
random `install_id`. `POST /v1/auth/google {id_token, client, install_id, device_name}`
returns that install's device: the same row and id if `(install_id, client kind)` exists,
with a fresh token and the old one revoked; otherwise a new row. A Mac or Windows install has
two rows: the admin client (the UI) and the non-admin node that runs threads
(`InstallID.node`), so a thread shell can never reach an admin credential. Tokens stay
`<device-id>.<secret>`, stored hashed, never shown.

**Why no setup code.** The setup code exists to stop a stranger claiming a hub that is
reachable before it has an owner. A local hub is created by the app the owner is sitting at:
the app starts the hub in its own process and claims it through a function call
(`claim_local`), passing an ID token the app itself just received from Google. No network
request can claim an unclaimed hub, because the HTTP claim path is switched off in local
mode, and the LAN listener doesn't start until the hub has an owner. For the Linux daemon the
same call is made by `grouper hub init` in the same process. Setup codes remain only for
server mode (section 6).

**Keeping other Google accounts out.**
- The hub has exactly one owner `sub`; any other account's token is refused (as now).
- Another account can't read this account's record, so it never learns the key or addresses.
  On the same LAN it can see that a hub exists and can reach the sign-in endpoint, which is
  rate-limited and refuses it.
- A fake hub on the LAN can't pass the key pin, and a token minted with its nonce is useless
  at the real hub.
- Someone who controls the owner's Google account controls the Grouper account. That is the
  meaning of "account = Google identity". Recovery stays `grouper-hub owner reset` on the hub.

**If the hub device is offline** (asleep, shut down, away):
- Devices show "Your hub (Oscar's PC) is offline. Last seen 21:40." Cached chats and threads
  stay readable (`core-sync`); sending, approvals and settings are disabled.
- A thread already running on a device keeps going and reports when the hub returns, as it
  does today on reconnect. Nothing new starts.
- No automatic takeover: two hubs with different histories is worse than a pause. The offline
  screen offers **Make this device the hub** only as a deliberate recovery (section 4).
- So the hub should be the machine that is on most. Sign-in on a laptop says so, and the hub
  app offers "Start at login" and "Keep awake while plugged in".

## 4. Which devices can be the hub

| Device | Hub? | How |
|---|---|---|
| Mac app | Yes | Hub embedded in the app process through `crates/ffi` |
| Windows app (new) | Yes | Tauri app embedding the hub crate |
| Linux, and any headless machine | Yes | `grouper hub init` / `grouper hub run` + a systemd user unit |
| iPhone | No | Client only |
| Browser | No | A view of a hub |

**Mac: embed, not a helper.** `crates/hub` is already a library; `ffi` gains a `HubHost`
object (`start(data_dir)`, `stop()`, `claim_local(id_token)`, `port()`), run on the tokio
runtime the node already uses. The Mac app is a menu-bar app that keeps running with its
window closed, so the hub lives as long as the menu-bar item; "Start at login" registers the
app itself (`SMAppService.mainApp`). A separate login-item helper would survive app updates
and crashes better but doubles the signing, packaging and IPC work; move to it only if
in-process proves fragile. The hub's master key goes in the Keychain.

**Windows: Tauri 2, recommended.**

| Choice | For | Against |
|---|---|---|
| **Tauri 2** | Rust host, so the hub and the node (`core-client`) link in directly, in one process. The window is the existing web UI loaded from the local hub: no third UI to write. Tray icon, autostart, single instance, small installer. Also builds for Linux desktop later. | WebView2 doesn't deliver Web Push, so notifications go through the native toast plugin. A new toolchain in the repo. |
| Electron | Mature, real Chromium (Web Push works) | Ships a browser (100 MB+); the hub becomes a sidecar process; a Node layer that does nothing for us. |
| WinUI 3 / C# | Most native look | A third UI to build and keep in step; Rust bindings for C# are not in the repo. |
| Tray program that opens the default browser | Smallest; a real browser | No app window or icon of its own; the tab can be closed or lost; feels like a dev tool. |

The Tauri app signs in with Google through the system browser and a loopback redirect, claims
or joins in process, then opens the webview already signed in (a one-time local ticket is
exchanged for the session cookie). It replaces `grouper connect` on Windows for normal use.

**Linux:** `grouper hub init` signs in (browser loopback, or `--device` for Google's device
flow on a headless box), claims in process and writes a systemd user unit. With the device
flow the hub itself receives the ID token straight from Google's token endpoint, so no nonce
is involved and none is needed: the token never passes through another party.

**iPhone first.** The sign-in succeeds (Google identity confirmed, no record found), and the
app says: "Grouper runs on one of your computers. Install it on a Mac or PC and sign in with
this Google account." It rechecks the record and continues by itself once a hub appears.
Nothing is created meanwhile, because an account with no hub has no data. So the rule is
"the hub is the first *computer* you sign in on".

**Moving the hub** (target must be hub-capable; both online):

1. On the target: Devices → **Make this device the hub**, then a fresh Google sign-in
   (the move is confirmed by the account, not just by a stored token).
2. The old hub stops new work, waits for or stops running hub threads, and takes a snapshot
   with the existing backup code (`grouper.db` via SQLite online backup, workspaces, manifest
   with SHA-256s).
3. It streams the snapshot plus the master key and hub identity key to the target over the
   pinned connection. The target verifies with the existing restore checks (hashes, the key
   decrypts provider keys) before it replaces anything.
4. The target starts serving with the same `hub_id` (device caches and tokens stay valid),
   writes the record with `epoch + 1` and its own addresses, and receives the Google refresh
   token from its own app.
5. The old hub sees the higher epoch, deletes its key copies, keeps its data aside as
   `pre-move-<time>` for a week, and reconnects as an ordinary device.

If the old hub is gone for good: on any hub-capable device, **Make this device the hub** →
restore from a backup file, or start fresh. Either writes `epoch + 1`, so the old machine
steps down if it ever comes back. Whether backups also go to Drive is an owner question.

## 5. The web UI

The web UI stays the hub's own UI and becomes the Windows app's whole front end.

- **On the hub machine:** the hub serves it on `http://localhost:47600` (fixed default port,
  so the Google redirect can be registered). The Tauri window loads it; any browser on that
  machine can too. `localhost` is a secure context, so cookies, the service worker and Google
  sign-in work. Web sign-in on localhost redirects to
  `http://localhost:47600/oauth/google/return`, registered on the web OAuth client: no shared
  return page involved.
- **On the LAN:** the hub also listens on the LAN with TLS and a self-made certificate that
  apps pin. A browser can't pin and would show a certificate warning, so **browsers on other
  devices are not a supported LAN path**. Other devices use the apps.
- **Remote, with a real HTTPS address** (the owner's tunnel or Tailscale name): the web UI
  works in any browser as today, through the shared return page.
- The hub listens on loopback (plain HTTP) and, once claimed, on the LAN (TLS). Windows and
  macOS will show a firewall prompt the first time.

Web UI changes needed: Google-only sign-in (no token form, no setup-code field in local
mode); a "hub offline / this is your hub" state; Devices shows which device is the hub and
offers the move; an approve screen for the device link flow; hub Settings gains "Remote
address" and "Start at login".

## 6. Keep, change, drop

| Today | Verdict |
|---|---|
| **VM + Docker deployment** (`deploy/`, `Dockerfile`) | **Keep as documentation**, renamed "server mode" and marked parked. Not the default. Oscar's data moves VM → PC with the existing `backup` / `restore`. |
| **Cloudflare tunnel** | **Change:** optional. For Oscar, `cloudflared` runs on the PC and the address is published in the record. Not required for anyone. |
| **Setup codes** (`#/claim`, `grouper-hub setup-code`) | **Drop from local mode** (replaced by the in-process claim). Kept for server mode only. |
| **Admin tokens** | **Keep internally, never typed.** Apps lose the hub URL and token fields. `GROUPER_TOKEN` and `grouper-hub device add` stay for tests, automation and recovery on the hub machine. |
| **`#/token-signin`** | **Keep, unlinked, loopback only by default** (a server-mode switch re-enables it elsewhere). e2e tests run on loopback and keep working. |
| **Device link flow** (`/v1/auth/link/*`, `#/link`) | **Keep.** It is the join path for a headless CLI and the fallback when the Drive scope is refused. Approvable from any signed-in app, not just the web. |
| **`grouper login`** | **Change:** no `--hub`. Signs in with Google, finds the hub through the record, joins. |
| **Shared return page** (`/oauth/google/return` on Oscar's hub) | **Change:** move to a static host under `clockmillbarn.uk`, together with the **privacy page**. Both are plain files; Google's consent screen needs the privacy URL to stay up when the VM is off. Only remote browsers use the return page. |
| **Web Push** | **Keep where it can work.** The hub sends pushes outbound, so it needs no public address itself, but the browser must subscribe from a secure origin: `localhost` on the hub machine or the owner's HTTPS remote address. The Windows app uses native toasts while running. |
| **iPhone notifications** | **Unchanged and limited:** local notifications while the app is alive. Real iPhone push needs an Apple push key held on a server, which is a cloud env plus the paid developer programme. Without it, an iPhone PWA over the owner's HTTPS address can receive Web Push. This is the strongest argument for a small cloud service later. |

## 7. Owner questions

1. **Add the Drive app-data scope to Google sign-in?** Users see one more consent line.
   *Recommend yes:* it is the only way to find the hub from anywhere with no server of ours.
2. **Remote access in the first build:** bring-your-own address (tunnel or Tailscale)
   published automatically, or wait for built-in peer-to-peer? *Recommend bring-your-own now;*
   your tunnel keeps working. Decide on built-in after a spike.
3. **May built-in remote access use a third party's relay servers** (encrypted traffic, but
   their terms and uptime)? The alternative is running our own, which is a cloud env.
   *Recommend yes in principle, decide after the spike.*
4. **iPhone-first sign-in:** wait for a computer (recommended), or refuse sign-in until one
   exists?
5. **Hub backups in your Google Drive** (encrypted, for recovery when the hub dies), or local
   files only? *Recommend local files first;* Drive backups need a recovery key the user must
   keep, which is its own design.
6. **Windows app as Tauri?** *Recommend yes.*
7. **Which of your machines is the hub?** *Recommend the PC* (always on, already hosts the VM).
8. **Console work only you can do:** add the scope, enable the Drive API, create a Desktop
   OAuth client (Windows app and CLI) and a "TVs and limited input" client (headless CLI), add
   `http://localhost:47600/oauth/google/return` to the web client, host the two static pages.

## 8. Build plan

Packages own their files; where two touch one file, the later one waits. "CI" means the
existing `rust.yml` unless stated.

| # | Package | Owns | Verify | Needs |
|---|---|---|---|---|
| 1 | **Embeddable hub and local claim.** Move serving out of `main.rs` into a `host` module (`start(data, binds) → handle`); move `backup` into the library; `claim_local(identity)`; local mode turns off HTTP claim and limits token sign-in to loopback. | `crates/hub/src/host.rs` (new), `main.rs`, `lib.rs`, `backup.rs`, `google_auth.rs`, `session.rs` | `cargo test --workspace`; `scripts/e2e.*` unchanged; new tests: local claim, HTTP claim refused in local mode, token sign-in refused off loopback | — |
| 2 | **Web UI sign-in for local mode.** Google-only screen, localhost return, no setup-code or token form, hub-offline and "this device is your hub" states. | `crates/hub/web/views/login.js`, `app.js` (routes), `css/brand.css` | Local hub + headless browser as in earlier W packages; `sw.test.mjs`; Google button needs the localhost redirect registered | Q8 for a real sign-in |
| 3 | **Hub identity, LAN TLS listener, mDNS.** Identity key and certificate, second listener, `_grouper._tcp`, `/v1/hub` returns `key_sha256` and `epoch`; pinned-certificate support in Rust clients. | `crates/hub/src/identity.rs`, `lan.rs` (new), `server.rs` (`hub_info` only), `crates/core-protocol` (`HubInfo`), `crates/core-client/src/lib.rs`, `crates/core-sync/src/client.rs` | Integration test: pinned client connects, wrong pin fails; mDNS seen by a test browser on loopback | After 1 |
| 4 | **Rendezvous.** Record schema, Drive app-data client (base URL injectable), resolver (cache → mDNS → LAN → remote), hub-side publisher, epoch rules. | `crates/core-rendezvous` (new crate), `crates/hub/src/rendezvous.rs` (new) | Unit tests against a fake Drive server: tie-break, epoch step-down, stale record; one manual run against real Drive | Q1, Q8 for the manual run |
| 5 | **CLI.** `grouper login` with Google (loopback, `--device`), `grouper hub init/run/status`, systemd unit. | `crates/cli/src/main.rs`, `credentials.rs`, `crates/cli/src/google.rs` (new) | Tests with a fake Google and fake Drive; manual on Windows and Linux | After 3, 4; Q8 |
| 6 | **Windows app.** Tauri shell: hub host + node in process, tray, autostart, Google sign-in, session hand-off to the webview, toasts, installer. | `apps/desktop/**` (new), `.github/workflows/desktop.yml` (new) | CI build on `windows-latest`; **real Windows PC** for sign-in, tray, firewall prompt, toasts | After 1, 2; 4 for joining |
| 7 | **Mac app as hub or device.** `HubHost` and resolver in ffi; sign-in screen with no URL or token; hub controller; Keychain master key. | `crates/ffi/src/lib.rs`, `scripts/build-apple.sh`, `apple/GrouperApp/macOS/**`, `Shared/SignInView.swift`, `Shared/AppModel.swift`, `apple/GrouperKit/Sources/GrouperKit/**` | **macOS CI** (`apple.yml`) to compile; **real Mac** for sign-in, start at login, firewall, sleep | After 1, 3, 4 |
| 8 | **iPhone as client.** Drive scope in the OAuth request, record reader and resolver in Swift (the iOS app has no Rust slice), `NWBrowser`, pinned `URLSession`, Info.plist keys, waiting-for-a-computer screen. | `apple/GrouperKit/Sources/GrouperAPI/Rendezvous.swift`, `HubResolver.swift` (new), `GoogleOAuth.swift`, `HubClient.swift`, `apple/GrouperApp/iOS/**` | Swift unit tests with fixtures shared with package 4; **macOS CI**; **real iPhone** for the Local Network prompt and pinning to a LAN address (App Transport Security behaviour is unverified) | After 4; shares 7's sign-in screen |
| 9 | **Moving the hub.** Maintenance mode, snapshot stream, restore on target, epoch bump, step-down; UI in web Devices and Apple Devices. | `crates/hub/src/migrate.rs` (new), `crates/hub/web/views/devices.js`, `apple/GrouperApp/Shared/DevicesView.swift` | Integration test: two hubs in one test, move, old one steps down, device token still works | After 3, 4 |
| 10 | **Docs and deployment.** README, `deploy/` marked server mode, `accounts-design.md` pointers, static return and privacy pages, Oscar's VM → PC move. | `README.md`, `deploy/**`, `docs/accounts-design.md`, `docs/decisions-needed.md` | Read-through; `curl` the static pages | Any time; the move after 6 |
| 11 | **Built-in remote access (spike only).** Measure a peer-to-peer library on Windows, Mac and an iOS slice; report relay terms, size, battery. | `spikes/remote/**` (new) | A written result | Q3 |

Packages 1 and 2 can start now and in parallel; 3 and 4 are independent of each other; 5 to 9
fan out after them.

### Not verified in this design

- Whether iOS accepts a pinned self-made certificate on a LAN address without extra App
  Transport Security exceptions.
- Google Console behaviour when a scope is added to a published app (section 2).
- That a second Google request with `login_hint` is a single tap inside
  `ASWebAuthenticationSession`.
- The current state, terms and relay limits of any peer-to-peer library (search for this was
  unavailable while writing); iroh is named from general knowledge only.
- That WebView2 inside Tauri cannot receive Web Push (reported by Tauri users, not tested).
