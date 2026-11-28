// Grouper relay Worker: sign-in, hub claim, and routing to each account's Durable Object.

export { Account } from "./account";

import { AuthError, DEFAULT_AUTH_URL, verifyIdToken, type GoogleClaims } from "./google";
import { cliConfirmPage, errorPage, signInPage } from "./pages";
import {
  APP_TOKEN_TTL,
  CLI_CODE_TTL,
  checkFormToken,
  makeCliCode,
  makeFormToken,
  verifyCliCode,
  WEB_TOKEN_TTL,
  acctKey,
  checkNonce,
  makeHubSecret,
  nonceClaim,
  verifyHubSecret,
  makeNonce,
  makeRelayToken,
  readRelayToken,
  type RelayClaims,
} from "./tokens";
import { b64urlDecode, b64urlEncode, json, nowSec, randomB64url, relayError, safeEqual, sha256Hex, utf8, type Env } from "./util";

const COOKIE = "grouper_relay";
const STATE_COOKIE = "grouper_relay_state";
const STATE_TTL = 600;
const MAX_JSON = 16 * 1024;

type Stub = DurableObjectStub<import("./account").Account>;

function stubFor(env: Env, acct: string): Stub {
  return env.ACCOUNT.get(env.ACCOUNT.idFromName(acct));
}

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    try {
      return await route(request, env);
    } catch (e) {
      if (e instanceof GuardError) return e.response;
      if (e instanceof AuthError) return relayError(401, e.message);
      if (e instanceof HttpError) return relayError(e.status, e.message);
      console.error("relay error", e instanceof Error ? e.message : String(e));
      return relayError(500, "internal relay error");
    }
  },
} satisfies ExportedHandler<Env>;

class GuardError extends Error {
  constructor(public response: Response) {
    super("guard");
  }
}

class HttpError extends Error {
  constructor(
    public status: number,
    message: string,
  ) {
    super(message);
  }
}

async function route(request: Request, env: Env): Promise<Response> {
  if (!env.RELAY_SECRET) return relayError(500, "relay is not configured");
  const url = new URL(request.url);
  const p = url.pathname;
  const m = request.method;

  if (p.startsWith("/relay/v1/") || p.startsWith("/relay/auth/")) {
    switch (p) {
      case "/relay/v1/nonce":
        return need(m, "POST") ?? json(200, await makeNonce(env.RELAY_SECRET));
      case "/relay/v1/sign-in":
        return need(m, "POST") ?? signInApi(request, env);
      case "/relay/v1/sign-out":
        return need(m, "POST") ?? signOut(request, env);
      case "/relay/v1/me":
        return need(m, "GET") ?? me(request, env);
      case "/relay/v1/hubs/claim":
        return need(m, "POST") ?? claimHub(request, env);
      case "/relay/v1/hubs":
        return need(m, "DELETE") ?? deleteHub(request, env);
      case "/relay/v1/hub/connect":
        return need(m, "GET") ?? hubConnect(request, env);
      case "/relay/v1/cli-token":
        if (m === "GET") return cliConfirm(request, env, url);
        return need(m, "POST") ?? cliMint(request, env);
      case "/relay/v1/cli-token/exchange":
        return need(m, "POST") ?? cliExchange(request, env);
      case "/relay/auth/start":
        return need(m, "GET") ?? authStart(url, env);
      case "/relay/auth/return":
        return need(m, "POST") ?? authReturn(request, env);
      default:
        return relayError(404, "unknown relay route");
    }
  }
  if (p === "/relay/sign-in") {
    return need(m, "GET") ?? signInPage(safeNext(url.searchParams.get("next")));
  }
  if (p.startsWith("/relay/")) return relayError(404, "unknown relay route");

  if (isHubPath(p)) {
    // The relay's own browser sign-in returns to the path Google has registered for the hub.
    if (p === "/oauth/google/return" && m === "POST" && (await isRelayReturn(request))) return authReturn(request, env);
    // Every hub's browser Google sign-in comes back through this registered URI. The page needs no
    // hub and no relay session: it reads the starting hub's origin from `state` and posts Google's
    // answer on to that hub, exactly as the hub's own copy of the page does.
    if (p === "/oauth/google/return" && (m === "POST" || m === "GET")) return hubReturnPage(request, url);
    return forward(request, env, url);
  }
  return website(request, env);
}

