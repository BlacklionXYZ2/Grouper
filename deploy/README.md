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
