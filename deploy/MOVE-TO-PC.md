# Move the hub from the VM to this PC

The hub to run is the Windows app on this PC. [README.md](README.md) is **server mode**:
the Docker hub on the Debian VM, or later a VPS. It stays as documentation.

These steps are the 27 November 2026 review, section 3, in that order. The
`grouper-hub` commands were checked against `grouper-hub --help`,
`grouper-hub backup --help` and `grouper-hub restore --help`. Backup takes an
output path. Restore takes the archive path. The only restore option is
`--force`. `--data` is the global data-folder option (default `./data`, or
`GROUPER_DATA`).

Do not paste the OAuth secret, the tunnel token or `master.key` into a chat.

## 1. Desktop OAuth client

Google Cloud, project `grouper-515912`: **Credentials → Create OAuth client ID →
Desktop app**. Copy the client ID and the client secret. The app reads the
secret from `GROUPER_GOOGLE_DESKTOP_CLIENT_SECRET`. Hand the ID to an agent.
Keep the secret in a file, not a chat.

## 2. Redirect URIs

Register none for the Desktop client. Loopback is allowed by that client type.
Whether `http://localhost:47600/oauth/google/return` (with a path) is accepted
is untested. The web client needs no change: browser sign-in still returns
through `https://grouper.clockmillbarn.uk/oauth/google/return`, which the PC
hub serves once the tunnel points at it. Still open: point the consent screen's
privacy link at `/privacy`.

## 3. Back up the VM hub

On the VM, from the repo root (`/opt/grouper/src`). The image sets
`GROUPER_DATA=/data`, so `backup` writes a snapshot of that folder. The output
path is required. `--include-workspaces` defaults to `true`, so leaving it out
copies `<data>/workspaces`. Pass `--include-workspaces=false` to skip them.

```bash
docker compose -f deploy/compose.yaml exec hub \
  grouper-hub backup /data/backups/grouper-$(date -u +%Y%m%dT%H%M%SZ).tar.gz
```

Copy the archive off the VM. A backup that only lives on the same disk is not
a backup.

```bash
mkdir -p "$HOME/grouper-backups"
docker compose -f deploy/compose.yaml cp \
  hub:/data/backups/grouper-YYYYMMDDTHHMMSSZ.tar.gz \
  "$HOME/grouper-backups/"
```

From the PC:

```powershell
scp -i $HOME\.ssh\grouper_hub_ed25519 grouper@<vm ip>:grouper-backups/grouper-YYYYMMDDTHHMMSSZ.tar.gz .
```

Copy `master.key` separately. It is not in the archive. The compose file sets
`GROUPER_MASTER_KEY_FILE=/secrets/master.key`. Do not commit it, do not put it
in the tarball, and do not paste it into a chat.

```bash
docker compose -f deploy/compose.yaml exec -T hub cat /secrets/master.key > "$HOME/grouper-master.key"
chmod 600 "$HOME/grouper-master.key"
```

`scp` that file off the VM the same way, then delete the VM copy if it should
not stay there.

## 4. Stop the VM before the PC serves

In the VM, before step 7. Two connectors on the `grouper-hub` tunnel split
traffic.

```bash
sudo cloudflared service uninstall
docker compose -f deploy/compose.yaml down
```

## 5. Restore on the PC

From the repo root on the PC. `cargo build -p hub --release` writes
`target\release\grouper-hub.exe`. The Windows app uses
`%LOCALAPPDATA%\Grouper\hub` (`hub_data_dir` in
`apps/desktop/src-tauri/src/main.rs`). Put `master.key` in that folder first,
under that name. `restore` reads it from the data folder (or from
`GROUPER_MASTER_KEY_FILE`). A folder that contains only `master.key` is empty
enough: do not pass `--force`. `--force` is for a folder that already has hub
files; it moves those files aside and does not delete them.

```powershell
cargo build -p hub --release
New-Item -ItemType Directory -Force -Path "$env:LOCALAPPDATA\Grouper\hub"
Copy-Item "$HOME\grouper-master.key" "$env:LOCALAPPDATA\Grouper\hub\master.key"
.\target\release\grouper-hub.exe --data "$env:LOCALAPPDATA\Grouper\hub" restore .\grouper-YYYYMMDDTHHMMSSZ.tar.gz
```

This PC is not the VM, so restore registers this computer and sets
`hub_device_id` to that new device at `epoch + 1`. Device rows come across,
and existing device tokens still work. Restoring on the machine that already
owns the hub device in the backup leaves `hub_device_id` and `epoch` as the
snapshot recorded them. There is no flag for either case.

## 6. Install cloudflared on the PC

```powershell
winget install Cloudflare.cloudflared
```

Do not install it as a Windows service. The hub starts and stops it.

## 7. Point the tunnel at the PC

Cloudflare dashboard, tunnel `grouper-hub` → **Public Hostname**: change the
service from `localhost:8080` to `http://127.0.0.1:47600`. The desktop hub
listens on IPv4 only, port 47600. Copy the tunnel token. Do not paste it into
a chat.

## 8. Sign in on the Windows app

Choose **This computer**, then **Sign in with Google**. That claims the hub,
or matches the existing owner. Then **Settings → Remote access**: paste the
tunnel token and switch it on.

## 9. Check

`https://grouper.clockmillbarn.uk/healthz` answers from the PC. The phone
signs in with Google. Phones already signed in with old tokens keep working,
because those device rows came with the backup.

## Still required before step 8 works

Checked on this branch, not fixed here:

- `DEFAULT_DESKTOP_CLIENT_ID` in `apps/desktop/src-tauri/src/oauth.rs` is empty,
  and `DEFAULT_CLIENT_IDS` in `crates/hub/src/google_auth.rs` does not include
  a Desktop client. `claim_local` rejects a token whose audience is not
  trusted. Without a rebuild of those defaults, set
  `GROUPER_GOOGLE_DESKTOP_CLIENT_ID` and `GROUPER_GOOGLE_CLIENT_IDS`.
  `GROUPER_GOOGLE_CLIENT_IDS` replaces the default list, so it has to list
  every client the hub should trust, including the new Desktop id.
- The client secret must not be a plain field in `desktop.json`. The app
  reads `GROUPER_GOOGLE_DESKTOP_CLIENT_SECRET`.
- The Windows app has not had a first real run (window, tray, sign-in,
  session hand-off, real `cloudflared`).
- The same day, the app still needs a single instance and autostart, or the
  hub is down after a reboot.
- The first-run **Restore from a backup** choice is not in the Windows UI.
  Step 5 is the restore.
