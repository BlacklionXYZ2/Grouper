# Running the hub

The hub runs in a Debian VM on the Windows PC (Hyper-V) and is reached at
`https://grouper.clockmillbarn.uk` through a Cloudflare Tunnel. The tunnel connects
out from the VM, so no router ports are opened and the VM's changing NAT address
doesn't matter. Later the same compose file runs on a VPS.

## 1. The VM

Needs Hyper-V enabled, the hypervisor running (`bcdedit /set hypervisorlaunchtype auto`,
admin, then restart) and your account in "Hyper-V Administrators".

```powershell
pip install pycdlib
deploy\hyperv\new-hub-vm.ps1
```

This creates `grouper-hub` (Debian 13, 40 GB disk, 4 CPUs, 4–8 GB RAM, starts with
Windows) and prints its IP. Log in with:

```powershell
$ip = (Get-VMNetworkAdapter -VMName grouper-hub).IPAddresses | Where-Object { $_ -match '^\d+\.\d+\.\d+\.\d+$' } | Select-Object -First 1
ssh -i $HOME\.ssh\grouper_hub_ed25519 grouper@$ip
```

## 2. The hub

From the repo root on the PC, copy the source in and build:

```bash
ip=<vm ip>
git ls-files -co --exclude-standard | grep -vE '^apple/|^data/' | tar -czf - -T - \
  | ssh -i ~/.ssh/grouper_hub_ed25519 grouper@$ip 'mkdir -p /opt/grouper/src && cd /opt/grouper/src && tar -xzf -'
ssh -i ~/.ssh/grouper_hub_ed25519 grouper@$ip 'cd /opt/grouper/src && docker compose -f deploy/compose.yaml up -d --build'
```

The hub listens on port 8080 in the VM. Its database lives in the `grouper-data`
Docker volume, so rebuilding keeps it.

## 3. The tunnel (`grouper.clockmillbarn.uk`)

Full step-by-step guide: [cloudflare-tunnel.md](cloudflare-tunnel.md).

In the Cloudflare dashboard (done once, by you):

1. **Zero Trust → Networks → Tunnels → Create a tunnel**, type **Cloudflared**, name it `grouper-hub`.
2. On the install page, copy the **token** (the long string after `--token`). Save it on
   the PC as `%USERPROFILE%\.grouper\cloudflared-token.txt`. Don't paste it into chats.
3. **Public hostname:** subdomain `grouper`, domain `clockmillbarn.uk`, service
   **HTTP** → `localhost:8080`. WebSockets need no extra setting.

Then install the tunnel as a service in the VM (removes the token file afterwards):

```bash
ssh -i ~/.ssh/grouper_hub_ed25519 grouper@$ip 'sudo cloudflared service install "$(cat)"' \
  < ~/.grouper/cloudflared-token.txt && rm ~/.grouper/cloudflared-token.txt
curl https://grouper.clockmillbarn.uk/healthz   # prints "ok"
```

Clients then use `--hub https://grouper.clockmillbarn.uk` (or `GROUPER_HUB`).

## 4. Models: local Gemma on the PC, through the PC's own connection

