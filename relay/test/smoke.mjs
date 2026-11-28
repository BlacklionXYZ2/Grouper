// End-to-end smoke test for the Grouper relay. Self-contained: starts a fake Google, runs
// `wrangler dev --local` against it, and plays hubs and clients. Run: `npm test` (or
// `node test/smoke.mjs`) from relay/. Requires `npm install` first. Never touches Cloudflare.

import crypto from "node:crypto";
import http from "node:http";
import { spawn, execSync } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import WebSocket from "ws";

const PORT = Number(process.env.RELAY_TEST_PORT ?? 8799);
const GPORT = PORT - 1;
const BASE = `http://127.0.0.1:${PORT}`;
const WS_BASE = `ws://127.0.0.1:${PORT}`;
const GOOGLE = `http://127.0.0.1:${GPORT}`;
const CLIENT_ID = "test-client.apps.googleusercontent.com";
const relayDir = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

let passed = 0;
const failures = [];
function check(name, cond, detail = "") {
  if (cond) {
    passed++;
    console.log(`  ok   ${name}`);
  } else {
    failures.push(name);
    console.log(`  FAIL ${name} ${detail}`);
  }
}

// ---- fake Google ------------------------------------------------------------------------------

const { privateKey, publicKey } = crypto.generateKeyPairSync("rsa", { modulusLength: 2048 });
const other = crypto.generateKeyPairSync("rsa", { modulusLength: 2048 });
const jwk = { ...publicKey.export({ format: "jwk" }), kid: "test-kid", alg: "RS256", use: "sig" };
const b64u = (b) => Buffer.from(b).toString("base64url");

function mint(o) {
  const now = Math.floor(Date.now() / 1000);
  const claims = {
    iss: "https://accounts.google.com",
    aud: CLIENT_ID,
    sub: o.sub,
    email: o.email,
    email_verified: true,
    iat: now,
    exp: now + 3600,
    ...o.claims,
  };
  if (o.nonce !== undefined) claims.nonce = o.nonce;
  const head = b64u(JSON.stringify({ alg: "RS256", kid: "test-kid", typ: "JWT" }));
  const body = b64u(JSON.stringify(claims));
  const sig = crypto.sign("sha256", Buffer.from(`${head}.${body}`), o.key ?? privateKey);
  return `${head}.${body}.${b64u(sig)}`;
}

const google = http.createServer((req, res) => {
  if (req.url === "/jwks") {
    res.writeHead(200, { "content-type": "application/json", "cache-control": "public, max-age=300" });
    res.end(JSON.stringify({ keys: [jwk] }));
  } else {
    res.writeHead(200).end("fake google");
  }
});

// ---- relay process ------------------------------------------------------------------------------

const persist = mkdtempSync(path.join(tmpdir(), "grouper-relay-test-"));
const SECRET = crypto.randomBytes(24).toString("hex");
let relay;
let relayLog = "";

function startRelay() {
  const vars = {
    RELAY_SECRET: SECRET,
    GOOGLE_CLIENT_IDS: `${CLIENT_ID},other-client`,
    GOOGLE_WEB_CLIENT_ID: CLIENT_ID,
    GOOGLE_JWKS_URL: `${GOOGLE}/jwks`,
    GOOGLE_AUTH_URL: `${GOOGLE}/auth`,
  };
  const args = [
    "wrangler", "dev", "--local", "--ip", "127.0.0.1", "--port", String(PORT),
    "--persist-to", `"${persist}"`, "--show-interactive-dev-session=false",
    ...Object.entries(vars).flatMap(([k, v]) => ["--var", `${k}:${v}`]),
  ];
  relay = spawn("npx", args, { cwd: relayDir, shell: true, env: { ...process.env, CI: "1", WRANGLER_SEND_METRICS: "false" } });
  relay.stdout.on("data", (d) => (relayLog += d));
  relay.stderr.on("data", (d) => (relayLog += d));
}

function stopRelay() {
  if (!relay?.pid) return;
  try {
    if (process.platform === "win32") execSync(`taskkill /pid ${relay.pid} /T /F`, { stdio: "ignore" });
    else process.kill(-relay.pid);
  } catch {}
}

async function waitReady() {
  for (let i = 0; i < 120; i++) {
    try {
      const r = await fetch(`${BASE}/relay/v1/me`);
      if (r.ok) return;
    } catch {}
    await sleep(500);
  }
  throw new Error("relay did not start:\n" + relayLog.slice(-2000));
}

