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
| `RELAY_WEB_CLIENT_ID` (optional) | When set, browser sign-in (`/relay/auth/*`) uses this client and accepts **only** this audience, so the relay's page is not a way to turn tokens minted for the apps into relay sessions. Takes precedence over `GOOGLE_WEB_CLIENT_ID`. |
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
  5 minutes ahead, `email_verified` true, and `nonce` the **domain-separated hash of a relay
  nonce** (below) that has not been used. One-time use is recorded in the account's Durable Object.
- **Nonce domain separation.** A relay nonce is just a string anyone can obtain, and hubs run
  Google sign-in with their own nonces for the same audiences. If the relay accepted the raw relay
  nonce as the token's `nonce`, a malicious hub could hand a victim a relay nonce as its "hub
  nonce" and replay the resulting ID token to the relay. So the Google `nonce` claim the relay
  accepts is `b64url(SHA-256("grouper-relay-v1:" + relay_nonce))` (43 characters), never the relay
  nonce itself. Requests carry the raw `relay_nonce` next to `id_token`; the relay checks that
  nonce's HMAC, expiry and one-time use, and that the token's `nonce` equals its hash. Hub nonces
  are 32 hex characters, so a hash can never be produced by a client that sends a hub's nonce
  verbatim. **Apps must send a hub-issued nonce to Google verbatim only if it matches
  `/^[0-9a-f]{32}$/`, and must always hash relay nonces** (the hub-side nonce shape is what makes
  the two namespaces disjoint).
- **Relay nonce:** `POST /relay/v1/nonce` returns `{nonce, expires}` where
  `nonce = "rn." + rand(16 bytes b64url) + "." + exp + "." + b64url(HMAC(RELAY_SECRET, rand + "." + exp))`,
  valid 10 minutes. Stateless until used; the account object keeps used nonces until they expire.
- **Relay token** (what a signed-in client presents):
  `"rt." + b64url(json {a: acct, e: email, k: "web"|"app", g: generation, x: expires}) + "." + b64url(HMAC)`.
  Web tokens live 30 days, app tokens 365 days. `g` must equal the account object's current
  generation; "sign out everywhere" bumps it.
- **Hub secret** (what a hub presents): `"hs." + acct + "." + rand(32 bytes b64url) + "." + tag`,
  where `tag = b64url(HMAC(RELAY_SECRET, "hs:" + acct + "." + rand))` truncated to 16 bytes. The
  Worker verifies the tag (constant time) before touching any Durable Object, so forged secrets
  can't create or wake account objects. The account object stores only the secret's SHA-256 and
  still checks it. Claiming again replaces it (one hub per account; the old hub's
  socket is closed with code 4001 "replaced").

How a client presents its relay token:
- Browsers: cookie `grouper_relay` (HttpOnly, `SameSite=Lax`, `Path=/`, `Secure` when the relay
  is on HTTPS). Lax, not Strict, so the cookie is sent on top-level navigations arriving from
  other sites.
- Apps and CLIs: header `Grouper-Relay: <token>` (HTTP and the WebSocket handshake).

The relay **removes** `grouper_relay` from the `Cookie` header and drops `Grouper-Relay` before
forwarding, so the hub never sees relay credentials. In the other direction it drops any
`Set-Cookie` from a hub response that sets `grouper_relay` or `grouper_relay_state` (a hub can't
set, replace or clear the relay's cookies).

Cross-site protections on the JSON API: `POST /relay/v1/sign-in`, `/sign-out` and `/hubs/claim`
require `Content-Type: application/json` (otherwise 415, so a cross-site HTML form can't post to
them) and, when an `Origin` header is present, it must equal the relay's own origin (otherwise
403).

## Relay HTTP API (all under `/relay/v1/`, JSON)

