# Local-first review, 27 November 2026

Read-only review of `harness` at `88eae2b` against `docs/local-first-design.md` (owner answers
first) and the owner's words. I read the code; I ran no builds or tests. Nothing here has been
run on a real PC yet: the changelog says so for the tunnel, the Windows app and both Apple apps.

## 1. Owner requirements

| Requirement | Verdict | Evidence |
|---|---|---|
| Sign in only with Google, any platform | **Partly** | Web: Google only; token form only at unlinked `#/token-signin`, loopback-only in local mode (`session.rs:445`). Apple: one Google button; recovery token in the ⋯ menu (`SignInView.swift:79`). CLI: `grouper login` with no arguments. **Windows: not yet.** `DEFAULT_DESKTOP_CLIENT_ID` is empty (`oauth.rs:32`), so the Google button is hidden and the first-run screen points at `#/token-signin` (`ui/index.html:253`). |
| Device ids internal, linked to the one account | **Holds** | `devices.account_sub`, backfilled from the owner (`db.rs:280-284`); a second Google account is refused (`google_auth.rs:791`); tokens are never typed. |
| The hub is a device id | **Partly** | `hub_state(hub_device_id, epoch)`, `set_hub_device_id` raises `epoch` (`db.rs:287, 433`); `/v1/hub` reports both (`server.rs:1540`). But the hub's id is a new, extra device row made at first start (`db.rs:385`), not the app's own row, so one PC shows as up to three devices (drift 2). |
| Everything through my domain by default, changeable in settings | **Holds** | Default `https://grouper.clockmillbarn.uk` in `HubAddress.swift`, `cli/credentials.rs`, `desktop/settings.rs:17`, `tunnel.rs:47`. Changeable: Apple Settings → Hub address, web Settings → Remote access, `--hub` / `GROUPER_HUB`, Windows tray "Where Grouper runs…". |
| The user can pick which device is the hub | **Partly** | Picker, confirmation, progress and fresh Google sign-in are built on web and Apple (`devices.js`, `DevicesView.swift`, `migrate.rs:550`). **No app can receive a move:** `execute_target_move` (`migrate.rs:1537`) is only called from tests, and the Windows node ignores the offer (`node.rs:125` has no `hub_move_offer`). Mac hub (`HubHost` in ffi) and `grouper hub` commands are not built, so the PC is the only possible hub. |
| Local-first, hub on his own PC | **Partly** | `host::start` in local mode with `claim_local`, tunnel supervisor and step-down loop start with it (`host.rs:58-117`); the Tauri app embeds it on `127.0.0.1:47600` (`main.rs:215`). Unproven on a real PC, and blocked on the steps in section 3. |

## 2. Drift and risks

1. **The owner's email is public.** `GET /v1/hub` needs no token and returns `account` = the owner's
   Google email (`server.rs:1549`), at a public domain. Step-down compares hubs by that email
   (`stepdown.rs:122`). The design said "account id", not an address anyone can read. Use a hash of `sub`.
2. **One PC lists as several devices.** The node uses install id `<id>-node` (`settings.rs:121`) and the
   hub row has none, while the list groups by exact `install_id` (`devices.rs:354`). The picker will show
   the hub PC's own node as a separate, selectable target. The design says "installs, not rows".
3. **Windows first run can dead-end.** "Continue" with This computer starts an unclaimed local hub and
   opens its web UI, but claiming over HTTP is off in local mode (`google_auth.rs:1021`). Without the
   Desktop client there is no way in except a token made with `grouper-hub device add`.
4. **Signing in stops the hub.** The Google return listens on port 47600, the hub's port, so the app
   stops the hub (and its tunnel) for every Windows Google sign-in (`main.rs:145-151`). Use a random
   loopback port; Desktop clients don't need a fixed one.
5. **The master key is a plain file** in the data folder (`secrets.rs:39`). The design says DPAPI on
   Windows. The desktop app already has DPAPI helpers for its own tokens (`settings.rs:305`).
6. **Restore keeps the old hub's identity.** `backup::restore` never touches `hub_state`. A PC restored
   from the VM answers with the VM's `hub_device_id` and `epoch`, so step-down sees "same device" and keeps
   serving on both. Two connectors on one tunnel split traffic. Until fixed, the VM must be stopped by hand
   before the PC's tunnel starts. The design wanted `epoch + 1` on restore.
7. **Tunnel can start with no owner.** `tunnel.rs` has no owner check; the design starts the tunnel only
   once claimed. Low risk (HTTP claim is off) but not as designed.
8. **A stepped-down hub keeps serving on localhost.** Step-down stops `cloudflared` only
   (`stepdown.rs:199`); the desktop app never reads `is_stepped_down`, so its window would keep showing
   the stale copy. Matters once a second hub-capable device exists.
9. **Cross-machine move confirmation looks unfinished.** The target confirms against its own loopback
   unless `confirm_address` is set (`migrate.rs:1776`); the old hub checks the reported target URL or a
   *configured* public address, which is unset by default (`migrate.rs:1134`). Tests run both hubs in one
   process, so this has never crossed a network. Not verified by me beyond reading.
10. **Windows joining a remote hub registers as "cli"** (`oauth.rs:343`), though the hub now accepts
    `windows`; and its session hand-off uses `POST /v1/session`, which a local-mode hub refuses over the
    tunnel (`session.rs:445`). A second PC would land on the sign-in page.
11. **Missing from the Windows package:** autostart, single instance (a second launch fails on the port
    and data lock), native toasts, installer (CI uploads a bare `.exe` from `cargo build`,
    `desktop.yml:62-76`), the "Restore from a backup" choice, the laptop warning. Whether that bare exe
    shows the first-run page without `tauri build` is unchecked.
