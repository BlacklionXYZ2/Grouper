# Accounts: Sign in with Google (and later Apple)

Status: design, 26 November 2026. Owner decisions so far:

- **Grouper is a product anyone can install.** Each person runs their own hub on their own
  hardware (a VM, a Windows PC, a Mac; movable to another device later). There is no
  shared multi-user hub: a hub has one owner.
- **"Sign up" means claiming your own hub with your Google account,** then signing every
  app and browser in with that account instead of pasting tokens.
- **Each person brings their own provider keys** (as today, per hub).
- **Google only for now.** Sign in with Apple needs the paid Apple Developer Program; the
  design leaves room for it (same flow, another issuer).
- Later: the owner picks which device is the hub during setup and can move it.

## Goals

1. No token pasting for normal use: the owner signs in with Google on the web client, the
   iPhone app and the Mac app; the apps get their tokens automatically (the Mac app's device
   token is already automatic: see the Mac auto-registration change).
2. No server run by the Grouper project is required for sign-in. A self-hosted hub on any
   domain works without the owner creating their own Google OAuth client.
3. Tokens stay as the fallback and for automation (`grouper-hub device add`, the CLI).

## How it works

### Identity: Google ID tokens, verified by each hub

- The Grouper project (Oscar) registers Google OAuth clients once: an iOS client for the
  iPhone app, one for the Mac app, a "Web application" client for browsers, and later a
  "TVs and limited input" client for the CLI. Their client IDs ship in the apps and are the
  hub's default trusted audiences (`GROUPER_GOOGLE_CLIENT_IDS` overrides them for a fork).
  Scopes: `openid email profile` only (non-sensitive; light consent-screen verification).
- An app signs in with Google directly (OAuth 2.0 authorization code + PKCE in
  `ASWebAuthenticationSession`, redirect to the client's reversed-client-id scheme; no Google
  SDK, no client secret on device) and gets a Google **ID token**.
- The hub verifies the ID token itself: signature against Google's JWKS (fetched and cached
  per `Cache-Control`), `iss` is Google, `aud` is a trusted client ID, `exp`/`iat` in range,
  `email_verified` true, and **`nonce` equals a one-time nonce this hub issued** in the last
  10 minutes.
- **Why the nonce matters:** an ID token for Grouper's client ID would otherwise be accepted
  by any hub. A hub only accepts tokens whose nonce it issued, once, so a token minted for
  hub A can't be replayed at hub B, and a malicious hub can't use your sign-in elsewhere.

### Owner and claim

- A hub stores one owner: Google `sub` (stable id), email and name. Only that `sub` can
  sign in. (A later "invite a second person" feature would add rows; out of scope.)
- **Claiming a new hub** needs a one-time **setup code** so nobody who finds a freshly
  exposed hub can claim it: on first start the hub logs a setup code and the claim URL
  (`https://<hub>/#/claim?code=...`); `grouper-hub setup-code` prints a fresh one. The first
  Google sign-in that presents a valid code becomes the owner. Codes expire after 1 hour.
- **Existing hubs** (Oscar's): an admin session can "Link your Google account" in Settings
  (runs the same flow with the admin session as proof instead of a setup code).
- `grouper-hub owner reset` (CLI on the hub machine) unlinks the owner if the Google account
  is lost; tokens keep working.

### What sign-in returns

- **Web:** an admin session cookie, the same as signing in with an admin token today.
- **Apps:** an admin token minted for that app install (a device row named
  "Grouper on <device name>", admin, revocable from Devices), stored in the Keychain. The
  Mac app additionally auto-registers its non-admin node device (separate change).
- `POST /v1/auth/google/nonce` -> `{nonce, expires}` (unauthenticated, rate-limited).
- `POST /v1/auth/google` `{id_token, setup_code?, client: "web"|"ios"|"macos"|"cli",
  device_name?}` -> web: sets the session cookie; apps: `{token, device}`.
- `GET /v1/auth/status` (unauthenticated) -> `{claimed: bool, google: bool}` so sign-in
  pages know whether to ask for a setup code and whether Google sign-in is enabled.

### The web client on any domain (decided 26 Nov: a hidden return page)

The owner sees ordinary Google sign-in: "Sign in with Google" opens Google's page and comes
back signed in. Behind the scenes: Google only returns to redirect URIs registered on the
Grouper web client, and every self-hosted hub has its own address, so all browsers return
to one registered **return page**, which forwards the result to the hub that started the
sign-in (named in the OAuth `state`, alongside a value that hub checks).

- Flow: the hub's sign-in page gets a nonce and a signed `state` from its hub, then sends
  the browser to Google (response type `id_token`, `response_mode=form_post` or fragment,
  `redirect_uri` = the return page). The return page reads the hub origin from `state` and
  forwards the ID token to `https://<hub>/v1/auth/google/return` with a form POST. The hub
  checks `state` (it made it, unused, unexpired), then verifies the ID token and its nonce as
  above and sets the session.
- Safe because a token is bound to the nonce of the hub that asked for it; the return page
  stores nothing.
- Every hub serves the same static return page at `/oauth/google/return`. The registered
  one is Oscar's hub (`https://grouper.clockmillbarn.uk/oauth/google/return`) for now; it can
  move to its own static site (for example Cloudflare Pages) later without changing hubs,
  because the return URL is configuration (`GROUPER_GOOGLE_RETURN_URL`).
- A hub owner can still register their own web client (`GROUPER_GOOGLE_WEB_CLIENT_ID`) and
  skip the shared return page.

### Finding your hub

v1: the app asks for the hub address once (typed, or scanned from a QR on the hub's
setup/claim page). Later: LAN discovery (Bonjour) and, if the hub moves between devices,
a stable address via the tunnel. A project-run directory ("which hubs does this Google
account own") is possible later but not needed now.

## What Oscar needs to do (one-time, free)

In Google Cloud Console, in a project for Grouper:

1. OAuth consent screen: External, app name "Grouper", support email, scopes
   `openid`, `email`, `profile`. Publish it (non-sensitive scopes).
2. Create OAuth client IDs:
   - iOS, bundle ID `uk.clockmillbarn.grouper.ios`.
   - iOS type again for the Mac app, bundle ID `uk.clockmillbarn.grouper.macos` (Google's
     iOS client type covers macOS apps).
   - Web application, authorized redirect URI
     `https://grouper.clockmillbarn.uk/oauth/google/return`.
3. Send the client IDs (they are public identifiers, not secrets; they go in the apps and the
   hub config).

## Build order

1. Hub: ID-token verification (JWKS cache), nonce store, owner table, setup codes and
   `grouper-hub setup-code` / `owner reset`, the endpoints above, tests with a local fake
   JWKS and self-signed test tokens.
2. Apple apps: "Sign in with Google" on the sign-in screen (PKCE in
   `ASWebAuthenticationSession`), hub address + optional setup code, token in Keychain;
   Settings shows the linked account.
3. Web: claim page and the chosen browser sign-in option; Settings > "Link your Google
   account" for existing hubs.
4. CLI (`grouper login`): Google device flow, later.