/** Paths forwarded to the signed-in account's hub. Everything else is the website. */
function isHubPath(p: string): boolean {
  return (
    p === "/app" ||
    p.startsWith("/app/") ||
    p.startsWith("/v1/") ||
    p === "/sw.js" ||
    p === "/oauth" ||
    p.startsWith("/oauth/") ||
    p === "/favicon.ico" ||
    p === "/healthz"
  );
}

/** A same-origin relative path, or the default. Refuses scheme-relative URLs, backslashes and control characters. */
function safeNext(raw: string | null | undefined): string {
  const d = "/app";
  if (!raw || raw.length > 512) return d;
  if (!raw.startsWith("/") || raw.startsWith("//")) return d;
  if (/[\\\u0000-\u001f\u007f]/.test(raw)) return d;
  try {
    if (new URL(raw, "https://relay.invalid").origin !== "https://relay.invalid") return d;
  } catch {
    return d;
  }
  return raw;
}

/** True when the posted `state` matches this browser's relay state cookie. Reads a clone; the body stays intact. */
async function isRelayReturn(request: Request): Promise<boolean> {
  const cookie = cookieValue(request.headers.get("cookie"), STATE_COOKIE);
  if (!cookie) return false;
  const cState = parseState(cookie).state;
  if (!cState) return false;
  try {
    const posted = (await request.clone().formData()).get("state");
    return typeof posted === "string" && safeEqual(posted, cState);
  } catch {
    return false;
  }
}

/** Cookie value is `<state>.<relay nonce>`, plus `~<base64url next>` when `next` isn't the default. */
function parseState(cookie: string): { state: string; nonce: string; next: string } {
  let rest = cookie;
  let next = "/app";
  const t = rest.lastIndexOf("~");
  if (t >= 0) {
    try {
      next = safeNext(new TextDecoder().decode(b64urlDecode(rest.slice(t + 1))));
    } catch {}
    rest = rest.slice(0, t);
  }
  const dot = rest.indexOf(".");
  return dot > 0 ? { state: rest.slice(0, dot), nonce: rest.slice(dot + 1), next } : { state: "", nonce: "", next };
}

/** Website: static assets, with the site's own 404 page. */
async function website(request: Request, env: Env): Promise<Response> {
  if (request.method !== "GET" && request.method !== "HEAD") return relayError(405, "use GET", { allow: "GET, HEAD" });
  const res = await env.ASSETS.fetch(request);
  if (res.status !== 404) return res;
  const nf = await env.ASSETS.fetch(new Request(new URL("/404", request.url), { headers: { accept: "text/html" } }));
  return new Response(request.method === "HEAD" ? null : nf.body, {
    status: 404,
    headers: { "content-type": "text/html; charset=utf-8", "cache-control": "no-store" },
  });
}

function need(method: string, want: string): Response | null {
  return method === want ? null : relayError(405, `use ${want}`, { allow: want });
}

// ---- helpers ----

/** State-changing JSON endpoints: JSON content type (no cross-site form posts) and same-origin Origin. */
function guardJson(request: Request): Response | null {
  const ct = (request.headers.get("content-type") ?? "").toLowerCase();
  if (!ct.startsWith("application/json")) return relayError(415, "content-type must be application/json");
  const origin = request.headers.get("origin");
  if (origin !== null && origin !== new URL(request.url).origin) return relayError(403, "cross-origin request refused");
  return null;
}

async function readJson(request: Request): Promise<Record<string, unknown>> {
  const bad = guardJson(request);
  if (bad) throw new GuardError(bad);
  const text = await request.text();
  if (text.length > MAX_JSON) throw new HttpError(413, "body too large");
  try {
    const v = JSON.parse(text || "{}");
    if (typeof v === "object" && v !== null && !Array.isArray(v)) return v as Record<string, unknown>;
  } catch {}
  throw new HttpError(400, "expected a JSON object");
}

function cookieValue(header: string | null, name: string): string | null {
  if (!header) return null;
  for (const part of header.split(";")) {
    const i = part.indexOf("=");
    if (i > 0 && part.slice(0, i).trim() === name) return part.slice(i + 1).trim();
  }
  return null;
}

function stripRelayCookie(header: string | null): string | null {
  if (!header) return null;
  const kept = header
    .split(";")
    .map((s) => s.trim())
    .filter((s) => {
      if (!s) return false;
      const i = s.indexOf("=");
      const n = (i < 0 ? s : s.slice(0, i)).trim();
      return n !== COOKIE && n !== STATE_COOKIE;
    });
  return kept.length ? kept.join("; ") : null;
}

