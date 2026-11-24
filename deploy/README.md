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
