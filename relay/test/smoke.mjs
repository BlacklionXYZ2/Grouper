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
// The Google nonce claim the relay accepts for a relay nonce (domain separated).
const claimOf = (n) => b64u(crypto.createHash("sha256").update("grouper-relay-v1:" + n).digest());

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
  if (o.nonce !== undefined) claims.nonce = o.rawClaim ? o.nonce : claimOf(o.nonce);
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
    CLI_CODE_TTL: "3",
  };
  const args = [
    "wrangler", "dev", "--local", "--ip", "127.0.0.1", "--port", String(PORT), "--local-upstream", `127.0.0.1:${PORT}`,
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

async function signInBody(o, client) {
  return { id_token: mint(o), relay_nonce: o.nonce, client };
}

async function newNonce() {
  return (await api("POST", "/relay/v1/nonce")).json.nonce;
}

async function appSignIn(user) {
  const nonce = await newNonce();
  const r = await api("POST", "/relay/v1/sign-in", { body: { id_token: mint({ ...user, nonce }), relay_nonce: nonce, client: "app" } });
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
    if (f.path.startsWith("/v1/evilcookie")) {
      this.send({ t: "res", id: f.id, status: 200, headers: [["set-cookie", "grouper_relay=evil; Path=/"], ["set-cookie", "grouper_relay_state=evil"], ["set-cookie", " Grouper_Relay=evil2"], ["set-cookie", "app=ok"]] });
      return this.send({ t: "end", id: f.id });
    }
    if (f.path.startsWith("/v1/slowstream")) {
      this.send({ t: "res", id: f.id, status: 200, headers: [["content-type", "application/octet-stream"]] });
      this.send({ t: "data", id: f.id, b64: Buffer.from("first").toString("base64") });
      return; // never ends
    }
    if (f.path.startsWith("/v1/boom")) return this.send({ t: "err", id: f.id, message: "kaput" });
    if (f.path.startsWith("/v1/cut")) {
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
  const r = await api("POST", "/relay/v1/hubs/claim", { body: { hub_id: hubId, id_token: mint({ ...user, nonce }), relay_nonce: nonce } });
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

    const si = (o, relayNonce = o.nonce, client = "app") => api("POST", "/relay/v1/sign-in", { body: { id_token: mint(o), relay_nonce: relayNonce, client } });
    const good = await si({ ...A, nonce: n.json.nonce });
    check("valid sign-in (app) returns token", good.status === 200 && good.json.relay_token?.startsWith("rt.") && good.json.email === A.email, good.text);
    check("sign-in reports no hub yet", good.json.hub_connected === false);

    const reuse = await si({ ...A, nonce: n.json.nonce });
    check("reused nonce refused", reuse.status === 401, reuse.text);

    const n2 = (await api("POST", "/relay/v1/nonce")).json.nonce;
    check("garbage nonce refused", (await si({ ...A, nonce: "rn.x.9999999999.zzz" })).status === 401);
    check("missing nonce refused", (await si({ ...A })).status === 401);
    const tampered = n2.replace(/\.[^.]+$/, ".AAAA");
    check("tampered nonce signature refused", (await si({ ...A, nonce: tampered })).status === 401);
    const n3 = (await api("POST", "/relay/v1/nonce")).json.nonce;
    check("wrong audience refused", (await si({ ...A, nonce: n3, claims: { aud: "evil" } })).status === 401);
    const now = Math.floor(Date.now() / 1000);
    check("expired token refused", (await si({ ...A, nonce: n3, claims: { iat: now - 120, exp: now - 10 } })).status === 401);
    check("old iat refused", (await si({ ...A, nonce: n3, claims: { iat: now - 1200 } })).status === 401);
    check("future iat refused", (await si({ ...A, nonce: n3, claims: { iat: now + 1200 } })).status === 401);
    check("wrong issuer refused", (await si({ ...A, nonce: n3, claims: { iss: "https://evil.example" } })).status === 401);
    check("unverified email refused", (await si({ ...A, nonce: n3, claims: { email_verified: false } })).status === 401);
    check("bad signature refused", (await si({ ...A, nonce: n3, key: other.privateKey })).status === 401);
    check("none of those burned the nonce", (await si({ ...A, nonce: n3 })).status === 200);
    check("bad client value refused", (await si({ ...A, nonce: await newNonce() }, undefined, "x")).status === 400);

    // Domain separation: a token minted with the RAW relay nonce (what a malicious hub would hand a
    // victim as its "hub nonce") is refused, as is a relay_nonce that doesn't match the token.
    const nr = await newNonce();
    check("token carrying the raw relay nonce refused", (await si({ ...A, nonce: nr, rawClaim: true })).status === 401);
    const nOther = await newNonce();
    check("relay_nonce that doesn't match the token's hash refused", (await si({ ...A, nonce: nr }, nOther)).status === 401);
    check("missing relay_nonce refused", (await si({ ...A, nonce: nr }, null)).status === 401);
    check("hashed claim with the right relay_nonce still works afterwards", (await si({ ...A, nonce: nr })).status === 200);
    const nc = await newNonce();
    const rawClaim = await api("POST", "/relay/v1/hubs/claim", { body: { hub_id: "evil", id_token: mint({ ...A, nonce: nc, rawClaim: true }), relay_nonce: nc } });
    check("claim with a raw-nonce token refused", rawClaim.status === 401);
    check("hub-style hex nonce can't be replayed as a relay nonce", (await si({ ...A, nonce: "0123456789abcdef0123456789abcdef", rawClaim: true }, nr)).status === 401);

    // Content type and origin.
    const jb = async () => { const nn = await newNonce(); return JSON.stringify({ id_token: mint({ ...A, nonce: nn }), relay_nonce: nn, client: "app" }); };
    const raw = (headers, body) => fetch(`${BASE}/relay/v1/sign-in`, { method: "POST", headers, body });
    check("form-encoded sign-in refused (415)", (await raw({ "content-type": "application/x-www-form-urlencoded" }, await jb())).status === 415);
    check("text/plain sign-in refused (415)", (await raw({ "content-type": "text/plain" }, await jb())).status === 415);
    check("no content-type refused (415)", (await raw({}, await jb())).status === 415);
    check("cross-origin sign-in refused (403)", (await raw({ "content-type": "application/json", origin: "https://evil.example" }, await jb())).status === 403);
    check("same-origin Origin accepted", (await raw({ "content-type": "application/json", origin: BASE }, await jb())).status === 200);
    const nn2 = await newNonce();
    check("cross-origin claim refused (403)", (await fetch(`${BASE}/relay/v1/hubs/claim`, { method: "POST", headers: { "content-type": "application/json", origin: "https://evil.example" }, body: JSON.stringify({ hub_id: "h", id_token: mint({ ...A, nonce: nn2 }), relay_nonce: nn2 }) })).status === 403);
    check("form-encoded claim refused (415)", (await fetch(`${BASE}/relay/v1/hubs/claim`, { method: "POST", headers: { "content-type": "application/x-www-form-urlencoded" }, body: "x=1" })).status === 415);
    check("form-encoded sign-out refused (415)", (await fetch(`${BASE}/relay/v1/sign-out`, { method: "POST", headers: { "content-type": "text/plain", "grouper-relay": await appSignIn(A) }, body: "{}" })).status === 415);
    check("cross-origin sign-out refused (403)", (await fetch(`${BASE}/relay/v1/sign-out`, { method: "POST", headers: { "content-type": "application/json", origin: "https://evil.example", "grouper-relay": await appSignIn(A) }, body: "{}" })).status === 403);
  }

  console.log("claim + tunnel");
  const tokA = await appSignIn(A);
  const tokB = await appSignIn(B);
  const tokC = await appSignIn(C);
  const claimA = await claim(A, "hub-a");
  const claimB = await claim(B, "hub-b");
  check("claim returns hub secret", claimA.status === 200 && claimA.json.hub_secret?.startsWith("hs.") && claimA.json.account_email === A.email, claimA.text);
  check("claim reuse of id token refused", (await api("POST", "/relay/v1/hubs/claim", { body: { hub_id: "x", id_token: mint({ ...A, nonce: "bogus" }), relay_nonce: "bogus" } })).status === 401);
  check("A and B get different secrets", claimA.json.hub_secret !== claimB.json.hub_secret);

  const hubA = new FakeHub("A");
  const hubB = new FakeHub("B");
  let refused = null;
  await new FakeHub("x").connect(claimA.json.hub_secret + "x", "hub-a").catch((e) => (refused = e.status));
  check("wrong secret refused (401)", refused === 401);
  refused = null;
  await new FakeHub("x").connect(claimA.json.hub_secret, "hub-wrong").catch((e) => (refused = e.status));
  check("wrong hub id refused (401)", refused === 401);
  {
    const parts = claimA.json.hub_secret.split(".");
    check("hub secret has acct, rand and tag", parts.length === 4 && parts[0] === "hs");
    const forgedTag = [...parts.slice(0, 3), "A".repeat(parts[3].length)].join(".");
    const noTag = parts.slice(0, 3).join(".");
    const otherAcct = ["hs", "0".repeat(32), parts[2], parts[3]].join(".");
    for (const [label, sec] of [["forged tag", forgedTag], ["missing tag", noTag], ["tag from another account", otherAcct]]) {
      let st = null;
      await new FakeHub("x").connect(sec, "hub-a").catch((e) => (st = e.status));
      check(`hub secret with ${label} refused (401)`, st === 401);
      check(`DELETE /hubs with ${label} refused (401)`, (await api("DELETE", "/relay/v1/hubs", { headers: { authorization: `Bearer ${sec}` } })).status === 401);
    }
  }
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
  check("host is relay host", hdr.host === `127.0.0.1:${PORT}`, hdr.host);

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

  const cut = await fetch(`${BASE}/v1/cut`, { headers: { "grouper-relay": tokA } });
  let cutErr = false;
  let cutText = "";
  try { cutText = await cut.text(); } catch (e) { cutErr = true; cutText = String(e); }
  check("err after res never ends as a clean 200 body", cutErr || cut.status !== 200, JSON.stringify(cutText));
  const ec = await fetch(`${BASE}/v1/evilcookie`, { headers: { "grouper-relay": tokA } });
  const ecCookies = ec.headers.getSetCookie();
  check("hub Set-Cookie for relay cookies stripped, others kept", ecCookies.length === 1 && ecCookies[0] === "app=ok", JSON.stringify(ecCookies));
  const boom = await api("GET", "/v1/boom", { headers: { "grouper-relay": tokA } });
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
  const HTML = { accept: "text/html,application/xhtml+xml" };
  const html = await api("GET", "/relay/sign-in", { headers: HTML });
  check("sign-in page", html.status === 200 && html.text.includes("Sign in with Google") && html.text.includes('href="/relay/auth/start"') && html.text.includes("/site/style.css"));
  const htmlNext = await api("GET", "/relay/sign-in?next=%2Fapp%2Fchat%3Fx%3D1", { headers: HTML });
  check("sign-in page carries a safe next", htmlNext.text.includes("/relay/auth/start?next=%2Fapp%2Fchat%3Fx%3D1"), htmlNext.text);
  for (const evil of ["https://evil.example/", "//evil.example/x", "/\\evil.example", "javascript:alert(1)", "/app\r\nSet-Cookie: x=1"]) {
    const r = await api("GET", "/relay/sign-in?next=" + encodeURIComponent(evil), { headers: HTML });
    check(`sign-in page drops open-redirect next ${JSON.stringify(evil)}`, r.status === 200 && !r.text.includes("next=") && r.text.includes('href="/relay/auth/start"'), r.text);
    const s = await api("GET", "/relay/auth/start?next=" + encodeURIComponent(evil));
    const ck = s.headers.getSetCookie().find((c) => c.startsWith("grouper_relay_state=")) ?? "";
    check(`auth/start drops open-redirect next ${JSON.stringify(evil)}`, s.status === 302 && !ck.includes("~"), ck);
  }
  check("hub path without session redirects html to sign-in", await (async () => {
    const r = await api("GET", "/app", { headers: HTML });
    return r.status === 302 && r.headers.get("location") === "/relay/sign-in?next=%2Fapp";
  })());
  check("deep hub path keeps its query in next", (await api("GET", "/app/x?y=1", { headers: HTML })).headers.get("location") === "/relay/sign-in?next=%2Fapp%2Fx%3Fy%3D1");
  for (const p of ["/sw.js", "/favicon.ico", "/healthz"]) {
    check(`${p} without session redirects html`, (await api("GET", p, { headers: HTML })).headers.get("location") === "/relay/sign-in?next=" + encodeURIComponent(p));
  }
  check("hub API without session is 401 JSON, not a redirect", (await api("GET", "/v1/hub", { headers: { accept: "application/json" } })).status === 401);
  check("unknown /relay path is 404 JSON", (await api("GET", "/relay/nope")).status === 404);

  console.log("website");
  {
    const home = await api("GET", "/", { headers: HTML });
    check("/ is the website, no sign-in", home.status === 200 && home.text.includes("Open Grouper") && home.text.includes('href="/download"') && home.text.includes("github.com/BlacklionXYZ2/Grouper"), home.text.slice(0, 200));
    const dl = await api("GET", "/download", { headers: HTML });
    check("/download", dl.status === 200 && dl.text.includes("harness-latest"));
    const pv = await api("GET", "/privacy", { headers: HTML });
    check("/privacy", pv.status === 200 && pv.text.includes("What the relay sees"));
    const css = await api("GET", "/site/style.css");
    check("/site/style.css served as css", css.status === 200 && /text\/css/.test(css.headers.get("content-type") ?? ""));
    check("/site/icon.svg", (await api("GET", "/site/icon.svg")).status === 200);
    check("website works without Accept: text/html", (await api("GET", "/")).status === 200);
    const nf = await api("GET", "/no-such-page", { headers: HTML });
    check("unclaimed path is the site's 404 page", nf.status === 404 && nf.text.includes("Page not found"), `${nf.status} ${nf.text.slice(0, 100)}`);
    check("website refuses POST", (await api("POST", "/privacy", { body: { a: 1 } })).status === 405);
  }
  const noTok = await api("GET", "/v1/hub", { headers: { accept: "application/json" } });
  check("401 JSON without token", noTok.status === 401 && noTok.json?.relay === true && noTok.json.error === "sign in to the Grouper relay first");
  check("401 JSON for POST", (await api("POST", "/v1/x", { body: { a: 1 } })).status === 401);
  check("bad token is unauthenticated", (await api("GET", "/v1/x", { headers: { "grouper-relay": "rt.AAAA.BBBB" } })).status === 401);
  check("another account's secret can't forge a token", (await api("GET", "/v1/x", { headers: { "grouper-relay": tokA.slice(0, -2) + "xx" } })).status === 401);

  console.log("browser flow");
  {
    const s = await api("GET", "/relay/auth/start");
    const loc = new URL(s.headers.get("location"));
    check("auth/start redirects to Google", s.status === 302 && loc.origin === GOOGLE && loc.searchParams.get("response_type") === "id_token" && loc.searchParams.get("response_mode") === "form_post" && loc.searchParams.get("redirect_uri") === `${BASE}/oauth/google/return` && loc.searchParams.get("client_id") === CLIENT_ID, loc.toString());
    const claimNonce = loc.searchParams.get("nonce");
    const state = loc.searchParams.get("state");
    const stateSet = s.headers.getSetCookie().find((c) => c.startsWith("grouper_relay_state=")) ?? "";
    const stateCookie = stateSet.split(";")[0];
    check("auth/start sets a short-lived HttpOnly state cookie", /HttpOnly/i.test(stateSet) && /Max-Age=600/.test(stateSet) && stateCookie.includes(state), stateSet);
    check("auth/start's Google nonce is the hashed form (not an rn. string)", !!claimNonce && !claimNonce.startsWith("rn.") && /^[\w-]{43}$/.test(claimNonce));
    const relayNonceInCookie = stateCookie.slice(stateCookie.indexOf(".") + 1);
    check("hashed nonce matches the cookie's relay nonce", claimOf(relayNonceInCookie) === claimNonce);
    const form = (idt, { st = state, ck = stateCookie } = {}) => fetch(`${BASE}/relay/auth/return`, { method: "POST", redirect: "manual", headers: { "content-type": "application/x-www-form-urlencoded", ...(ck ? { cookie: ck } : {}) }, body: new URLSearchParams({ id_token: idt, ...(st ? { state: st } : {}) }) });
    const goodTok = () => mint({ ...A, nonce: relayNonceInCookie });
    check("auth/return without the state cookie refused", (await form(goodTok(), { ck: "" })).status === 401);
    check("auth/return with a wrong state cookie refused", (await form(goodTok(), { ck: "grouper_relay_state=zzz." + relayNonceInCookie })).status === 401);
    check("auth/return with a wrong posted state refused", (await form(goodTok(), { st: "nope" })).status === 401);
    check("auth/return without posted state refused", (await form(goodTok(), { st: "" })).status === 401);
    const swapped = await form(mint({ ...A, nonce: relayNonceInCookie, rawClaim: true }));
    check("auth/return with a raw-nonce token refused", swapped.status === 401);
    const other = (await api("GET", "/relay/auth/start")).headers.getSetCookie()[0].split(";")[0];
    check("auth/return with another flow's cookie refused", (await form(goodTok(), { ck: other })).status === 401);
    const bad = await form(mint({ ...A, nonce: "nope" }));
    check("auth/return refuses a bad nonce", bad.status === 401);
    const ok = await form(goodTok());
    const cks = ok.headers.getSetCookie();
    const sc = cks.find((c) => c.startsWith("grouper_relay=")) ?? "";
    check("auth/return clears the state cookie", cks.some((c) => c.startsWith("grouper_relay_state=;") && /Max-Age=0/.test(c)));
    check("state/nonce are single use", (await form(goodTok())).status === 401);
    check("auth/return sets cookie and redirects to /", ok.status === 303 && ok.headers.get("location") === "/app" && /grouper_relay=rt\./.test(sc) && /HttpOnly/i.test(sc) && /SameSite=Lax/i.test(sc) && /Path=\//.test(sc), sc);
    const cookie = sc.split(";")[0];
    const viaCookie = await api("GET", "/app", { headers: { cookie: `${cookie}; other=1`, accept: "text/html" } });
    check("cookie reaches hub A, stripped", viaCookie.json?.hub === "A" && Object.fromEntries(viaCookie.json.headers).cookie === "other=1", viaCookie.text);
    const web = await api("POST", "/relay/v1/sign-in", { body: await signInBody({ ...A, nonce: await newNonce() }, "web") });
    check("web sign-in sets cookie, no token in body", web.status === 200 && !web.json.relay_token && /grouper_relay=/.test(web.headers.get("set-cookie") ?? ""));
  }

  console.log("browser flow: /oauth/google/return, next");
  {
    const start = async (next) => {
      const s = await api("GET", "/relay/auth/start" + (next ? "?next=" + encodeURIComponent(next) : ""));
      const loc = new URL(s.headers.get("location"));
      const cookie = (s.headers.getSetCookie().find((c) => c.startsWith("grouper_relay_state=")) ?? "").split(";")[0];
      const set = s.headers.getSetCookie().find((c) => c.startsWith("grouper_relay_state=")) ?? "";
      const rn = cookie.slice(cookie.indexOf(".") + 1).split("~")[0];
      return { state: loc.searchParams.get("state"), cookie, rn, set };
    };
    const post = (f, cookie, extra = {}) => fetch(`${BASE}/oauth/google/return`, { method: "POST", redirect: "manual", headers: { "content-type": "application/x-www-form-urlencoded", ...(cookie ? { cookie } : {}) }, body: new URLSearchParams({ ...extra, ...f }) });
    const a = await start("/app/threads?open=2");
    check("state cookie has Path=/ so it reaches /oauth/google/return", /Path=\/;/.test(a.set), a.set);
    const wrong = await post({ id_token: mint({ ...A, nonce: a.rn }), state: "nope" }, a.cookie);
    check("return with a mismatched state isn't the relay's: no sign-in, no relay cookie (the hub return page refuses the unknown state)", wrong.status === 400 && !(wrong.headers.get("set-cookie") || "").includes("grouper_relay="));
    const ok = await post({ id_token: mint({ ...A, nonce: a.rn }), state: a.state }, a.cookie);
    const sc = ok.headers.getSetCookie().find((c) => c.startsWith("grouper_relay=")) ?? "";
    check("POST /oauth/google/return with matching state signs in and redirects to next", ok.status === 303 && ok.headers.get("location") === "/app/threads?open=2" && /grouper_relay=rt\./.test(sc), `${ok.status} ${ok.headers.get("location")}`);
    const replay = await post({ id_token: mint({ ...A, nonce: a.rn }), state: a.state }, a.cookie);
    check("replaying it fails", replay.status !== 303);
    const b = await start("/app");
    const okB = await post({ id_token: mint({ ...A, nonce: b.rn }), state: b.state }, b.cookie);
    check("default next is /app", okB.status === 303 && okB.headers.get("location") === "/app");
    // Tampered next in the cookie is validated again.
    const c = await start();
    const evilCookie = c.cookie + "~" + Buffer.from("https://evil.example/").toString("base64url");
    const okC = await post({ id_token: mint({ ...A, nonce: c.rn }), state: c.state }, evilCookie);
    check("a tampered next in the cookie falls back to /app", okC.status === 303 && okC.headers.get("location") === "/app", okC.headers.get("location"));
    // Without a relay state cookie it's a hub's own sign-in: the relay serves the shared return
    // page (no hub, no session needed), which posts on to the hub named in the state.
    const hubState = (o) => Buffer.from(JSON.stringify({ o, id: "abc123" })).toString("base64url");
    const local = await post({ id_token: "tok.en", state: hubState("http://127.0.0.1:47600") }, "");
    const lt = await local.text();
    check("a hub's return (no relay session) gets the return page", local.status === 200 && lt.includes('action="http://127.0.0.1:47600/v1/auth/google/return"') && lt.includes('name="id_token" value="tok.en"'), `${local.status}`);
    check("…with a CSP that only lets the form post to that hub", (local.headers.get("content-security-policy") || "").includes("form-action http://127.0.0.1:47600"));
    check("…and it sets no relay cookie", !(local.headers.get("set-cookie") || "").includes("grouper_relay"));
    const viaRelay = await post({ id_token: "x", state: hubState(BASE) }, "");
    check("a hub reached through the relay is returned to the relay's origin", viaRelay.status === 200 && (await viaRelay.text()).includes(`action="${BASE}/v1/auth/google/return"`));
    for (const [label, st] of [["plain http to a remote host", hubState("http://evil.example")], ["a path, not an origin", hubState("https://hub.example/x")], ["garbage", "not-a-state"]]) {
      const r = await post({ id_token: "x", state: st }, "");
      check(`return page refuses ${label}`, r.status === 400);
    }
    const getErr = await fetch(`${BASE}/oauth/google/return?error=access_denied&state=${hubState("https://hub.example")}`, { redirect: "manual" });
    check("a GET with Google's error is passed on too", getErr.status === 200 && (await getErr.text()).includes('name="error" value="access_denied"'));
    // Forwarded hub requests never carry the relay state cookie.
    const viaState = await api("GET", "/app", { headers: { "grouper-relay": tokA, cookie: "grouper_relay_state=abc.def; other=1", accept: "text/html" } });
    check("relay state cookie is stripped before the hub", Object.fromEntries(viaState.json?.headers ?? []).cookie === "other=1", viaState.text);
    // After sign-in the redirect target works with the cookie.
    const hubPage = await api("GET", "/app/threads?open=2", { headers: { cookie: sc.split(";")[0], accept: "text/html" } });
    check("the next path reaches the hub with the new cookie", hubPage.json?.hub === "A" && hubPage.json.path.startsWith("/app/threads"));
    check("hub paths with a session are forwarded (/sw.js, /v1, /healthz)", (await Promise.all(["/sw.js", "/v1/x", "/healthz", "/favicon.ico"].map((p) => api("GET", p, { headers: { "grouper-relay": tokA } })))).every((r) => r.json?.hub === "A"));
  }

  console.log("command-line sign-in");
  {
    const STATE = "abcdef0123456789abcdef0123456789";
    const PORT_CLI = 41234;
    const path = (port = PORT_CLI, state = STATE) => `/relay/v1/cli-token?port=${port}&state=${state}`;
    const web = await api("POST", "/relay/v1/sign-in", { body: await signInBody({ ...A, nonce: await newNonce() }, "web") });
    const ck = (web.headers.get("set-cookie") ?? "").split(";")[0];
    const form = (fields, { cookie = ck, origin } = {}) =>
      fetch(BASE + "/relay/v1/cli-token", { method: "POST", redirect: "manual", headers: { "content-type": "application/x-www-form-urlencoded", ...(cookie ? { cookie } : {}), ...(origin ? { origin } : {}) }, body: new URLSearchParams(fields) });
    const confirm = async (port, state) => {
      const r = await api("GET", path(port, state), { headers: { cookie: ck, accept: "text/html" } });
      return { r, csrf: /name="csrf" value="([^"]+)"/.exec(r.text)?.[1] };
    };
    const exchange = (code, state, extra = {}) => api("POST", "/relay/v1/cli-token/exchange", { body: { code, state }, ...extra });
    const codeFrom = (res) => new URL(res.headers.get("location")).searchParams.get("code");

    const noSess = await api("GET", path(), { headers: { accept: "text/html" } });
    check("no session: redirect to sign-in with next = this path and query", noSess.status === 302 && noSess.headers.get("location") === "/relay/sign-in?next=" + encodeURIComponent(path()), noSess.headers.get("location"));
    check("no session, JSON client: 401", (await api("GET", path(), { headers: { accept: "application/json" } })).status === 401);
    const signInNext = await api("GET", "/relay/sign-in?next=" + encodeURIComponent(path()), { headers: { accept: "text/html" } });
    check("the sign-in page carries the cli-token next", signInNext.text.includes(encodeURIComponent(path())), signInNext.text.slice(-400));

    const { r: page, csrf } = await confirm(PORT_CLI, STATE);
    check("GET with a session shows the confirm page with the email and a POST form", page.status === 200 && page.text.includes("a@example.com") && page.text.includes('method="post"') && !!csrf && /frame-ancestors/.test(page.headers.get("content-security-policy") ?? ""), page.text.slice(-500));
    check("GET never mints (no redirect to loopback, no code)", !page.headers.get("location") && !/code=/.test(page.text));
    check("bad port refused (80)", (await api("GET", path(80), { headers: { cookie: ck } })).status === 400);
    check("bad port refused (70000)", (await api("GET", path(70000), { headers: { cookie: ck } })).status === 400);
    check("bad state refused (short)", (await api("GET", path(PORT_CLI, "abc"), { headers: { cookie: ck } })).status === 400);
    check("bad state refused (chars)", (await api("GET", `/relay/v1/cli-token?port=${PORT_CLI}&state=${encodeURIComponent("a".repeat(15) + "<")}`, { headers: { cookie: ck } })).status === 400);

    // POST refusals
    check("POST without csrf refused", (await form({ port: PORT_CLI, state: STATE })).status === 403);
    check("POST with a wrong csrf refused", (await form({ port: PORT_CLI, state: STATE, csrf: csrf.slice(0, -3) + "AAA" })).status === 403);
    check("csrf is bound to the port", (await form({ port: PORT_CLI + 1, state: STATE, csrf })).status === 403);
    check("csrf is bound to the state", (await form({ port: PORT_CLI, state: STATE.replace("a", "b"), csrf })).status === 403);
    check("POST cross-origin refused", (await form({ port: PORT_CLI, state: STATE, csrf }, { origin: "https://evil.example" })).status === 403);
    check("POST without a session refused", (await form({ port: PORT_CLI, state: STATE, csrf }, { cookie: "" })).status === 401);
    const other = await appSignIn({ sub: "sub-other", email: "o@example.com" });
    const otherCsrf = (await fetch(BASE + "/relay/v1/cli-token", { method: "POST", redirect: "manual", headers: { "content-type": "application/x-www-form-urlencoded", "grouper-relay": other }, body: new URLSearchParams({ port: PORT_CLI, state: STATE, csrf }) })).status;
    check("another account's session can't use this csrf", otherCsrf === 403);
    check("POST with JSON content type refused (415)", (await api("POST", "/relay/v1/cli-token", { body: { port: PORT_CLI }, headers: { cookie: ck } })).status === 415);
    for (const bad of ["http://evil.example:41234", "localhost"]) {
      const r = await form({ port: bad, state: STATE, csrf });
      check(`non-numeric port ${bad} refused`, r.status === 400 && !r.headers.get("location"));
    }

    // Happy path
    const ok = await form({ port: PORT_CLI, state: STATE, csrf });
    const loc = ok.headers.get("location") ?? "";
    const code = ok.status === 303 ? codeFrom(ok) : null;
    check("POST mints a code and redirects to 127.0.0.1 loopback with code and state", ok.status === 303 && loc.startsWith(`http://127.0.0.1:${PORT_CLI}/?code=cc.`) && new URL(loc).searchParams.get("state") === STATE && new URL(loc).hostname === "127.0.0.1", loc);
    const ex = await exchange(code, STATE);
    const claims = ex.json?.relay_token ? JSON.parse(Buffer.from(ex.json.relay_token.split(".")[1], "base64url").toString()) : {};
    check("exchange returns an app relay token (365 d)", ex.status === 200 && ex.json.relay_token?.startsWith("rt.") && claims.k === "app" && claims.e === "a@example.com" && Math.abs(ex.json.expires - (Math.floor(Date.now() / 1000) + 365 * 86400)) < 120, ex.text);
    check("the token works at the relay", (await api("GET", "/relay/v1/me", { headers: { "grouper-relay": ex.json.relay_token } })).json?.signed_in === true);
    check("code is single use", (await exchange(code, STATE)).status === 401);

    // Wrong state burns the code
    const c2 = codeFrom(await form({ port: PORT_CLI, state: STATE, csrf }));
    check("wrong state refused", (await exchange(c2, "z".repeat(32))).status === 401);
    check("…and the code is burned", (await exchange(c2, STATE)).status === 401);

    // Expired
    const c3 = codeFrom(await form({ port: PORT_CLI, state: STATE, csrf }));
    await sleep(3500);
    check("expired code refused", (await exchange(c3, STATE)).status === 401);

    // Forged / malformed / request checks
    check("forged code refused", (await exchange("cc." + claims.a + ".AAAA.BBBB", STATE)).status === 401);
    check("garbage code refused", (await exchange("nope", STATE)).status === 401);
    check("missing state refused", (await api("POST", "/relay/v1/cli-token/exchange", { body: { code } })).status === 400);
    const raw = (headers, body) => fetch(`${BASE}/relay/v1/cli-token/exchange`, { method: "POST", headers, body });
    check("exchange needs JSON content type (415)", (await raw({ "content-type": "text/plain" }, JSON.stringify({ code, state: STATE }))).status === 415);
    check("exchange from another origin refused (403)", (await raw({ "content-type": "application/json", origin: "https://evil.example" }, JSON.stringify({ code, state: STATE }))).status === 403);
    check("exchange with no Origin (the CLI) is accepted by the origin check", (await raw({ "content-type": "application/json" }, JSON.stringify({ code, state: STATE }))).status === 401);
    check("exchange GET refused (405)", (await api("GET", "/relay/v1/cli-token/exchange")).status === 405);

    // Sign-out everywhere invalidates the minted token (separate account so A keeps its tokens)
    const web2 = await api("POST", "/relay/v1/sign-in", { body: await signInBody({ sub: "sub-cli2", email: "c2@example.com", nonce: await newNonce() }, "web") });
    const ck2 = (web2.headers.get("set-cookie") ?? "").split(";")[0];
    const csrf2 = /name="csrf" value="([^"]+)"/.exec((await api("GET", path(), { headers: { cookie: ck2, accept: "text/html" } })).text)?.[1];
    const c4 = codeFrom(await form({ port: PORT_CLI, state: STATE, csrf: csrf2 }, { cookie: ck2 }));
    const t4 = (await exchange(c4, STATE)).json.relay_token;
    await api("POST", "/relay/v1/sign-out", { body: { everywhere: true }, headers: { "grouper-relay": t4 } });
    check("a CLI token is revoked by sign-out everywhere", (await api("GET", "/relay/v1/me", { headers: { "grouper-relay": t4 } })).json?.signed_in === false);
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
    const h = await api("GET", "/app", { headers: { "grouper-relay": tokC, accept: "text/html" } });
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
    const wsLive = await clientWs("/v1/events", { "grouper-relay": t2 });
    const wsLiveClosed = closed(wsLive);
    const streaming = await fetch(`${BASE}/v1/slowstream`, { headers: { "grouper-relay": t2 } });
    const reader = streaming.body.getReader();
    const firstChunk = await reader.read();
    check("streaming response started", !firstChunk.done);
    const streamEnd = reader.read().then((r) => (r.done ? "clean" : "data"), () => "aborted");
    const all = await api("POST", "/relay/v1/sign-out", { body: { everywhere: true }, headers: { "grouper-relay": t1 } });
    const t0 = Date.now();
    const wsCode = await Promise.race([wsLiveClosed, sleep(60000).then(() => "timeout")]);
    check("sign-out everywhere closes client sockets with 4401", wsCode === 4401, `${wsCode} after ${Date.now() - t0}ms`);
    check("sign-out everywhere aborts in-flight streams", (await Promise.race([streamEnd, sleep(5000).then(() => "timeout")])) === "aborted");
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