function setCookie(url: URL, value: string, maxAge: number): string {
  return `${COOKIE}=${value}; HttpOnly; SameSite=Lax; Path=/; Max-Age=${maxAge}${
    url.protocol === "https:" ? "; Secure" : ""
  }`;
}

/** A relay token from `Grouper-Relay` or the cookie, if valid and of the current generation. */
async function authenticate(request: Request, env: Env): Promise<{ claims: RelayClaims; stub: Stub } | null> {
  const token = request.headers.get("grouper-relay") ?? cookieValue(request.headers.get("cookie"), COOKIE);
  const claims = await readRelayToken(env.RELAY_SECRET, token);
  if (!claims) return null;
  const stub = stubFor(env, claims.a);
  if ((await stub.generation()) !== claims.g) return null;
  return { claims, stub };
}

/**
 * Full ID-token check including the relay nonce. The token's `nonce` claim must be the domain-separated
 * hash of `relayNonce`, and `relayNonce` must be ours, unexpired and unused (consumed here).
 */
async function verifySignIn(
  env: Env,
  idToken: unknown,
  relayNonce: unknown,
  audiences?: string[],
): Promise<{ g: GoogleClaims; acct: string; stub: Stub }> {
  const g = await verifyIdToken(env, idToken, audiences);
  const exp = await checkNonce(env.RELAY_SECRET, relayNonce);
  if (exp === null || typeof relayNonce !== "string") throw new AuthError("missing or invalid relay nonce");
  if (typeof g.nonce !== "string" || !safeEqual(g.nonce, await nonceClaim(relayNonce))) {
    throw new AuthError("id_token was not issued for this relay nonce");
  }
  const acct = await acctKey(env.RELAY_SECRET, g.sub);
  const stub = stubFor(env, acct);
  if (!(await stub.consumeNonce(relayNonce, exp))) throw new AuthError("relay nonce already used");
  return { g, acct, stub };
}

async function issueToken(
  env: Env,
  acct: string,
  email: string,
  kind: "web" | "app",
  stub: Stub,
): Promise<{ token: string; expires: number }> {
  const expires = nowSec() + (kind === "web" ? WEB_TOKEN_TTL : APP_TOKEN_TTL);
  const token = await makeRelayToken(env.RELAY_SECRET, {
    a: acct,
    e: email,
    k: kind,
    g: await stub.generation(),
    x: expires,
  });
  return { token, expires };
}

// ---- relay API ------------------------------------------------------------------------------

async function signInApi(request: Request, env: Env): Promise<Response> {
  const body = await readJson(request);
  const client = body.client;
  if (client !== "web" && client !== "app") throw new HttpError(400, 'client must be "web" or "app"');
  const { g, acct, stub } = await verifySignIn(env, body.id_token, body.relay_nonce);
  const { token, expires } = await issueToken(env, acct, g.email, client, stub);
  const hub_connected = (await stub.hubInfo()).hub_connected;
  if (client === "web") {
    return json(200, { email: g.email, hub_connected }, { "set-cookie": setCookie(new URL(request.url), token, WEB_TOKEN_TTL) });
  }
  return json(200, { relay_token: token, email: g.email, hub_connected, expires });
}

async function signOut(request: Request, env: Env): Promise<Response> {
  const bad = guardJson(request);
  if (bad) return bad;
  const auth = await authenticate(request, env);
  if (!auth) return relayError(401, "sign in to the Grouper relay first");
  const body = await readJson(request);
  if (body.everywhere === true) await auth.stub.bumpGeneration();
  return json(200, { ok: true }, { "set-cookie": setCookie(new URL(request.url), "", 0) });
}

async function me(request: Request, env: Env): Promise<Response> {
  const auth = await authenticate(request, env);
  if (!auth) return json(200, { signed_in: false });
  const info = await auth.stub.hubInfo();
  return json(200, { signed_in: true, email: auth.claims.e, hub_connected: info.hub_connected, hub_id: info.hub_id });
}

async function claimHub(request: Request, env: Env): Promise<Response> {
  const body = await readJson(request);
  if (typeof body.hub_id !== "string" || !body.hub_id || body.hub_id.length > 200) {
    throw new HttpError(400, "hub_id is required");
  }
  const { g, acct, stub } = await verifySignIn(env, body.id_token, body.relay_nonce);
  const secret = await makeHubSecret(env.RELAY_SECRET, acct);
  await stub.claimHub(body.hub_id, await sha256Hex(secret));
  return json(200, { hub_secret: secret, account_email: g.email });
}