// ---- relay API helpers -------------------------------------------------------------------------------

async function api(method, p, { body, headers } = {}) {
  const r = await fetch(BASE + p, {
    method,
    headers: { ...(body ? { "content-type": "application/json" } : {}), ...headers },
    body: body ? JSON.stringify(body) : undefined,
    redirect: "manual",
  });
  const text = await r.text();
  let j = null;
  try { j = JSON.parse(text); } catch {}
  return { status: r.status, json: j, text, headers: r.headers };
}

async function newNonce() {
  return (await api("POST", "/relay/v1/nonce")).json.nonce;
}

async function appSignIn(user) {
  const nonce = await newNonce();
  const r = await api("POST", "/relay/v1/sign-in", { body: { id_token: mint({ ...user, nonce }), client: "app" } });
  return r.json?.relay_token;
}

// ---- fake hub ---------------------------------------------------------------------------------

const allHubs = [];
class FakeHub {
  constructor(name) {
    allHubs.push(this);
    this.name = name;
    this.cancels = [];
    this.closeCode = null;
    this.wsClosed = [];
  }
  connect(secret, hubId) {
    return new Promise((resolve, reject) => {
      const ws = new WebSocket(`${WS_BASE}/relay/v1/hub/connect`, {
        headers: { authorization: `Bearer ${secret}`, "grouper-hub-id": hubId },
      });
      this.ws = ws;
      ws.on("unexpected-response", (_q, res) => { res.resume(); reject(Object.assign(new Error("refused"), { status: res.statusCode })); });
      ws.on("error", reject);
      ws.on("open", () => {
        ws.removeListener("error", reject);
        ws.on("error", () => {});
        ws.send(JSON.stringify({ t: "hello", hub_id: hubId, version: "test" }));
        resolve();
      });
      ws.on("close", (code) => (this.closeCode = code));
      ws.on("message", (m) => this.onFrame(JSON.parse(m.toString())));
    });
  }
  send(o) { this.ws.send(JSON.stringify(o)); }
  onFrame(f) {
    if (f.t === "ping") return this.send({ t: "pong" });
    if (f.t === "cancel") return void this.cancels.push(f.id);
    if (f.t === "ws_open") return this.send({ t: "ws_ok", id: f.id });
    if (f.t === "ws_msg") {
      return this.send(f.text !== undefined
        ? { t: "ws_msg", id: f.id, text: `${this.name}:echo:${f.text}` }
        : { t: "ws_msg", id: f.id, b64: f.b64 });
    }
    if (f.t === "ws_close") return void this.wsClosed.push(f);
    if (f.t !== "req") return;
    if (f.path.startsWith("/hang")) return;
    if (f.path.startsWith("/boom")) return this.send({ t: "err", id: f.id, message: "kaput" });
    if (f.path.startsWith("/cut")) {
      this.send({ t: "res", id: f.id, status: 200, headers: [["content-type", "text/plain"]] });
      this.send({ t: "data", id: f.id, b64: Buffer.from("partial").toString("base64") });
      return this.send({ t: "err", id: f.id, message: "mid-stream" });
    }
    const echo = {
      hub: this.name,
      method: f.method,
      path: f.path,
      headers: f.headers,
      body: f.body ? Buffer.from(f.body, "base64").toString() : null,
    };
    const buf = Buffer.from(JSON.stringify(echo));
    const half = Math.floor(buf.length / 2);
    this.send({ t: "res", id: f.id, status: 201, headers: [["content-type", "application/json"], ["set-cookie", "a=1"], ["set-cookie", "b=2"], ["connection", "close"]] });
    this.send({ t: "data", id: f.id, b64: buf.subarray(0, half).toString("base64") });
    this.send({ t: "data", id: f.id, b64: buf.subarray(half).toString("base64") });
    this.send({ t: "end", id: f.id });
  }
  close() { try { this.ws.close(); } catch {} }
}

async function claim(user, hubId) {
  const nonce = await newNonce();
  const r = await api("POST", "/relay/v1/hubs/claim", { body: { hub_id: hubId, id_token: mint({ ...user, nonce }) } });
  return r;
}

