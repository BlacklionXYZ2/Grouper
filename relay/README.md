# Grouper relay

A Cloudflare Worker with one Durable Object per account. A user's hub dials **out** to the relay
over one WebSocket; phones, Macs and browsers sign in to the relay with Google and are joined only
to **their own account's** hub. The relay is a router, not an authority: the hub still checks every
request itself. Full design, token formats and the tunnel frame protocol: `../docs/relay-design.md`.

Source layout (`src/`):

| File | What |
|---|---|
| `index.ts` | Worker: routes, sign-in, hub claim, forwarding to the account object |
| `account.ts` | `Account` Durable Object: nonces, generation, hub secret hash, the hub tunnel (Hibernation API) |
| `google.ts` | Google ID token verification (WebCrypto RS256, JWKS cache) |
| `tokens.ts` | Stateless HMAC account keys, relay nonces, relay tokens, hub secrets |
| `pages.ts` | Sign-in and "hub offline" HTML pages |

## Develop

```sh
cd relay
npm install
cp .dev.vars.example .dev.vars     # local RELAY_SECRET; .dev.vars is git-ignored
npx wrangler dev                   # http://localhost:8787 (add --local to stay fully offline)
npx tsc --noEmit                   # typecheck
npm test                           # end-to-end smoke test, see below
```

The smoke test (`test/smoke.mjs`) needs nothing deployed: it runs a fake Google (RSA key, JWKS,
minted ID tokens), starts `wrangler dev --local` on port 8799 against it (config passed with
`--var`, state in a temp dir), and plays two fake hubs plus clients. It covers routing isolation
between accounts, nonce/aud/exp/signature checks, credential stripping, streaming, WebSockets,
sign-out everywhere, re-claim (close code 4001) and the no-hub 503. Set `RELAY_TEST_PORT` to
change the port and `RELAY_TEST_VERBOSE=1` to print the wrangler log.

## Configuration

Set in `wrangler.toml` `[vars]` (non-secret) or as a secret.

| Name | Meaning |
|---|---|
| `RELAY_SECRET` (secret) | HMAC key for nonces, relay tokens and account keys. Long random string. Rotating it signs everyone out and orphans every account (new account keys). |
| `GOOGLE_CLIENT_IDS` | Comma-separated audiences accepted in ID tokens (iOS, Mac, web, desktop clients). |
| `GOOGLE_WEB_CLIENT_ID` | OAuth client used by the relay's own browser sign-in page. Its authorized redirect URI must be `https://<relay host>/relay/auth/return`. |
| `GOOGLE_JWKS_URL` | Default `https://www.googleapis.com/oauth2/v3/certs`. |
| `GOOGLE_AUTH_URL` | Default `https://accounts.google.com/o/oauth2/v2/auth`. |
| `GOOGLE_ISSUERS` | Default `https://accounts.google.com,accounts.google.com`. |

## Deploy (not done by this repo's tooling)

1. Create the Google OAuth clients and put their IDs in `GOOGLE_CLIENT_IDS` / `GOOGLE_WEB_CLIENT_ID`
   in `wrangler.toml`.
2. `npx wrangler login`
3. `openssl rand -base64 48 | npx wrangler secret put RELAY_SECRET`
4. `npx wrangler deploy` (the first deploy applies the `v1` migration that creates the SQLite-backed
   `Account` class).
5. Optionally attach a custom domain in the Cloudflare dashboard; point hubs at the resulting URL.

## Behaviour notes

- Times in nonces and tokens are unix **seconds**.
- A hub that sends nothing for 90 s is closed (code 4003). Idle checks run from an alarm every
  60 s, so the close happens 90-150 s after the last frame. The exact text frame `{"t":"ping"}` is
  answered with `{"t":"pong"}` by the platform without waking the object (this also applies to a
  client WebSocket that sends exactly that text).
- Closing codes on the hub socket: 4001 replaced (re-claim or newer connection), 4002 forgotten
  (`DELETE /relay/v1/hubs`), 4003 idle.
- Client WebSockets get close code 1012 when the hub goes away or reconnects.
- A client abort sends `cancel` to the hub when the platform reports it (`request.signal`, and
  cancellation of the streamed body). `wrangler dev`'s local proxy does not forward client
  disconnects, so that path is only exercised in production.