function bearer(request: Request): string {
  return (request.headers.get("authorization") ?? "").replace(/^Bearer\s+/i, "").trim();
}

async function deleteHub(request: Request, env: Env): Promise<Response> {
  const secret = bearer(request);
  const acct = await verifyHubSecret(env.RELAY_SECRET, secret);
  if (!acct || !(await stubFor(env, acct).forgetHub(secret))) return relayError(401, "wrong hub secret");
  return json(200, { ok: true });
}

async function hubConnect(request: Request, env: Env): Promise<Response> {
  const secret = bearer(request);
  const acct = await verifyHubSecret(env.RELAY_SECRET, secret);
  if (!acct) return relayError(401, "wrong hub secret or hub id");
  const headers = new Headers();
  for (const k of ["upgrade", "connection", "authorization", "grouper-hub-id", "sec-websocket-key", "sec-websocket-version"]) {
    const v = request.headers.get(k);
    if (v !== null) headers.set(k, v);
  }
  headers.set("x-relay-kind", "hub-connect");
  return stubFor(env, acct).fetch(new Request("https://account.internal/hub-connect", { method: "GET", headers }));
}

// ---- command-line sign-in (loopback code exchange) --------------------------------------------

const CLI_STATE_RE = /^[A-Za-z0-9_-]{16,128}$/;

function signInRedirect(url: URL): Response {
  const next = safeNext(url.pathname + url.search);
  return new Response(null, {
    status: 302,
    headers: { location: "/relay/sign-in?next=" + encodeURIComponent(next), "cache-control": "no-store" },
  });
}

/** Valid loopback port (1024-65535) and state, or null. */
function cliParams(port: unknown, state: unknown): { port: string; state: string } | null {
  if (typeof port !== "string" || !/^\d{4,5}$/.test(port)) return null;
  const n = Number(port);
  if (n < 1024 || n > 65535 || String(n) !== port) return null;
  if (typeof state !== "string" || !CLI_STATE_RE.test(state)) return null;
  return { port, state };
}

/** GET: needs a session, then only shows a confirmation page. It never mints anything. */
async function cliConfirm(request: Request, env: Env, url: URL): Promise<Response> {
  const auth = await authenticate(request, env);
  if (!auth) return wantsHtml(request) ? signInRedirect(url) : relayError(401, "sign in to the Grouper relay first");
  const p = cliParams(url.searchParams.get("port"), url.searchParams.get("state"));
  if (!p) return relayError(400, "port must be 1024-65535 and state 16-128 URL-safe characters");
  const csrf = await makeFormToken(env.RELAY_SECRET, auth.claims, p.port, p.state);
  return cliConfirmPage(auth.claims.e, p.port, p.state, csrf);
}

/** POST from the confirmation page: same origin, form token bound to the session; mints a 60 s single-use code. */
async function cliMint(request: Request, env: Env): Promise<Response> {
  const origin = request.headers.get("origin");
  if (origin !== null && origin !== new URL(request.url).origin) return relayError(403, "cross-origin request refused");
  const auth = await authenticate(request, env);
  if (!auth) return relayError(401, "sign in to the Grouper relay first");
  if (!(request.headers.get("content-type") ?? "").toLowerCase().startsWith("application/x-www-form-urlencoded")) {
    return relayError(415, "content-type must be application/x-www-form-urlencoded");
  }
  let form: FormData;
  try {
    form = await request.formData();
  } catch {
    return relayError(400, "malformed form");
  }
  const p = cliParams(form.get("port"), form.get("state"));
  if (!p) return relayError(400, "port must be 1024-65535 and state 16-128 URL-safe characters");
  if (!(await checkFormToken(env.RELAY_SECRET, auth.claims, p.port, p.state, form.get("csrf")))) {
    return relayError(403, "this confirmation expired or wasn't issued to you; start again");
  }
  const code = await makeCliCode(env.RELAY_SECRET, auth.claims.a);
  await auth.stub.putCliCode(await sha256Hex(code), p.state, auth.claims.e, Number(env.CLI_CODE_TTL) || CLI_CODE_TTL);
  // Only the loopback address, never another host.
  return new Response(null, {
    status: 303,
    headers: {
      location: `http://127.0.0.1:${p.port}/?code=${encodeURIComponent(code)}&state=${encodeURIComponent(p.state)}`,
      "cache-control": "no-store",
      "referrer-policy": "no-referrer",
    },
  });
}

