// Grouper relay Worker: sign-in, hub claim, and routing to each account's Durable Object.

export { Account } from "./account";

import { AuthError, DEFAULT_AUTH_URL, verifyIdToken, type GoogleClaims } from "./google";
import { errorPage, signInPage } from "./pages";
import {
  ACCT_RE,
  APP_TOKEN_TTL,
  WEB_TOKEN_TTL,
  acctKey,
  checkNonce,
  hubSecretAcct,
  makeHubSecret,
  makeNonce,
  makeRelayToken,
  readRelayToken,
  type RelayClaims,
} from "./tokens";
import { json, nowSec, relayError, sha256Hex, type Env } from "./util";

const COOKIE = "grouper_relay";
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
      if (e instanceof AuthError) return relayError(401, e.message);
      if (e instanceof HttpError) return relayError(e.status, e.message);
      console.error("relay error", e instanceof Error ? e.message : String(e));
      return relayError(500, "internal relay error");
    }
  },
} satisfies ExportedHandler<Env>;

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
      case "/relay/auth/start":
        return need(m, "GET") ?? authStart(url, env);
      case "/relay/auth/return":
        return need(m, "POST") ?? authReturn(request, env);
      default:
        return relayError(404, "unknown relay route");
    }
  }
  return forward(request, env, url);
}

function need(method: string, want: string): Response | null {
  return method === want ? null : relayError(405, `use ${want}`, { allow: want });
}

// ---- helpers --------------------------------------------------------------------------------

async function readJson(request: Request): Promise<Record<string, unknown>> {
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
      return (i < 0 ? s : s.slice(0, i)).trim() !== COOKIE;
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

/** Full ID-token check including the relay nonce (and its one-time use). Returns claims + account. */
async function verifySignIn(
  env: Env,
  idToken: unknown,
): Promise<{ g: GoogleClaims; acct: string; stub: Stub }> {
  const g = await verifyIdToken(env, idToken);
  const exp = await checkNonce(env.RELAY_SECRET, g.nonce);
  if (exp === null || typeof g.nonce !== "string") throw new AuthError("missing or invalid relay nonce");
  const acct = await acctKey(env.RELAY_SECRET, g.sub);
  const stub = stubFor(env, acct);
  if (!(await stub.consumeNonce(g.nonce, exp))) throw new AuthError("relay nonce already used");
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
  const { g, acct, stub } = await verifySignIn(env, body.id_token);
  const { token, expires } = await issueToken(env, acct, g.email, client, stub);
  const hub_connected = (await stub.hubInfo()).hub_connected;
  if (client === "web") {
    return json(200, { email: g.email, hub_connected }, { "set-cookie": setCookie(new URL(request.url), token, WEB_TOKEN_TTL) });
  }
  return json(200, { relay_token: token, email: g.email, hub_connected, expires });
}

async function signOut(request: Request, env: Env): Promise<Response> {
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
  const { g, acct, stub } = await verifySignIn(env, body.id_token);
  const secret = makeHubSecret(acct);
  await stub.claimHub(body.hub_id, await sha256Hex(secret));
  return json(200, { hub_secret: secret, account_email: g.email });
}

function bearer(request: Request): string {
  return (request.headers.get("authorization") ?? "").replace(/^Bearer\s+/i, "").trim();
}

async function deleteHub(request: Request, env: Env): Promise<Response> {
  const secret = bearer(request);
  const acct = hubSecretAcct(secret);
  if (!acct || !(await stubFor(env, acct).forgetHub(secret))) return relayError(401, "wrong hub secret");
  return json(200, { ok: true });
}

async function hubConnect(request: Request, env: Env): Promise<Response> {
  const secret = bearer(request);
  const acct = hubSecretAcct(secret);
  if (!acct || !ACCT_RE.test(acct)) return relayError(401, "wrong hub secret or hub id");
  const headers = new Headers();
  for (const k of ["upgrade", "connection", "authorization", "grouper-hub-id", "sec-websocket-key", "sec-websocket-version"]) {
    const v = request.headers.get(k);
    if (v !== null) headers.set(k, v);
  }
  headers.set("x-relay-kind", "hub-connect");
  return stubFor(env, acct).fetch(new Request("https://account.internal/hub-connect", { method: "GET", headers }));
}

// ---- browser sign-in ------------------------------------------------------------------------

async function authStart(url: URL, env: Env): Promise<Response> {
  const { nonce } = await makeNonce(env.RELAY_SECRET);
  const g = new URL(env.GOOGLE_AUTH_URL || DEFAULT_AUTH_URL);
  g.searchParams.set("client_id", env.GOOGLE_WEB_CLIENT_ID);
  g.searchParams.set("redirect_uri", url.origin + "/relay/auth/return");
  g.searchParams.set("response_type", "id_token");
  g.searchParams.set("response_mode", "form_post");
  g.searchParams.set("scope", "openid email");
  g.searchParams.set("nonce", nonce);
  return new Response(null, { status: 302, headers: { location: g.toString(), "cache-control": "no-store" } });
}

async function authReturn(request: Request, env: Env): Promise<Response> {
  let idToken: unknown;
  try {
    idToken = (await request.formData()).get("id_token");
  } catch {
    return errorPage(400, "The sign-in response was malformed.");
  }
  try {
    const { g, acct, stub } = await verifySignIn(env, idToken);
    const { token } = await issueToken(env, acct, g.email, "web", stub);
    return new Response(null, {
      status: 303,
      headers: { location: "/", "set-cookie": setCookie(new URL(request.url), token, WEB_TOKEN_TTL), "cache-control": "no-store" },
    });
  } catch (e) {
    if (e instanceof AuthError) return errorPage(401, e.message);
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
    return wantsHtml(request) ? signInPage() : relayError(401, "sign in to the Grouper relay first");
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
