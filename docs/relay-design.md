# Grouper relay: design

Status: v1 design, 28 November 2026. Replaces "a Cloudflare tunnel on the owner's domain" as the
way phones and browsers reach a hub away from home (research: `docs/remote-access-research.md`).

## The idea

The hub dials **out** to the relay over one WebSocket. Phones, Macs and browsers sign in to the
relay with Google; the relay joins each signed-in person only to **their own account's** hub and
passes HTTP requests and WebSockets down the hub's connection. Nobody sets up a domain, a tunnel
or a port. Same model as Claude Code Remote Control, Home Assistant Cloud and Synology
QuickConnect.

The relay is a **router, not an authority.** It decides which hub a request goes to. The hub
still checks every request itself (session cookie, device token, Google sign-in with its own
nonce, owner check), exactly as when it was reached through a tunnel. A relay that misroutes
reaches a hub that refuses the caller.

## Where it runs

A Cloudflare Worker with one Durable Object per account (`relay/` in this repo, TypeScript).
Free tier: Workers, SQLite-backed Durable Objects, WebSocket hibernation. No database outside the
per-account objects. Local development and tests use `wrangler dev` (workerd), nothing deployed.

Configuration (Worker vars / secrets):

| Name | Meaning |
|---|---|
| `RELAY_SECRET` (secret) | HMAC key for nonces, relay tokens and account keys. |
| `GOOGLE_CLIENT_IDS` | Comma-separated audiences accepted in ID tokens (iOS, Mac, web, desktop clients). |
| `GOOGLE_WEB_CLIENT_ID` | Client used by the relay's own browser sign-in page. |
| `GOOGLE_JWKS_URL` | Default `https://www.googleapis.com/oauth2/v3/certs`. Tests point it at a fake. |
| `GOOGLE_AUTH_URL` | Default `https://accounts.google.com/o/oauth2/v2/auth`. Tests point it at a fake. |
| `GOOGLE_ISSUERS` | Default `https://accounts.google.com,accounts.google.com`. |

## Identity

- **Account key:** `acct = hex(HMAC(RELAY_SECRET, "acct:" + google_sub))[0..32]`. The Durable
  Object for an account is `ACCOUNT.idFromName(acct)`. The relay never stores the raw `sub`
  outside that object.
- **Google ID token checks** (every place a token is accepted): RS256 signature against the JWKS
  (cached per `Cache-Control`, refetch on unknown `kid`), `iss` in `GOOGLE_ISSUERS`, `aud` in
  `GOOGLE_CLIENT_IDS`, `exp` in the future, `iat` within the last 10 minutes and not more than
  5 minutes ahead, `email_verified` true, and `nonce` a **relay nonce** (below) that has not been
  used. One-time use is recorded in the account's Durable Object.
- **Relay nonce:** `POST /relay/v1/nonce` returns `{nonce, expires}` where
  `nonce = "rn." + rand(16 bytes b64url) + "." + exp + "." + b64url(HMAC(RELAY_SECRET, rand + "." + exp))`,
  valid 10 minutes. Stateless until used; the account object keeps used nonces until they expire.
- **Relay token** (what a signed-in client presents):
  `"rt." + b64url(json {a: acct, e: email, k: "web"|"app", g: generation, x: expires}) + "." + b64url(HMAC)`.
  Web tokens live 30 days, app tokens 365 days. `g` must equal the account object's current
  generation; "sign out everywhere" bumps it.
- **Hub secret** (what a hub presents): `"hs." + acct + "." + rand(32 bytes b64url)`. The account
  object stores only its SHA-256. Claiming again replaces it (one hub per account; the old hub's
  socket is closed with code 4001 "replaced").

How a client presents its relay token:
- Browsers: cookie `grouper_relay` (HttpOnly, `SameSite=Lax`, `Path=/`, `Secure` when the relay
  is on HTTPS). Lax, not Strict, so the cookie is sent on the top-level return from Google.
- Apps and CLIs: header `Grouper-Relay: <token>` (HTTP and the WebSocket handshake).

The relay **removes** `grouper_relay` from the `Cookie` header and drops `Grouper-Relay` before
forwarding, so the hub never sees relay credentials.

## Relay HTTP API (all under `/relay/v1/`, JSON)

| Route | Auth | Does |
|---|---|---|
| `POST /relay/v1/nonce` | none (rate-limit later) | `{nonce, expires}` |
| `POST /relay/v1/sign-in` `{id_token, client: "web"\|"app"}` | ID token with relay nonce | web: sets the cookie, returns `{email, hub_connected}`; app: returns `{relay_token, email, hub_connected, expires}` |
| `POST /relay/v1/sign-out` `{everywhere?: bool}` | relay token | clears the cookie; `everywhere` bumps the generation |
| `GET /relay/v1/me` | relay token (optional) | `{signed_in, email?, hub_connected?, hub_id?}` |
| `POST /relay/v1/hubs/claim` `{hub_id, id_token}` | ID token with relay nonce | stores the hub id and a new hub secret for that account; returns `{hub_secret, account_email}` |
| `DELETE /relay/v1/hubs` | `Authorization: Bearer <hub secret>` | forgets the hub, closes its socket |
| `GET /relay/v1/hub/connect` (WebSocket) | `Authorization: Bearer <hub secret>`, `Grouper-Hub-Id: <hub_id>` | the hub's tunnel socket; 401 on a wrong secret or hub id |
| `GET /relay/auth/start` | none | browser sign-in: redirects to Google (`response_type=id_token`, `response_mode=form_post`, relay nonce, `redirect_uri=<relay>/relay/auth/return`) |
| `POST /relay/auth/return` | form `id_token` | verifies as sign-in `web`, sets the cookie, redirects to `/` |