async function cliExchange(request: Request, env: Env): Promise<Response> {
  const body = await readJson(request);
  const state = body.state;
  if (typeof state !== "string" || !CLI_STATE_RE.test(state)) throw new HttpError(400, "state is required");
  const acct = await verifyCliCode(env.RELAY_SECRET, body.code);
  if (!acct || typeof body.code !== "string") throw new HttpError(401, "invalid or expired code");
  const stub = stubFor(env, acct);
  const email = await stub.takeCliCode(await sha256Hex(body.code), state);
  if (email === null) throw new HttpError(401, "invalid or expired code");
  const { token, expires } = await issueToken(env, acct, email, "app", stub);
  return json(200, { relay_token: token, expires });
}

// ---- browser sign-in ------------------------------------------------------------------------

function webAudiences(env: Env): string[] | undefined {
  return env.RELAY_WEB_CLIENT_ID ? [env.RELAY_WEB_CLIENT_ID] : undefined;
}

/** Cookie for the state binding. SameSite=None on HTTPS because Google's form_post is a cross-site POST. */
function stateCookie(url: URL, value: string, maxAge: number): string {
  const https = url.protocol === "https:";
  return `${STATE_COOKIE}=${value}; HttpOnly; SameSite=${https ? "None; Secure" : "Lax"}; Path=/; Max-Age=${maxAge}`;
}

async function authStart(url: URL, env: Env): Promise<Response> {
  const next = safeNext(url.searchParams.get("next"));
  const { nonce } = await makeNonce(env.RELAY_SECRET);
  const state = randomB64url(18);
  const g = new URL(env.GOOGLE_AUTH_URL || DEFAULT_AUTH_URL);
  g.searchParams.set("client_id", env.RELAY_WEB_CLIENT_ID || env.GOOGLE_WEB_CLIENT_ID);
  g.searchParams.set("redirect_uri", url.origin + "/oauth/google/return");
  g.searchParams.set("response_type", "id_token");
  g.searchParams.set("response_mode", "form_post");
  g.searchParams.set("scope", "openid email");
  g.searchParams.set("nonce", await nonceClaim(nonce));
  g.searchParams.set("state", state);
  // The relay nonce rides in the cookie, so only this browser can complete this sign-in.
  return new Response(null, {
    status: 302,
    headers: {
      location: g.toString(),
      "set-cookie": stateCookie(url, `${state}.${nonce}${next === "/app" ? "" : "~" + b64urlEncode(utf8(next))}`, STATE_TTL),
      "cache-control": "no-store",
    },
  });
}

async function authReturn(request: Request, env: Env): Promise<Response> {
  const url = new URL(request.url);
  const clear = stateCookie(url, "", 0);
  let form: FormData;
  try {
    form = await request.formData();
  } catch {
    return errorPage(400, "The sign-in response was malformed.");
  }
  const cookie = cookieValue(request.headers.get("cookie"), STATE_COOKIE) ?? "";
  const { state: cState, nonce: cNonce, next } = parseState(cookie);
  const posted = form.get("state");
  if (!cState || typeof posted !== "string" || !safeEqual(posted, cState)) {
    const r = errorPage(401, "This sign-in wasn't started in this browser. Try again.");
    r.headers.append("set-cookie", clear);
    return r;
  }
  try {
    const { g, acct, stub } = await verifySignIn(env, form.get("id_token"), cNonce, webAudiences(env));
    const { token } = await issueToken(env, acct, g.email, "web", stub);
    const headers = new Headers({ location: next, "cache-control": "no-store" });
    headers.append("set-cookie", setCookie(url, token, WEB_TOKEN_TTL));
    headers.append("set-cookie", clear);
    return new Response(null, { status: 303, headers });
  } catch (e) {
    if (e instanceof AuthError) {
      const r = errorPage(401, e.message);
      r.headers.append("set-cookie", clear);
      return r;
    }
    throw e;
  }
}

// ---- forwarding -----------------------------------------------------------------------------

function wantsHtml(request: Request): boolean {
  return request.method === "GET" && (request.headers.get("accept") ?? "").includes("text/html");
}