function clientWs(p, headers) {
  return new Promise((resolve, reject) => {
    const ws = new WebSocket(WS_BASE + p, { headers });
    ws.on("unexpected-response", (_q, res) => { res.resume(); reject(Object.assign(new Error("refused"), { status: res.statusCode })); });
    ws.on("error", reject);
    ws.on("open", () => resolve(ws));
  });
}
const nextMsg = (ws) => new Promise((r) => ws.once("message", (m, bin) => r({ data: m, bin })));
const closed = (ws) => new Promise((r) => ws.once("close", (code) => r(code)));

// ---- the test ---------------------------------------------------------------------------------

const A = { sub: "sub-a", email: "a@example.com" };
const B = { sub: "sub-b", email: "b@example.com" };
const C = { sub: "sub-c", email: "c@example.com" };

async function main() {
  await new Promise((r) => google.listen(GPORT, "127.0.0.1", r));
  startRelay();
  await waitReady();
  console.log("relay up");

  console.log("sign-in checks");
  {
    const n = await api("POST", "/relay/v1/nonce");
    check("nonce shape", /^rn\.[\w-]+\.\d+\.[\w-]+$/.test(n.json.nonce) && n.json.expires > Date.now() / 1000);

    const si = (idt, client = "app") => api("POST", "/relay/v1/sign-in", { body: { id_token: idt, client } });
    const good = await si(mint({ ...A, nonce: n.json.nonce }));
    check("valid sign-in (app) returns token", good.status === 200 && good.json.relay_token?.startsWith("rt.") && good.json.email === A.email, good.text);
    check("sign-in reports no hub yet", good.json.hub_connected === false);

    const reuse = await si(mint({ ...A, nonce: n.json.nonce }));
    check("reused nonce refused", reuse.status === 401, reuse.text);

    const n2 = (await api("POST", "/relay/v1/nonce")).json.nonce;
    check("garbage nonce refused", (await si(mint({ ...A, nonce: "rn.x.9999999999.zzz" }))).status === 401);
    check("missing nonce refused", (await si(mint({ ...A }))).status === 401);
    const tampered = n2.replace(/\.[^.]+$/, ".AAAA");
    check("tampered nonce signature refused", (await si(mint({ ...A, nonce: tampered }))).status === 401);
    const n3 = (await api("POST", "/relay/v1/nonce")).json.nonce;
    check("wrong audience refused", (await si(mint({ ...A, nonce: n3, claims: { aud: "evil" } }))).status === 401);
    const now = Math.floor(Date.now() / 1000);
    check("expired token refused", (await si(mint({ ...A, nonce: n3, claims: { iat: now - 120, exp: now - 10 } }))).status === 401);
    check("old iat refused", (await si(mint({ ...A, nonce: n3, claims: { iat: now - 1200 } }))).status === 401);
    check("future iat refused", (await si(mint({ ...A, nonce: n3, claims: { iat: now + 1200 } }))).status === 401);
    check("wrong issuer refused", (await si(mint({ ...A, nonce: n3, claims: { iss: "https://evil.example" } }))).status === 401);
    check("unverified email refused", (await si(mint({ ...A, nonce: n3, claims: { email_verified: false } }))).status === 401);
    check("bad signature refused", (await si(mint({ ...A, nonce: n3, key: other.privateKey }))).status === 401);
    check("none of those burned the nonce", (await si(mint({ ...A, nonce: n3 }))).status === 200);
    check("bad client value refused", (await si(mint({ ...A, nonce: await newNonce() }), "x")).status === 400);
  }

  console.log("claim + tunnel");
  const tokA = await appSignIn(A);
  const tokB = await appSignIn(B);
  const tokC = await appSignIn(C);
  const claimA = await claim(A, "hub-a");
  const claimB = await claim(B, "hub-b");
  check("claim returns hub secret", claimA.status === 200 && claimA.json.hub_secret?.startsWith("hs.") && claimA.json.account_email === A.email, claimA.text);
  check("claim reuse of id token refused", (await api("POST", "/relay/v1/hubs/claim", { body: { hub_id: "x", id_token: mint({ ...A, nonce: "bogus" }) } })).status === 401);
  check("A and B get different secrets", claimA.json.hub_secret !== claimB.json.hub_secret);

  const hubA = new FakeHub("A");
  const hubB = new FakeHub("B");
  let refused = null;
  await new FakeHub("x").connect(claimA.json.hub_secret + "x", "hub-a").catch((e) => (refused = e.status));
  check("wrong secret refused (401)", refused === 401);
  refused = null;
  await new FakeHub("x").connect(claimA.json.hub_secret, "hub-wrong").catch((e) => (refused = e.status));
  check("wrong hub id refused (401)", refused === 401);
  await hubA.connect(claimA.json.hub_secret, "hub-a");
  await hubB.connect(claimB.json.hub_secret, "hub-b");

  const me = await api("GET", "/relay/v1/me", { headers: { "grouper-relay": tokA } });
  check("me: signed in with hub", me.json.signed_in && me.json.hub_connected && me.json.hub_id === "hub-a" && me.json.email === A.email, me.text);
  check("me without token", (await api("GET", "/relay/v1/me")).json.signed_in === false);

  console.log("routing");
  const ra = await api("GET", "/v1/who?x=1", { headers: { "grouper-relay": tokA, accept: "application/json" } });
  const rb = await api("GET", "/v1/who", { headers: { "grouper-relay": tokB } });
  check("A's token reaches hub A", ra.status === 201 && ra.json?.hub === "A" && ra.json.path === "/v1/who?x=1", ra.text);
  check("B's token reaches hub B", rb.json?.hub === "B");
  check("set-cookie headers preserved, hop-by-hop dropped", ra.headers.get("set-cookie")?.includes("a=1") && ra.headers.get("set-cookie")?.includes("b=2") && ra.headers.get("connection") !== "close");

  const hdr = Object.fromEntries(ra.json.headers);
  check("x-forwarded-proto/for added", hdr["x-forwarded-proto"] === "http" && !!hdr["x-forwarded-for"]);
  check("host is relay host", hdr.host === `127.0.0.1:${PORT}`);

  const pr = await api("POST", "/v1/echo", {
    body: { hello: "world" },
    headers: {
      "grouper-relay": tokA,
      cookie: "foo=1; grouper_relay=secretvalue; bar=2",
      "x-forwarded-for": "6.6.6.6",
      origin: "https://app.example",
      authorization: "Bearer app-token",
    },
  });
  const ph = Object.fromEntries(pr.json.headers);
  check("POST body forwarded", pr.json.body === JSON.stringify({ hello: "world" }) && pr.json.method === "POST");
  check("relay cookie stripped, others kept", ph.cookie === "foo=1; bar=2", ph.cookie);
  check("Grouper-Relay header dropped", !("grouper-relay" in ph));
  check("spoofed x-forwarded-for replaced", ph["x-forwarded-for"] !== "6.6.6.6");
  check("origin + authorization forwarded", ph.origin === "https://app.example" && ph.authorization === "Bearer app-token");
  check("no relay token in any forwarded value", !JSON.stringify(pr.json.headers).includes(tokA));

  const cut = await fetch(`${BASE}/cut`, { headers: { "grouper-relay": tokA } });
  let cutErr = false;
  let cutText = "";
  try { cutText = await cut.text(); } catch (e) { cutErr = true; cutText = String(e); }
  check("err after res never ends as a clean 200 body", cutErr || cut.status !== 200, JSON.stringify(cutText));
  const boom = await api("GET", "/boom", { headers: { "grouper-relay": tokA } });
  check("err before res is 502", boom.status === 502 && boom.json?.relay === true, boom.text);

  const ac = new AbortController();
  const hang = fetch(`${BASE}/hang`, { headers: { "grouper-relay": tokA }, signal: ac.signal }).catch(() => "aborted");
  await sleep(400);
  ac.abort();
  await hang;
  for (let i = 0; i < 20 && hubA.cancels.length === 0; i++) await sleep(100);
  // `wrangler dev`'s local proxy does not propagate client disconnects to the Worker, so this can
  // only be asserted when the platform delivers the abort. Reported as a warning otherwise.
  if (hubA.cancels.length === 1) check("client abort sends cancel to hub", true);
  else console.log("  warn client abort not propagated by wrangler dev (cancel path unverified locally)");

  console.log("unauthenticated");
  const html = await api("GET", "/", { headers: { accept: "text/html,application/xhtml+xml" } });
  check("sign-in page for html GET", html.status === 200 && html.text.includes("Sign in with Google") && html.text.includes("/relay/auth/start"));
  const noTok = await api("GET", "/v1/hub", { headers: { accept: "application/json" } });
  check("401 JSON without token", noTok.status === 401 && noTok.json?.relay === true && noTok.json.error === "sign in to the Grouper relay first");
  check("401 JSON for POST", (await api("POST", "/v1/x", { body: { a: 1 } })).status === 401);
  check("bad token is unauthenticated", (await api("GET", "/v1/x", { headers: { "grouper-relay": "rt.AAAA.BBBB" } })).status === 401);
  check("another account's secret can't forge a token", (await api("GET", "/v1/x", { headers: { "grouper-relay": tokA.slice(0, -2) + "xx" } })).status === 401);

  console.log("browser flow");
  {
    const s = await api("GET", "/relay/auth/start");
    const loc = new URL(s.headers.get("location"));
    check("auth/start redirects to Google", s.status === 302 && loc.origin === GOOGLE && loc.searchParams.get("response_type") === "id_token" && loc.searchParams.get("response_mode") === "form_post" && loc.searchParams.get("redirect_uri") === `${BASE}/relay/auth/return` && loc.searchParams.get("client_id") === CLIENT_ID);
    const nonce = loc.searchParams.get("nonce");
    const form = (idt) => fetch(`${BASE}/relay/auth/return`, { method: "POST", redirect: "manual", headers: { "content-type": "application/x-www-form-urlencoded" }, body: new URLSearchParams({ id_token: idt }) });
    const bad = await form(mint({ ...A, nonce: "nope" }));
    check("auth/return refuses a bad nonce", bad.status === 401);
    const ok = await form(mint({ ...A, nonce }));
    const sc = ok.headers.get("set-cookie") ?? "";
    check("auth/return sets cookie and redirects to /", ok.status === 303 && ok.headers.get("location") === "/" && /grouper_relay=rt\./.test(sc) && /HttpOnly/i.test(sc) && /SameSite=Lax/i.test(sc) && /Path=\//.test(sc), sc);
    const cookie = sc.split(";")[0];
    const viaCookie = await api("GET", "/", { headers: { cookie: `${cookie}; other=1`, accept: "text/html" } });
    check("cookie reaches hub A, stripped", viaCookie.json?.hub === "A" && Object.fromEntries(viaCookie.json.headers).cookie === "other=1", viaCookie.text);
    const web = await api("POST", "/relay/v1/sign-in", { body: { id_token: mint({ ...A, nonce: await newNonce() }), client: "web" } });
    check("web sign-in sets cookie, no token in body", web.status === 200 && !web.json.relay_token && /grouper_relay=/.test(web.headers.get("set-cookie") ?? ""));
  }

  console.log("client websocket");
  {
    const ws = await clientWs("/v1/events", { "grouper-relay": tokA });
    const m1 = nextMsg(ws);
    ws.send("hello");
    check("text echo via hub A", (await m1).data.toString() === "A:echo:hello");
    const m2 = nextMsg(ws);
    ws.send(Buffer.from([1, 2, 3]));
    const r2 = await m2;
    check("binary echo", r2.bin && Buffer.from(r2.data).equals(Buffer.from([1, 2, 3])));
    const wsB = await clientWs("/v1/events", { "grouper-relay": tokB });
    const m3 = nextMsg(wsB);
    wsB.send("yo");
    check("B's websocket goes to hub B", (await m3).data.toString() === "B:echo:yo");
    ws.close(1000, "done");
    for (let i = 0; i < 20 && hubA.wsClosed.length === 0; i++) await sleep(100);
    check("client close forwarded as ws_close", hubA.wsClosed.length === 1);
    wsB.close();
    let st = null;
    await clientWs("/v1/events", {}).catch((e) => (st = e.status));
    check("websocket without token refused (401)", st === 401);
  }

  console.log("no hub");
  {
    const r = await api("GET", "/v1/x", { headers: { "grouper-relay": tokC, accept: "application/json" } });
    check("503 JSON for user with no hub", r.status === 503 && r.json?.relay === true && r.json.error === "your hub isn't connected to the relay", r.text);
    const h = await api("GET", "/", { headers: { "grouper-relay": tokC, accept: "text/html" } });
    check("503 page for html navigation", h.status === 503 && h.text.includes("<html"));
    let st = null;
    await clientWs("/v1/events", { "grouper-relay": tokC }).catch((e) => (st = e.status));
    check("websocket with no hub refused (503)", st === 503);
  }

  console.log("sign-out");
  {
    const t1 = await appSignIn(A);
    const t2 = await appSignIn(A);
    const plain = await api("POST", "/relay/v1/sign-out", { body: {}, headers: { "grouper-relay": t1 } });
    check("sign-out clears cookie", plain.status === 200 && /Max-Age=0/i.test(plain.headers.get("set-cookie") ?? ""));
    check("plain sign-out leaves tokens valid", (await api("GET", "/v1/x", { headers: { "grouper-relay": t2 } })).status === 201);
    check("sign-out needs a token", (await api("POST", "/relay/v1/sign-out", { body: {} })).status === 401);
    const all = await api("POST", "/relay/v1/sign-out", { body: { everywhere: true }, headers: { "grouper-relay": t1 } });
    check("sign-out everywhere ok", all.status === 200);
    const after = await Promise.all([t1, t2, tokA].map((t) => api("GET", "/v1/x", { headers: { "grouper-relay": t } })));
    check("old tokens invalid after sign-out everywhere", after.every((r) => r.status === 401));
    check("B unaffected by A's sign-out", (await api("GET", "/v1/who", { headers: { "grouper-relay": tokB } })).json?.hub === "B");
    check("fresh sign-in works again", (await api("GET", "/v1/who", { headers: { "grouper-relay": await appSignIn(A) } })).json?.hub === "A");
  }

  console.log("re-claim");
  {
    const again = await claim(A, "hub-a2");
    check("re-claim ok with a new secret", again.status === 200 && again.json.hub_secret !== claimA.json.hub_secret);
    for (let i = 0; i < 30 && hubA.closeCode === null; i++) await sleep(100);
    check("old hub socket closed with 4001", hubA.closeCode === 4001, String(hubA.closeCode));
    let st = null;
    await new FakeHub("x").connect(claimA.json.hub_secret, "hub-a").catch((e) => (st = e.status));
    check("old secret no longer connects", st === 401);
    const t = await appSignIn(A);
    check("no hub connected right after re-claim (503)", (await api("GET", "/v1/x", { headers: { "grouper-relay": t } })).status === 503);
    const hubA2 = new FakeHub("A2");
    await hubA2.connect(again.json.hub_secret, "hub-a2");
    check("new hub serves requests", (await api("GET", "/v1/who", { headers: { "grouper-relay": t } })).json?.hub === "A2");
    // connecting the same hub again replaces the first socket
    const hubA3 = new FakeHub("A3");
    await hubA3.connect(again.json.hub_secret, "hub-a2");
    for (let i = 0; i < 30 && hubA2.closeCode === null; i++) await sleep(100);
    check("second connection replaces the first (4001)", hubA2.closeCode === 4001);
    check("replacement serves requests", (await api("GET", "/v1/who", { headers: { "grouper-relay": t } })).json?.hub === "A3");
    hubA3.close();
  }

  console.log("delete hub");
  {
    check("DELETE /hubs with wrong secret refused", (await api("DELETE", "/relay/v1/hubs", { headers: { authorization: "Bearer hs.bad" } })).status === 401);
    const d = await api("DELETE", "/relay/v1/hubs", { headers: { authorization: `Bearer ${claimB.json.hub_secret}` } });
    check("DELETE /hubs ok", d.status === 200);
    for (let i = 0; i < 30 && hubB.closeCode === null; i++) await sleep(100);
    check("hub B socket closed", hubB.closeCode !== null);
    check("B now gets 503", (await api("GET", "/v1/who", { headers: { "grouper-relay": tokB } })).status === 503);
  }
}

let tearingDown = false;
process.on("uncaughtException", (e) => {
  if (tearingDown && e?.code === "ECONNRESET") return;
  console.error(e);
  process.exit(1);
});
let crashed = false;
try {
  await main();
} catch (e) {
  crashed = true;
  console.error("test crashed:", e);
  if (relayLog) console.error("--- relay log tail ---\n" + relayLog.slice(-1500));
} finally {
  if (process.env.RELAY_TEST_VERBOSE) console.log("--- relay log ---\n" + relayLog);
  tearingDown = true;
  for (const h of allHubs) h.close();
  stopRelay();
  google.close();
  await sleep(500);
  try { rmSync(persist, { recursive: true, force: true }); } catch {}
}
console.log(`\n${passed} passed, ${failures.length} failed${crashed ? ", test crashed" : ""}`);
if (failures.length) console.log("failed:\n - " + failures.join("\n - "));
process.exit(failures.length || crashed ? 1 : 0);