Everything else (any path, including `/`, `/v1/...`, `/oauth/google/return`, the web app's
files): with a valid relay token, forwarded to that account's hub; without one, `/` and other
`GET`s that accept `text/html` get the relay's sign-in page, and anything else gets
`401 {"error": "sign in to the Grouper relay first", "relay": true}`. With a valid token but no
hub connected: `503 {"error": "your hub isn't connected to the relay", "relay": true}` (HTML
navigations get a short page saying the same).

## The tunnel protocol (hub socket)

One WebSocket per hub, text frames, one JSON object per frame. `id` is a relay-chosen integer,
unique per open socket. Bodies are base64 (standard alphabet) in chunks of at most 48 KiB raw.

Relay → hub:

```jsonc
{"t":"req","id":7,"method":"GET","path":"/v1/hub?x=1","headers":[["accept","application/json"]],"body":"<b64>"} // body omitted when empty; request bodies up to 16 MiB, sent whole
{"t":"cancel","id":7}                       // the client went away; stop and send nothing more for 7
{"t":"ws_open","id":9,"path":"/v1/events","headers":[["cookie","..."]]}
{"t":"ws_msg","id":9,"text":"..."}          // or "b64":"..." for binary
{"t":"ws_close","id":9,"code":1000,"reason":""}
{"t":"ping"}                                // answer {"t":"pong"}
```

Hub → relay:

```jsonc
{"t":"hello","hub_id":"…","version":"0.1.0"}   // first frame after connecting
{"t":"res","id":7,"status":200,"headers":[["content-type","application/json"],["set-cookie","a=1"],["set-cookie","b=2"]]}
{"t":"data","id":7,"b64":"…"}                 // zero or more, in order
{"t":"end","id":7}
{"t":"err","id":7,"message":"…"}              // instead of res (relay answers 502) or after res (relay aborts the body)
{"t":"ws_ok","id":9}  {"t":"ws_fail","id":9,"status":401,"message":"…"}
{"t":"ws_msg","id":9,"text":"…"}  {"t":"ws_close","id":9,"code":1000,"reason":""}
{"t":"ping"}  {"t":"pong"}
```

Header rules:
- Both sides drop hop-by-hop headers (`connection`, `keep-alive`, `transfer-encoding`, `upgrade`,
  `te`, `trailer`, `proxy-*`, `sec-websocket-*`, `content-length` — recomputed).
- The relay forwards the client's `host` (the relay's public host), `origin`, `cookie` (minus
  `grouper_relay`), `authorization`, and other end-to-end headers, and adds
  `x-forwarded-for: <client IP>` (Cloudflare's `CF-Connecting-IP`, else `0.0.0.0`) and
  `x-forwarded-proto: <relay scheme>`.
- The hub's connector **replaces** `x-forwarded-for` with the relay's value (never loopback; use
  `0.0.0.0` if missing), sets `cf-connecting-ip` to the same, sets `x-forwarded-proto`, and adds
  `x-grouper-relayed: 1`. So the hub's existing rules treat relayed requests as remote: no
  loopback-only token sign-in in local mode, no orchestrator MCP endpoint, rate limits per client
  IP.

Timeouts: the relay gives up on a request with no `res` after 100 s (504). Either side may ping;
each answers. The relay closes a hub socket that sends nothing for 90 s; the hub pings every 30 s.

## Hub side

- Settings in `hub_meta`: `relay_url`, `relay_enabled`, `relay_hub_secret` (sealed with the
  master key, like the tunnel token; never returned by any endpoint), `relay_account_email`.
- Admin endpoints:
  - `GET /v1/relay` → `{enabled, url, linked, status: "off"|"connecting"|"connected"|"failed", last_error, account_email}`
  - `PUT /v1/relay` `{url?, enabled?}`
  - `POST /v1/relay/nonce` → asks the relay for a nonce and returns it (the UI runs Google
    sign-in with it).
  - `POST /v1/relay/link` `{id_token}` → checks the token is the hub owner's (`sub` matches;
    signature checked, nonce not required here because the relay checks its own), calls
    `POST /relay/v1/hubs/claim`, stores the secret, enables and starts the connector.
  - `DELETE /v1/relay` → calls `DELETE /relay/v1/hubs`, clears the secret, stops.
- A supervisor (like the tunnel's) connects to `<relay_url>/relay/v1/hub/connect` when enabled
  and linked and not stepped down, reconnects with backoff (1 s doubling to 60 s), and serves
  frames by calling the hub's own loopback listener (`http://127.0.0.1:<port>`), with redirects
  off and the header rules above. WebSockets open a loopback WebSocket to the same path.
- Test hooks: `GROUPER_GOOGLE_JWKS_URL` and `GROUPER_GOOGLE_AUTH_URL` override Google's URLs (for
  fake-credential tests only).

## Browser flow

1. Open the relay address. No cookie → relay sign-in page → Google → `/relay/auth/return` →
   cookie set → `/`.
2. `/` now reaches the hub's web app. It asks the hub for a session; if none, the hub's own
   Google sign-in runs (nonce from the hub, `state` origin = the relay address, return page
   `/oauth/google/return` served by the hub through the relay). Two Google steps; the second is
   one click because Google remembers the account. (Joining them is later work.)

## Not in v1

Direct connections when on the same network (mDNS is planned separately), end-to-end encryption
so the relay sees only ciphertext, APNs push from the relay, per-IP rate limits, a relay page in
the hub's Settings UI (v1 links by API), more than one hub per account.