async function forward(request: Request, env: Env, url: URL): Promise<Response> {
  const auth = await authenticate(request, env);
  if (!auth) {
    return wantsHtml(request) ? signInRedirect(url) : relayError(401, "sign in to the Grouper relay first");
  }

  const headers = new Headers();
  request.headers.forEach((v, k) => {
    const n = k.toLowerCase();
    if (n === "grouper-relay" || n === "cookie" || n.startsWith("x-relay-")) return;
    headers.append(n, v);
  });
  const cookie = stripRelayCookie(request.headers.get("cookie"));
  if (cookie) headers.set("cookie", cookie);
  headers.set("x-relay-kind", "proxy");
  headers.set("x-relay-host", url.host);
  headers.set("x-relay-ip", request.headers.get("cf-connecting-ip") ?? "0.0.0.0");
  headers.set("x-relay-proto", url.protocol.replace(":", ""));

  const hasBody = request.method !== "GET" && request.method !== "HEAD" && request.body !== null;
  const inner = new Request("https://account.internal" + url.pathname + url.search, {
    method: request.method,
    headers,
    body: hasBody ? request.body : undefined,
    redirect: "manual",
    signal: request.signal,
  });
  return auth.stub.fetch(inner);
}

// ---- shared Google return page ----------------------------------------------------------------

const RETURN_SUBMIT = "document.forms[0].submit()";

/** `scheme://host[:port]` a browser sign-in may return to: HTTPS, or plain HTTP on a loopback host. */
function returnOrigin(raw: unknown): string | null {
  if (typeof raw !== "string") return null;
  let u: URL;
  try {
    u = new URL(raw);
  } catch {
    return null;
  }
  if (u.username || u.password || u.origin !== raw) return null;
  if (u.protocol === "https:") return u.origin;
  if (u.protocol === "http:") {
    const h = u.hostname.replace(/^\[|\]$/g, "").toLowerCase();
    if (h === "localhost" || h === "::1" || /^127\.\d{1,3}\.\d{1,3}\.\d{1,3}$/.test(h)) return u.origin;
  }
  return null;
}

/** The hub origin named in a hub's sign-in `state` (base64url JSON `{o, id}`). */
function stateOrigin(state: string | null): string | null {
  if (!state || state.length > 2048) return null;
  try {
    const v = JSON.parse(new TextDecoder().decode(b64urlDecode(state.trim())));
    if (!v || typeof v.id !== "string" || !v.id) return null;
    return returnOrigin(v.o);
  } catch {
    return null;
  }
}

function escapeHtml(s: string): string {
  return s.replace(/[&<>"']/g, (c) => `&#${c.charCodeAt(0)};`);
}

async function hubReturnPage(request: Request, url: URL): Promise<Response> {
  const params = new Map<string, string>();
  url.searchParams.forEach((v, k) => params.set(k, v));
  if (request.method === "POST") {
    const text = await request.text();
    if (text.length <= 64 * 1024) new URLSearchParams(text).forEach((v, k) => params.set(k, v));
  }
  const origin = stateOrigin(params.get("state") ?? null);
  const base = {
    "content-type": "text/html; charset=utf-8",
    "cache-control": "no-store",
    "referrer-policy": "no-referrer",
    "x-content-type-options": "nosniff",
  };
  const page = (body: string) =>
    `<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="referrer" content="no-referrer">` +
    `<meta name="viewport" content="width=device-width"><title>Grouper sign-in</title></head><body>${body}</body></html>`;
  if (!origin) {
    return new Response(
      page("<p>This sign-in can't continue: it doesn't say which hub to return to. Go back to your hub and start again.</p>"),
      {
        status: 400,
        headers: { ...base, "content-security-policy": "default-src 'none'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'" },
      },
    );
  }
  const fields = ["id_token", "state", "error"]
    .filter((k) => params.has(k))
    .map((k) => `<input type="hidden" name="${k}" value="${escapeHtml(params.get(k)!)}">`)
    .join("");
  const hash = btoa(String.fromCharCode(...new Uint8Array(await crypto.subtle.digest("SHA-256", new TextEncoder().encode(RETURN_SUBMIT)))));
  const csp = `default-src 'none'; script-src 'sha256-${hash}'; form-action ${origin}; base-uri 'none'; frame-ancestors 'none'`;
  const action = escapeHtml(`${origin}/v1/auth/google/return`);
  return new Response(
    page(
      `<form method="post" action="${action}">${fields}<p>Returning to your hub…</p>` +
        `<noscript><button type="submit">Continue</button></noscript></form><script>${RETURN_SUBMIT}</script>`,
    ),
    { status: 200, headers: { ...base, "content-security-policy": csp } },
  );
}