| Route | Auth | Does |
|---|---|---|
| `POST /relay/v1/nonce` | none (rate-limit later) | `{nonce, expires}` |
| `POST /relay/v1/sign-in` `{id_token, relay_nonce, client: "web"\|"app"}` | ID token whose nonce is the hash of `relay_nonce` | web: sets the cookie, returns `{email, hub_connected}`; app: returns `{relay_token, email, hub_connected, expires}` |
| `POST /relay/v1/sign-out` `{everywhere?: bool}` | relay token | clears the cookie; `everywhere` bumps the generation, closes every open client WebSocket of the account with code 4401 and aborts its in-flight streamed responses |
| `GET /relay/v1/me` | relay token (optional) | `{signed_in, email?, hub_connected?, hub_id?}` |
| `POST /relay/v1/hubs/claim` `{hub_id, id_token, relay_nonce}` | ID token whose nonce is the hash of `relay_nonce` | stores the hub id and a new hub secret for that account; returns `{hub_secret, account_email}` |
| `DELETE /relay/v1/hubs` | `Authorization: Bearer <hub secret>` | forgets the hub, closes its socket |
| `GET /relay/v1/hub/connect` (WebSocket) | `Authorization: Bearer <hub secret>`, `Grouper-Hub-Id: <hub_id>` | the hub's tunnel socket; 401 on a wrong secret or hub id |
| `GET /relay/auth/start` | none | browser sign-in: sets the `grouper_relay_state` cookie and redirects to Google (`response_type=id_token`, `response_mode=form_post`, `nonce` = hash of a fresh relay nonce, `state`, `redirect_uri=<relay>/relay/auth/return`) |
| `POST /relay/auth/return` | form `id_token`, `state` + the state cookie | verifies as sign-in `web`, sets the cookie, clears the state cookie, redirects to `/` |

Browser sign-in is bound to the browser that started it (login CSRF): `/relay/auth/start` sets
`grouper_relay_state=<state>.<relay_nonce>` (HttpOnly, `Path=/relay/auth`, 10 minutes;
`SameSite=None; Secure` on HTTPS because Google's `form_post` return is a cross-site POST, `Lax`
on plain-HTTP development) and sends `state` to Google. `/relay/auth/return` requires the posted
`state` to equal the cookie's (constant time), takes the relay nonce **from the cookie** (never
from the form), then clears the cookie. A sign-in response replayed from another browser has no
matching cookie and is refused.

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
  - `POST /v1/relay/nonce` → asks the relay for a nonce and returns it (the UI hashes it as above and runs Google
    sign-in with it).
  - `POST /v1/relay/link` `{id_token, relay_nonce}` → checks the token is the hub owner's (`sub` matches;
    signature checked, nonce not required here because the relay checks its own), calls
    `POST /relay/v1/hubs/claim` with `{hub_id, id_token, relay_nonce}`, stores the secret, enables and starts the connector.
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

## Deployment layout: one host, three parts (owner decision, 28 November 2026)

Everything lives on `https://grouper.clockmillbarn.uk`, split by path, served by the one relay
Worker (a zone route `grouper.clockmillbarn.uk/*`, so the existing proxied DNS record stays):