The online hub is set up with `gemma-local` (Gemma 4 12B in LM Studio on the PC's GPU) as
its default model for every role. The provider is hosted by the device "Gaming PC": the hub
sends model calls over that device's outbound connection and the PC calls LM Studio on
`127.0.0.1`, so LM Studio is never exposed to the network. To use it, on the PC:

```powershell
lms server start --port 45123
```

```powershell
lms load "hauhaucs/gemma-4-12b-qat-uncensored-hauhaucs-balanced-q4_k_m-gguf/gemma-4-12b-qat-uncensored-hauhaucs-balanced-q4_k_m.gguf" --identifier grouper-gemma --context-length 32768 --gpu max -y
```

```powershell
grouper connect --hub https://grouper.clockmillbarn.uk
```

(Port 45123 because Hyper-V reserves 1228–1327, including LM Studio's default 1234.)
While the PC or LM Studio is off, chats say the model's device is offline. To add a hosted
model alongside it: `grouper keys set <provider>` then `grouper models add …`.

## 5. Backups and moving to a VPS

The hub can keep running while you take a backup. `grouper-hub backup` uses SQLite's
online backup API, so `grouper.db` is a consistent snapshot, and copies `<data>/workspaces`.
The archive or directory also gets a `manifest.json` (hub id, versions, schema, file sizes
and SHA-256). **The master key is not in the backup.** Provider keys are ciphertext
without it. Copy the key itself every time you copy a backup, and store it somewhere
other than the data volume.

On the VM, from the repo root (`/opt/grouper/src`):

```bash
docker compose -f deploy/compose.yaml exec hub \
  grouper-hub backup /data/backups/grouper-$(date -u +%Y%m%dT%H%M%SZ).tar.gz
```

That writes inside the `grouper-data` volume. Copy the file off the VM (a backup that
only lives on the same disk is not a backup):

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

To write straight onto a host directory instead of the volume, add a bind mount under
`hub.volumes` (`./backups:/backups`) and back up to `/backups/...`. Skip thread
workspaces with `--include-workspaces=false` when you only need the database.

Copy the master key from the `grouper-secrets` volume (the file the compose file points
at with `GROUPER_MASTER_KEY_FILE`). This is the key that decrypts provider keys. Don't
commit it, don't put it in the backup tarball, and don't paste it into a chat:

```bash
docker compose -f deploy/compose.yaml exec -T hub cat /secrets/master.key > "$HOME/grouper-master.key"
chmod 600 "$HOME/grouper-master.key"
```

`scp` that file off the VM the same way, then `shred` or delete the VM copy if you don't
want it to stay there. The hub prints the same reminder after every backup.

### On a VPS

Use the same `deploy/compose.yaml`. Put the saved master key into the secrets volume
**before** the hub starts, or the hub will generate a new key and the restored provider
keys will not decrypt.

```bash
docker compose -f deploy/compose.yaml create
docker compose -f deploy/compose.yaml run --rm --no-deps -T --user root --entrypoint sh hub \
  -c 'umask 077; cat > /secrets/master.key; chown grouper:grouper /secrets/master.key' \
  < "$HOME/grouper-master.key"
```

Restore into the empty data volume (stop the hub first if it has already created a
database). `--force` moves an existing data directory's files to
`<data>/pre-restore-<timestamp>` instead of deleting them. It leaves `master.key` where
it is when that file lives inside the data directory.

```bash
docker compose -f deploy/compose.yaml stop hub
docker compose -f deploy/compose.yaml run --rm --no-deps \
  -v "$HOME/grouper-backups/grouper-YYYYMMDDTHHMMSSZ.tar.gz":/backup/grouper.tar.gz:ro \
  hub restore /backup/grouper.tar.gz
docker compose -f deploy/compose.yaml up -d
```

Restore checks every SHA-256 and checks that the master key this process can see
(`GROUPER_MASTER_KEY_FILE=/secrets/master.key` in the compose file) decrypts the
provider keys. If either check fails, the data volume is left as it was.

The restored database keeps its `hub_id`, so device caches still recognise the hub.
Point the Cloudflare Tunnel at the new host: install `cloudflared` on the VPS and run a
connector for the same `grouper-hub` tunnel (see [cloudflare-tunnel.md](cloudflare-tunnel.md)).
The public hostname stays `grouper` / `clockmillbarn.uk`, service HTTP `localhost:8080`.
Stop the connector on the old VM so Cloudflare doesn't split traffic between the two
machines.

### Nightly backups

Not enabled by default. A cron entry on the VM or VPS is enough. In crontab, escape `%`
as `\%`:

```cron
15 3 * * * docker compose -f /opt/grouper/src/deploy/compose.yaml exec -T hub grouper-hub backup /data/backups/grouper-$(date -u +\%Y\%m\%d).tar.gz
```

Copy new archives off the machine (and keep the master key with them, stored separately).
Delete old archives yourself. Overwriting one `nightly.tar.gz` only keeps a single copy.

A one-shot compose service does the same job if you'd rather not use cron. Don't add it
to the running file unless you want it; nothing in this repo starts it:

```yaml
# docker compose -f deploy/compose.yaml --profile backup run --rm backup
services:
  backup:
    image: grouper-hub:dev
    profiles: ["backup"]
    entrypoint: ["grouper-hub", "backup", "/data/backups/nightly.tar.gz"]
    volumes:
      - grouper-data:/data
      - grouper-secrets:/secrets
    environment:
      GROUPER_MASTER_KEY_FILE: /secrets/master.key
```