12. Still open from the design: every install asks the owner's hub first (Q1). Fine while he is the only user.

## 3. Shortest path to the hub on the owner's PC

### Only Oscar can do these

1. **Google Cloud, project `grouper-515912`:** Credentials → Create OAuth client ID → type **Desktop app**.
   Copy the client ID *and* the client secret (Google's token endpoint normally asks desktop clients for
   it; the app reads `GROUPER_GOOGLE_DESKTOP_CLIENT_SECRET`). Hand the ID to an agent; keep the secret in a
   file, not a chat.
2. **Redirect URIs:** none to register for the Desktop client (loopback is allowed by type; that
   `http://localhost:47600/oauth/google/return` with a path is accepted is untested here). The web client
   needs no change: browser sign-in still returns through `https://grouper.clockmillbarn.uk/oauth/google/return`,
   which the PC hub serves once the tunnel is on it. Also still open: point the consent screen's privacy
   link at `/privacy`.
3. **Back up the VM hub** (`deploy/README.md`, "backup"): run `grouper-hub backup` in the container, copy
   the archive to the PC, and copy the VM's `master.key` separately.
4. **Stop the VM for good before the PC serves:** in the VM, `sudo cloudflared service uninstall` and
   `docker compose -f deploy/compose.yaml down`. Do this before step 7 (drift 6).
5. **Restore on the PC:** build `grouper-hub.exe` (`cargo build -p hub --release`), create
   `%LOCALAPPDATA%\Grouper\hub`, put `master.key` in it first, then
   `grouper-hub --data %LOCALAPPDATA%\Grouper\hub restore <archive>`. The app uses that folder (`main.rs:244`).
6. **Install cloudflared on the PC:** `winget install Cloudflare.cloudflared`. Do not install it as a
   Windows service; the hub starts and stops it.
7. **Cloudflare dashboard, tunnel `grouper-hub` → Public Hostname:** change the service from
   `localhost:8080` to `http://127.0.0.1:47600` (the hub listens on IPv4 only). Copy the tunnel token.
8. **In the Windows app:** choose This computer → Sign in with Google (this claims the hub, or matches
   the existing owner). Then Settings → Remote access: paste the tunnel token, switch it on.
9. **Check:** `https://grouper.clockmillbarn.uk/healthz` answers from the PC; the phone signs in with Google.
   Phones signed in with old tokens keep working (rows came with the backup).

### Code still missing for that path

- The Desktop client ID in `DEFAULT_DESKTOP_CLIENT_ID` (`oauth.rs:32`) **and** in `DEFAULT_CLIENT_IDS`
  (`google_auth.rs:49`); otherwise `claim_local` rejects the token's audience. Stop-gap without a rebuild
  of defaults: env `GROUPER_GOOGLE_DESKTOP_CLIENT_ID` plus `GROUPER_GOOGLE_CLIENT_IDS` listing all four ids
  (that variable replaces the default list, `google_auth.rs:319`).
- A way to pass the client secret that isn't a plain field in `desktop.json`.
- A first real run of the app on Windows (window, tray, sign-in, session hand-off, real `cloudflared`).
- Not blocking but needed the same day: single instance and autostart, or the hub is down after a reboot.

## 4. Next packages, in order

| # | Package | Owns |
|---|---|---|
| A | **Desktop client wiring.** Client ids in both constants; secret from DPAPI-protected setting; Google return on a random loopback port so the hub keeps running; send `client: "windows"`; hide "Continue" for This computer until claimed. | `apps/desktop/src-tauri/src/{oauth.rs,main.rs,settings.rs}`, `apps/desktop/ui/index.html`, `google_auth.rs` (`DEFAULT_CLIENT_IDS` only) |
| B | **PC move runbook and restore identity.** Restore reassigns `hub_device_id` to this machine at `epoch + 1`; first-run "Restore from a backup"; rewrite the tunnel doc for the PC; mark `deploy/` server mode. | `crates/hub/src/backup.rs`, `apps/desktop/ui/index.html` (restore choice, after A), `deploy/**`, `README.md` |
| C | **Windows app basics.** Single instance, autostart, toasts for approvals, `tauri build` installer in CI. | `apps/desktop/src-tauri/{Cargo.toml,tauri.conf.json,capabilities/**}`, new `src/{autostart,toasts}.rs`, `.github/workflows/desktop.yml` |
| D | **Hub identity cleanup.** `/v1/hub` returns an account hash, not the email; step-down and Swift/JS readers follow; tunnel waits for an owner. | `server.rs` (`hub_info`), `stepdown.rs`, `tunnel.rs`, `crates/core-protocol`, `HubMove.swift`, `devices.js` (identity compare only) |
| E | **One install, one device.** Hub row adopts the app's install id; node and admin rows group together; the hub's own install is never a target. | `crates/hub/src/{db.rs (ensure_hub_device), devices.rs}`, `apps/desktop/src-tauri/src/node.rs` |
| F | **Master key in DPAPI / Keychain.** | `crates/hub/src/secrets.rs`, `migrate.rs` (key read/write only) |
| G | **Windows as a move target and stepped-down client.** Node handles offer/commit, calls `execute_target_move` with the public confirm address; app switches to client mode on step-down; old hub confirms via the default public address. | `apps/desktop/src-tauri/src/{node.rs,main.rs}`, `migrate.rs` (confirm addresses) |
| H | **Mac as hub** (design package 11), then **`grouper hub` commands** (12). Only after G proves a real two-machine move. | as in the design |

A and B are all the owner needs to run the hub on his PC. C makes it stay up. D to F fix what the
build got wrong. G and H are what make "pick which device is the hub" real.
