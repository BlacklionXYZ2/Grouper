# Remote access: how other products reach a home server

Research for the owner's question (28 November 2026): how do Claude, ChatGPT and similar apps
let a phone reach a computer at home, and do they hand out tunnels? Sources are linked; points
not checked against a primary source are marked.

## Claude, ChatGPT

- They don't hand out tunnels. The service runs in the vendor's cloud and the apps talk to it.
  (Common knowledge; not checked against a vendor page.)
- Closest to Grouper is **Claude Code Remote Control**: the local process dials out over HTTPS
  to Anthropic and registers a session; the phone or browser talks to Anthropic, which relays
  messages down that connection. Nothing listens for incoming connections at home. (Third-party
  write-ups: <https://dev.to/chwu1946/deep-dive-how-claude-code-remote-control-actually-works-50p6>,
  <https://www.datacamp.com/tutorial/claude-code-remote-control>.)
- Claude Desktop's local MCP servers run on the user's machine:
  <https://support.claude.com/en/articles/10949351-getting-started-with-local-mcp-servers-on-claude-desktop>.

## Products where your own computer is the server

| Product | Finding the server | Getting there | Vendor runs |
|---|---|---|---|
| Plex | plex.tv account directory | Direct when possible, else a throttled relay; each server gets a `*.plex.direct` name and certificate from plex.tv ([forum](https://forums.plex.tv/t/documentation-around-the-warn-cert-tls-errors-plex-direct-sni-dns-overlay-etc/801991)) | Directory, DNS, certificates, relays |
| Home Assistant Cloud (Nabu Casa) | A fixed remote address per subscription | The home server connects out to the cloud ([HA](https://www.home-assistant.io/cloud/)) | Relay; $6.50/month ([pricing](https://www.nabucasa.com/pricing/)) |
| Synology QuickConnect | QuickConnect ID | Direct, then hole-punching, then a relay ([Synology](https://kb.synology.com/sv-se/DSM/help/DSM/AdminCenter/connection_quickconnect)) | Coordination servers and relays |
| Tailscale | Coordination server | Direct WireGuard, DERP relays as fallback ([blog](https://tailscale.com/blog/how-nat-traversal-works)) | Every phone needs the Tailscale app |
| Cloudflare Tunnel per customer | You create a hostname per customer | Outbound `cloudflared`; tunnels created through the API ([Tunnels for Platforms](https://developers.cloudflare.com/tunnel/features/tunnels-for-platforms/)) | Your Cloudflare account and domain carry every user |
| Jellyfin | None | The user sets up a reverse proxy or Tailscale ([docs](https://jellyfin.org/docs/general/post-install/networking/tailscale/)) | Nothing |

## What fits Grouper

- **A. A small Grouper relay (the Remote Control model).** The hub dials out to a relay;
  phones and browsers sign in to the relay with Google and are joined only to their own
  account's hub. Users set up nothing. We run the relay and pay for its traffic. It is also the
  only place iPhone push can come from (APNs needs a server). Best end state.
- **B. Per-user Cloudflare tunnels made through the API.** Quick to build, but every user's
  traffic and limits sit in one Cloudflare account, and every PC needs `cloudflared`.
- **C. Tailscale.** Free to run, but each phone needs another app. Power users only.

iPhone apps can't keep a background connection, so the phone reconnects when opened and relies
on push for "task finished".