| Path | Part | Served by |
|---|---|---|
| `/`, `/privacy`, `/download`, `/site/*` (and any other path the Worker doesn't claim below) | **Website** | The Worker's static assets (`relay/site/`), no sign-in |
| `/relay/*` | **Relay** | The Worker (API, sign-in page `/relay/sign-in`) |
| `/app`, `/app/*`, `/v1/*`, `/sw.js`, `/oauth/*`, `/favicon.ico`, `/healthz` | **Web app and hub API** | Forwarded to the signed-in account's hub |

- Hub paths without a relay session: browser navigations (`Accept: text/html`) redirect to
  `/relay/sign-in?next=<path>`; anything else gets the 401 JSON. After sign-in the relay
  redirects to `next` (same-origin paths only; default `/app`).
- **Google return page:** the relay's browser sign-in uses `redirect_uri =
  <origin>/oauth/google/return`, the URI already registered on the web client, so no Google
  console change is needed. A `POST /oauth/google/return` whose `state` matches the
  `grouper_relay_state` cookie is the relay's own sign-in; any other request to that path is
  forwarded to the hub as before (the hub's web sign-in uses the same return URI).
- Web client: `GOOGLE_WEB_CLIENT_ID` = the hub's `DEFAULT_WEB_CLIENT_ID`. `GOOGLE_CLIENT_IDS` =
  the hub's `DEFAULT_CLIENT_IDS` (iOS, Mac, web, Windows desktop).
- The hub's service worker is registered with scope `/app/`, not `/`, so a hub never controls
  the website or relay pages.

## Clients (apps, CLI, Windows app)

Default address stays `https://grouper.clockmillbarn.uk`. A client signs in twice, relay first:

1. `POST /relay/v1/nonce` → `{nonce}`.
2. Google sign-in with `nonce = b64url_nopad(SHA-256("grouper-relay-v1:" + nonce))`.
3. `POST /relay/v1/sign-in` `{id_token, relay_nonce: nonce, client: "app"}` → `{relay_token,
   expires}`; store it with the hub token (Keychain / DPAPI / credentials file).
4. Every request to the hub address carries `Grouper-Relay: <relay_token>`, WebSocket
   handshakes included. A `401` whose JSON has `"relay": true` (or a `503` with `"relay": true`:
   hub offline) is about the relay, not the hub: on 401 redo steps 1–3, on 503 show "your hub
   is offline".
5. Then the hub's own sign-in as before (`/v1/auth/google/nonce`, `/v1/auth/google`). A hub nonce
   is sent to Google verbatim **only if it matches `^[0-9a-f]{32}$`**; anything else is refused
   (so a hub can never get a token usable at the relay).

Addresses that aren't a relay (a LAN address, `localhost`, Tailscale) skip steps 1–4: a client
treats an address as a relay when `GET /relay/v1/me` answers JSON with a `signed_in` field.

**Linking a hub** (owner, once): `POST /v1/relay/nonce` on the hub → Google sign-in with the hashed
nonce → `POST /v1/relay/link` `{id_token, relay_nonce}`. Offered in the Windows app's tray menu
("Turn on phone and web access") using its desktop Google client, and in the web app's Settings →
Remote access. The hub's default relay URL is `https://grouper.clockmillbarn.uk`.

### What the shipped clients do

- **Swift apps** (`apple/`): `RelayClient` (GrouperAPI) does `nonce`, `sign-in` and `me`; the relay nonce
  is hashed by `RelayNonce.hashed` (the package's own SHA-256, the same one PKCE uses). The relay token
  lives in the Keychain (`grouper.relay`) with the hub address it was made for in `UserDefaults`.
  `HubClient`, `HubMoveClient` and `EventStream` take a `relayToken` and send `Grouper-Relay` on every
  request and the WebSocket handshake. `HubClient.googleNonce()` refuses anything that isn't 32
  lowercase hex characters. "Sign in with Google" on a relay address runs two Google sessions back to
  back (relay, then hub). A relay `503` shows "Your hub is offline"; a relay `401` drops the saved relay
  token and asks for sign-in again.
- **Rust** (`core-client`, used by the Mac app through `crates/ffi`, and by the CLI): `Config.relay_token`
  is sent as `Grouper-Relay` on the `/v1/connect` handshake; a `401`/`503` handshake answer with
  `"relay": true` is retried (it is not a rejected device token); `core-sync`'s `SyncClient` has
  `with_relay_token` for its HTTP calls and the `/v1/events` handshake.
- **CLI sign-in** (`grouper login`): the CLI can't do Google, and `/v1/auth/link/*` is the hub's own flow,
  so against a relay address it first gets a relay token by a loopback code exchange (the OAuth
  native-app pattern; the token never appears in a URL):
  1. `GET /relay/v1/me` answers JSON with `signed_in` → the address is a relay. If the saved relay token
     is still accepted, skip to step 5.
  2. The CLI listens on `127.0.0.1:<port>` and opens
     `<hub>/relay/sign-in?next=/relay/v1/cli-token?port=<port>&state=<random>`. The browser signs in at
     the relay as for the web app.
  3. **Needs a relay endpoint (not built yet):** `GET /relay/v1/cli-token?port&state`, requires the relay
     session cookie, accepts only a loopback `port` (1024–65535), mints a single-use code bound to the
     account and `state` (valid about 60 seconds), and redirects to `http://127.0.0.1:<port>/?code=<code>&state=<state>`.
  4. **Needs a relay endpoint (not built yet):** `POST /relay/v1/cli-token/exchange` `{code, state}` →
     `{relay_token, expires}`; the code works once; the token is the same kind `sign-in` with
     `client: "app"` returns. The CLI checks `state` before it exchanges.
  5. The CLI runs the existing link flow with `Grouper-Relay: <relay_token>` on every call; the
     approving browser reaches `/#/link?code=...` through the relay with its cookie. On approval the CLI
     saves the relay token (`credentials.json`, protected like the device token: DPAPI, Keychain
     account `relay-token`, or the 0600 file) beside the hub and device token. Every later command sends
     it; `GROUPER_RELAY_TOKEN` overrides it for automation.
