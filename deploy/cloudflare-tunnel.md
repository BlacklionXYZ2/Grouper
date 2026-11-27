# Cloudflare Tunnel for the Grouper hub

**Server mode.** This guide installs `cloudflared` as a service in the VM and points
the public hostname at `localhost:8080`. The PC does not use that service. Once the
hub runs on the PC, follow [MOVE-TO-PC.md](MOVE-TO-PC.md): the hostname becomes
`http://127.0.0.1:47600`, and the hub starts and stops `cloudflared` itself.

Goal: `https://grouper.clockmillbarn.uk` reaches the Grouper hub, which runs in the
Hyper-V VM `grouper-hub` on Oscar's Windows PC. Everything here can be done by a person or
an agent with access to Cloudflare and (for step 3) a terminal on that PC.

## How it fits together

```
phone / laptop / browser ──HTTPS──▶ Cloudflare edge (grouper.clockmillbarn.uk)
                                        ▲
                                        │ outbound tunnel (no open ports)
                                        │
             Windows PC ─ Hyper-V VM "grouper-hub" (Debian 13)
                           ├─ cloudflared (system service)  ──▶ http://localhost:8080
                           └─ Docker: grouper-hub (listens on 0.0.0.0:8080)
```

- **No IP address and no port forwarding.** `cloudflared` in the VM connects out to
  Cloudflare. The VM's own address (172.30.x.x on Hyper-V's Default Switch) is private and
  changes on reboot; nothing should point at it.
- `cloudflared` is already installed in the VM (from Cloudflare's apt repository). The hub
  is already running in Docker there and answers on `localhost:8080`.

## 1. Create the tunnel (Cloudflare dashboard)

1. Open **Zero Trust → Networks → Tunnels → Create a tunnel**.
2. Connector type: **Cloudflared**. Name: `grouper-hub`. Save.
3. On the "Install and run a connector" page choose **Debian, 64-bit**. Don't run the
   commands it shows. Copy only the **token**: the long string after `--token` (it starts
   with `eyJ`).
4. **Save the token to a file, not a chat.** On the Windows PC, create
   `C:\Users\oscar\.grouper\cloudflared-token.txt` containing only the token on one line.
   It's a secret: anyone with it can run this tunnel. Don't paste it into any chat,
   ticket or log.

## 2. Public hostname (same tunnel, "Public Hostname" tab)

Add one route:

| Field | Value |
|---|---|
| Subdomain | `grouper` |
| Domain | `clockmillbarn.uk` |
| Path | (empty) |
| Service type | `HTTP` |
| URL | `localhost:8080` |

- Leave "Additional application settings" at their defaults.
- Cloudflare creates the DNS record itself: a proxied CNAME `grouper` →
  `<tunnel-id>.cfargotunnel.com`. **Don't add an A record or any other record for
  `grouper` by hand.** If one already exists, delete it first.
- Check that WebSockets are on for the zone: **clockmillbarn.uk → Network → WebSockets: On**
  (the default). Devices and the web app use WebSockets.
- **Don't put Cloudflare Access (Zero Trust login) in front of this hostname for now.**
  Grouper devices authenticate with their own tokens over a WebSocket and can't complete an
  Access login, so they would be locked out. The hub already refuses anything without a
  valid device or admin token, except `/healthz`, `/v1/hub` (version and a random hub id)
  and the web app's static files.
- SSL/TLS mode doesn't matter for a tunnel. "Always Use HTTPS" on is fine.

## 3. Start the connector in the VM

Either tell Claude the token file is saved (it will do this step), or run this in
PowerShell on the Windows PC as Oscar:

```powershell
$ip = (Get-VMNetworkAdapter -VMName grouper-hub).IPAddresses | Where-Object { $_ -match '^\d+\.\d+\.\d+\.\d+$' } | Select-Object -First 1
Get-Content "$HOME\.grouper\cloudflared-token.txt" -Raw | ssh -i "$HOME\.ssh\grouper_hub_ed25519" "grouper@$ip" 'read -r T; sudo cloudflared service install "$T"'
```

```powershell
Remove-Item "$HOME\.grouper\cloudflared-token.txt"
```

The token goes in through standard input, so it never appears on a command line, and the
file is deleted afterwards. `cloudflared` then runs as a system service in the VM and
starts with it; the VM starts with Windows.

## 4. Check it works

- Dashboard: the tunnel `grouper-hub` shows **Healthy** with one connector.
- `https://grouper.clockmillbarn.uk/healthz` returns `ok`.
- `https://grouper.clockmillbarn.uk/v1/hub` returns JSON with `hub_id`, `hub_version` and
  `protocol_version: 4`.
- `https://grouper.clockmillbarn.uk/` shows the Grouper web sign-in page.
- In the VM, if something's wrong: `sudo systemctl status cloudflared` and
  `sudo journalctl -u cloudflared -n 50`.

## 5. Afterwards

Clients use the domain instead of an IP:

```powershell
grouper login --hub https://grouper.clockmillbarn.uk
```

Devices and the CLI then connect from anywhere, and the web app is at
`https://grouper.clockmillbarn.uk/` (sign in with an admin token).

## Don't

- Open or forward ports on the router, or point DNS at the PC's or VM's IP.
- Expose the VM's SSH through the tunnel.
- Put the tunnel token anywhere but that one file, which is deleted after step 3.
- Change the VM, Docker or the hub; nothing there needs touching for the tunnel.
