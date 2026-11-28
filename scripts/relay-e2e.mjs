// End-to-end test of the Grouper relay with fake Google accounts. Nothing touches real Google
// or Cloudflare: a fake Google (JWKS + sign-in page) runs here, the relay runs in `wrangler dev`,
// and two real hubs (target/release/grouper-hub) run with their own owners.
//
//   cargo build -p hub --release
//   (cd relay && npm install)
//   node scripts/relay-e2e.mjs            # runs the checks, then stops everything
//   node scripts/relay-e2e.mjs --keep     # leaves everything running for a browser test
//
// Users: alice (owns hub A), bob (owns hub B), carol (no hub).
import { spawn, spawnSync } from "node:child_process";
import { createServer } from "node:http";
import { generateKeyPairSync, createSign, randomBytes, createHash } from "node:crypto";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");
const KEEP = process.argv.includes("--keep");
const GOOGLE_PORT = Number(process.env.RELAY_E2E_GOOGLE_PORT ?? 8790), RELAY_PORT = Number(process.env.RELAY_E2E_PORT ?? 8799);
const GOOGLE = `http://127.0.0.1:${GOOGLE_PORT}`, RELAY = `http://127.0.0.1:${RELAY_PORT}`;
const IOS = "test-ios.apps.googleusercontent.com", WEB = "test-web.apps.googleusercontent.com";
const HUB_BIN = join(ROOT, "target", "release", process.platform === "win32" ? "grouper-hub.exe" : "grouper-hub");
const USERS = {
  alice: { sub: "100000000000000000001", email: "alice@example.test", name: "Alice Test" },
  bob: { sub: "100000000000000000002", email: "bob@example.test", name: "Bob Test" },
  carol: { sub: "100000000000000000003", email: "carol@example.test", name: "Carol Test" },
};

// ------------------------------------------------------------------ fake Google
const { privateKey, publicKey } = generateKeyPairSync("rsa", { modulusLength: 2048 });
const KID = "fake-" + randomBytes(4).toString("hex");
const jwk = { ...publicKey.export({ format: "jwk" }), kid: KID, alg: "RS256", use: "sig" };
const b64u = (b) => Buffer.from(b).toString("base64url");
// Relay flows put the domain-separated hash of the relay nonce in the Google `nonce` claim.
const relayClaim = (n) => b64u(createHash("sha256").update("grouper-relay-v1:" + n).digest());
function mint(user, { aud = IOS, nonce, relay = false, iatOffset = 0, expOffset = 3600, iss = "https://accounts.google.com" } = {}) {
  if (relay && nonce) nonce = relayClaim(nonce);
  const u = USERS[user], now = Math.floor(Date.now() / 1000) + iatOffset;
  const head = b64u(JSON.stringify({ alg: "RS256", kid: KID, typ: "JWT" }));
  const body = b64u(JSON.stringify({ iss, aud, sub: u.sub, email: u.email, email_verified: true, name: u.name, iat: now, exp: now + expOffset, ...(nonce ? { nonce } : {}) }));
  const sig = createSign("RSA-SHA256").update(`${head}.${body}`).sign(privateKey);
  return `${head}.${body}.${b64u(sig)}`;
}
const google = createServer((req, res) => {
  const url = new URL(req.url, GOOGLE);
  if (url.pathname === "/jwks") {
    res.writeHead(200, { "content-type": "application/json", "cache-control": "max-age=300" });
    return res.end(JSON.stringify({ keys: [jwk] }));
  }
  if (url.pathname === "/authorize") {
    // Browser sign-in: pick a test user, then form_post the ID token back like Google does.
    const p = url.searchParams, user = p.get("user");
    if (!user) {
      const links = Object.keys(USERS).map((u) => {
        const q = new URLSearchParams(p); q.set("user", u);
        return `<p><a href="/authorize?${q}" style="font:16px system-ui">Continue as ${USERS[u].name} (${USERS[u].email})</a></p>`;
      }).join("");
      res.writeHead(200, { "content-type": "text/html" });
      return res.end(`<!doctype html><title>Fake Google</title><h1 style="font:20px system-ui">Fake Google sign-in (test only)</h1>${links}`);
    }
    const token = mint(user, { aud: p.get("client_id"), nonce: p.get("nonce") });
    const esc = (s) => String(s ?? "").replace(/[&<>"]/g, (c) => `&#${c.charCodeAt(0)};`);
    res.writeHead(200, { "content-type": "text/html" });
    return res.end(`<!doctype html><form method="post" action="${esc(p.get("redirect_uri"))}">
<input type="hidden" name="id_token" value="${esc(token)}"><input type="hidden" name="state" value="${esc(p.get("state"))}">
</form><script>document.forms[0].submit()</script>`);
  }
  res.writeHead(404).end();
});

// ------------------------------------------------------------------ processes
const procs = [];
const tmp = mkdtempSync(join(tmpdir(), "grouper-relay-e2e-"));
function start(name, cmd, args, opts) {
  const p = spawn(cmd, args, { ...opts, shell: process.platform === "win32" && cmd === "npx", stdio: ["ignore", "pipe", "pipe"] });
  let log = "";
  p.stdout.on("data", (d) => (log += d)); p.stderr.on("data", (d) => (log += d));
  p.logs = () => log; p.label = name;
  procs.push(p);
  return p;
}
function stopAll() {
  for (const p of procs) {
    if (process.platform === "win32") spawnSync("taskkill", ["/pid", String(p.pid), "/t", "/f"], { stdio: "ignore" });
    else p.kill();
  }
  google.close();
}
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
async function waitFor(what, fn, ms = 60000) {
  const end = Date.now() + ms;
  for (;;) {
    try { const v = await fn(); if (v) return v; } catch {}
    if (Date.now() > end) throw new Error(`timed out waiting for ${what}`);
    await sleep(300);
  }
}

const hubEnv = (extra) => ({
  ...process.env,
  GROUPER_GOOGLE_JWKS_URL: `${GOOGLE}/jwks`,
  GROUPER_GOOGLE_AUTH_URL: `${GOOGLE}/authorize`,
  GROUPER_GOOGLE_CLIENT_IDS: `${IOS},${WEB}`,
  GROUPER_GOOGLE_WEB_CLIENT_ID: WEB,
  GROUPER_GOOGLE_RETURN_URL: `${RELAY}/oauth/google/return`,
  RUST_LOG: "info",
  ...extra,
});

// ------------------------------------------------------------------ checks
let failures = 0, passes = 0;
function check(name, ok, detail = "") {
  if (ok) { passes++; console.log(`  ok   ${name}`); }
  else { failures++; console.log(`  FAIL ${name}${detail ? `: ${detail}` : ""}`); }
}
async function http(base, path, { method = "GET", headers = {}, body, raw } = {}) {
  const r = await fetch(base + path, { method, headers: { ...(body ? { "content-type": "application/json" } : {}), ...headers }, body: body ? JSON.stringify(body) : raw, redirect: "manual" });
  const text = await r.text();
  let json; try { json = JSON.parse(text); } catch {}
  return { status: r.status, json, text, headers: r.headers };
}
const relayNonce = async () => (await http(RELAY, "/relay/v1/nonce", { method: "POST" })).json.nonce;
async function relayBody(user, client, o = {}) {
  const n = o.nonce ?? await relayNonce();
  return { id_token: mint(user, { ...o, nonce: n, relay: true }), relay_nonce: n, client };
}
async function relaySignIn(user, client = "app") {
  const r = await http(RELAY, "/relay/v1/sign-in", { method: "POST", body: await relayBody(user, client) });
  return r;
}

async function setupHub(label, owner, port) {
  const data = join(tmp, label);
  const hub = start(`hub ${label}`, HUB_BIN, ["serve", "--bind", `127.0.0.1:${port}`, "--data", data], { env: hubEnv() });
  const base = `http://127.0.0.1:${port}`;
  await waitFor(`hub ${label}`, async () => (await fetch(base + "/healthz")).ok);
  // The hub logs a setup code on first start; `grouper-hub setup-code` is the fallback.
  const fromLog = () => hub.logs().match(/claim\?code=([A-Z0-9-]+)/)?.[1];
  const code = (await waitFor("setup code in log", async () => fromLog(), 5000).catch(() => null))
    ?? spawnSync(HUB_BIN, ["setup-code", "--data", data], { env: hubEnv(), encoding: "utf8" }).stdout.match(/Setup code[^:]*:\s*([A-Z0-9-]+)/)?.[1];
  const nonce = (await http(base, "/v1/auth/google/nonce", { method: "POST" })).json.nonce;
  const claim = await http(base, "/v1/auth/google", { method: "POST", body: { id_token: mint(owner, { nonce }), setup_code: code, client: "cli", device_name: `${label} admin` } });
  if (!claim.json?.token) throw new Error(`claiming hub ${label} failed: ${claim.status} ${claim.text} (setup code ${code ? "found" : "missing"})`);
  const admin = { authorization: `Bearer ${claim.json.token}` };
  const hubId = (await http(base, "/v1/hub")).json.hub_id;
  await http(base, "/v1/relay", { method: "PUT", headers: admin, body: { url: RELAY } });
  const rn = (await http(base, "/v1/relay/nonce", { method: "POST", headers: admin })).json?.nonce;
  const link = await http(base, "/v1/relay/link", { method: "POST", headers: admin, body: { id_token: mint(owner, { nonce: rn, relay: true }), relay_nonce: rn } });
  if (link.status !== 200) throw new Error(`linking hub ${label} failed: ${link.status} ${link.text}`);
  await waitFor(`hub ${label} connected to relay`, async () => (await http(base, "/v1/relay", { headers: admin })).json?.status === "connected", 30000);
  return { label, base, admin, hubId, data, proc: hub };
}

async function main() {
  await new Promise((r) => google.listen(GOOGLE_PORT, "127.0.0.1", r));
  const relay = start("relay", "npx", ["wrangler", "dev", "--local", "--port", String(RELAY_PORT), "--local-upstream", `127.0.0.1:${RELAY_PORT}`, "--ip", "127.0.0.1",
    "--var", `GOOGLE_JWKS_URL:${GOOGLE}/jwks`, "--var", `GOOGLE_AUTH_URL:${GOOGLE}/authorize`,
    "--var", `GOOGLE_CLIENT_IDS:${IOS},${WEB}`, "--var", `GOOGLE_WEB_CLIENT_ID:${WEB}`,
    "--var", `RELAY_SECRET:${randomBytes(32).toString("hex")}`], { cwd: join(ROOT, "relay") });
  await waitFor("relay", async () => (await fetch(RELAY + "/relay/v1/me")).ok, 120000).catch((e) => { console.log(relay.logs()); throw e; });

  console.log("Hubs");
  const A = await setupHub("A", "alice", 47701);
  const B = await setupHub("B", "bob", 47702);
  check("both hubs linked and connected", A.hubId && B.hubId && A.hubId !== B.hubId);

  console.log("Relay sign-in");
  const aliceRelay = (await relaySignIn("alice")).json?.relay_token;
  const bobRelay = (await relaySignIn("bob")).json?.relay_token;
  const carolRelay = (await relaySignIn("carol")).json?.relay_token;
  check("alice, bob and carol get relay tokens", aliceRelay && bobRelay && carolRelay);
  const asA = { "grouper-relay": aliceRelay }, asB = { "grouper-relay": bobRelay }, asC = { "grouper-relay": carolRelay };

  const n1 = await relayNonce(), t1 = mint("alice", { nonce: n1, relay: true });
  check("a relay nonce works once", (await http(RELAY, "/relay/v1/sign-in", { method: "POST", body: { id_token: t1, relay_nonce: n1, client: "app" } })).status === 200);
  check("…and is refused the second time", (await http(RELAY, "/relay/v1/sign-in", { method: "POST", body: { id_token: t1, relay_nonce: n1, client: "app" } })).status >= 400);
  // A token minted for a hub-style nonce (what a malicious hub could hand a victim) must not sign in.
  const rawN = await relayNonce();
  check("a token carrying the raw relay nonce is refused", (await http(RELAY, "/relay/v1/sign-in", { method: "POST", body: { id_token: mint("alice", { nonce: rawN }), relay_nonce: rawN, client: "app" } })).status >= 400);
  check("a cross-site form post to sign-in is refused", (await http(RELAY, "/relay/v1/sign-in", { method: "POST", headers: { "content-type": "application/x-www-form-urlencoded" }, raw: "id_token=x" })).status === 415);
  check("a nonce the relay didn't issue is refused", (await http(RELAY, "/relay/v1/sign-in", { method: "POST", body: { id_token: mint("alice", { nonce: "made-up", relay: true }), relay_nonce: "made-up", client: "app" } })).status >= 400);
  check("a token for an unknown app (aud) is refused", (await http(RELAY, "/relay/v1/sign-in", { method: "POST", body: await relayBody("alice", "app", { aud: "evil.apps.googleusercontent.com" }) })).status >= 400);
  check("an expired token is refused", (await http(RELAY, "/relay/v1/sign-in", { method: "POST", body: await relayBody("alice", "app", { iatOffset: -7200, expOffset: 3600 }) })).status >= 400);
  check("a token signed by another key is refused", (await http(RELAY, "/relay/v1/sign-in", { method: "POST", body: { ...(await relayBody("alice", "app")), id_token: undefined, ...(await (async () => { const b = await relayBody("alice", "app"); return { id_token: b.id_token.replace(/\.[^.]+$/, "." + b64u(randomBytes(256))), relay_nonce: b.relay_nonce }; })()) } })).status >= 400);

  console.log("Routing");
  check("alice reaches hub A", (await http(RELAY, "/v1/hub", { headers: asA })).json?.hub_id === A.hubId);
  check("bob reaches hub B", (await http(RELAY, "/v1/hub", { headers: asB })).json?.hub_id === B.hubId);
  check("the relay knows hub A by its stable hub id", (await http(RELAY, "/relay/v1/me", { headers: asA })).json?.hub_id === A.hubId);
  const carolHub = await http(RELAY, "/v1/hub", { headers: asC });
  check("carol (no hub) gets 503", carolHub.status === 503, `${carolHub.status}`);
  const anon = await http(RELAY, "/v1/hub");
  check("no relay token: 401 JSON", anon.status === 401 && anon.json?.relay === true, `${anon.status}`);
  const anonPage = await http(RELAY, "/app", { headers: { accept: "text/html" } });
  check("no relay token: /app redirects to the relay sign-in page", anonPage.status === 302 && anonPage.headers.get("location") === "/relay/sign-in?next=%2Fapp");
  const signInPage = await http(RELAY, "/relay/sign-in?next=%2Fapp", { headers: { accept: "text/html" } });
  check("the relay sign-in page offers Google", signInPage.status === 200 && /sign in with google/i.test(signInPage.text));
  const siteHome = await http(RELAY, "/", { headers: { accept: "text/html" } });
  check("/ is the public website, no sign-in", siteHome.status === 200 && siteHome.text.includes("Open Grouper"));
  const forged = aliceRelay.slice(0, -4) + (aliceRelay.endsWith("AAAA") ? "BBBB" : "AAAA");
  check("a tampered relay token is refused", (await http(RELAY, "/v1/hub", { headers: { "grouper-relay": forged } })).status === 401);

  console.log("Signing in to the hub through the relay");
  async function appSignIn(asRelay, user) {
    const nonce = (await http(RELAY, "/v1/auth/google/nonce", { method: "POST", headers: asRelay })).json?.nonce;
    return http(RELAY, "/v1/auth/google", { method: "POST", headers: asRelay, body: { id_token: mint(user, { nonce }), client: "ios", device_name: `${user}'s test phone`, install_id: randomBytes(12).toString("hex") } });
  }
  const alicePhone = await appSignIn(asA, "alice");
  check("alice's phone signs in to hub A through the relay", alicePhone.status === 200 && alicePhone.json?.token, `${alicePhone.status} ${alicePhone.text.slice(0, 200)}`);
  const bobPhone = await appSignIn(asB, "bob");
  check("bob's phone signs in to hub B through the relay", bobPhone.status === 200 && bobPhone.json?.token);
  const bobAtA = await appSignIn(asA, "bob");
  check("bob's Google account is refused by hub A even when routed there", bobAtA.status === 403, `${bobAtA.status}`);
  const aliceDevices = await http(RELAY, "/v1/devices", { headers: { ...asA, authorization: `Bearer ${alicePhone.json?.token}` } });
  const names = JSON.stringify(aliceDevices.json ?? "");
  check("alice's device list comes from hub A only", aliceDevices.status === 200 && names.includes("alice's test phone") && !names.includes("bob's test phone"), `${aliceDevices.status}`);
  const crossed = await http(RELAY, "/v1/devices", { headers: { ...asB, authorization: `Bearer ${alicePhone.json?.token}` } });
  check("alice's hub token is useless on bob's route", crossed.status === 401, `${crossed.status}`);

  console.log("What the hub sees");
  const leak = await http(RELAY, "/v1/session", { headers: { ...asA, cookie: `grouper_relay=${aliceRelay}; other=1` } });
  check("relay credentials never reach the hub (hub answers as signed out)", leak.status === 200 && leak.json?.signed_in === false);
  const hubLog = A.proc.logs();
  check("hub A's log never contains a relay token", !hubLog.includes(aliceRelay.slice(3, 40)));

  console.log("Browser-style session through the relay");
  const webIn = await relaySignIn("alice", "web");
  const relayCookie = (webIn.headers.get("set-cookie") || "").split(";")[0];
  check("web relay sign-in sets the grouper_relay cookie", relayCookie.startsWith("grouper_relay="));
  const page = await http(RELAY, "/app", { headers: { cookie: relayCookie, accept: "text/html" } });
  check("with the cookie, /app is hub A's web app", page.status === 200 && /<script|app\.js/i.test(page.text) && !/relay\/auth\/start/.test(page.text));

  console.log("Browser sign-in through Google's return page");
  {
    const start = await http(RELAY, "/relay/auth/start?next=%2Fapp%2Fsettings");
    const stCookie = (start.headers.getSetCookie().find((c) => c.startsWith("grouper_relay_state=")) || "").split(";")[0];
    const g = new URL(start.headers.get("location"));
    check("browser sign-in returns to /oauth/google/return", g.searchParams.get("redirect_uri") === `${RELAY}/oauth/google/return`, g.searchParams.get("redirect_uri"));
    const q = new URLSearchParams(g.search); q.set("user", "alice");
    const gForm = await (await fetch(`${GOOGLE}/authorize?${q}`)).text();
    const tok = /name="id_token" value="([^"]+)"/.exec(gForm)?.[1];
    const st = /name="state" value="([^"]+)"/.exec(gForm)?.[1];
    const back = await http(RELAY, "/oauth/google/return", { method: "POST", headers: { "content-type": "application/x-www-form-urlencoded", cookie: stCookie }, raw: new URLSearchParams({ id_token: tok, state: st }).toString() });
    check("the relay completes it and sends the browser to next", back.status === 303 && back.headers.get("location") === "/app/settings" && (back.headers.get("set-cookie") || "").includes("grouper_relay="), `${back.status} ${back.headers.get("location")}`);
    const hubReturn = await http(RELAY, "/oauth/google/return", { method: "POST", headers: { ...asA, "content-type": "application/x-www-form-urlencoded" }, raw: "id_token=x&state=hub-own-state" });
    check("a return page post with no relay state goes to the hub", hubReturn.status !== 303 && !(hubReturn.headers.get("set-cookie") || "").includes("grouper_relay="), `${hubReturn.status}`);
  }

  console.log("WebSockets through the relay");
  const wsUrl = RELAY.replace("http", "ws") + "/v1/events";
  const ws = new WebSocket(wsUrl, { headers: { ...asA, authorization: `Bearer ${alicePhone.json?.token}` } });
  const opened = await new Promise((r) => { ws.onopen = () => r(true); ws.onerror = () => r(false); ws.onclose = () => r(false); setTimeout(() => r(false), 10000); });
  check("alice opens /v1/events on hub A through the relay", opened);
  try { ws.close(); } catch {}

  console.log("Sign-out and replacing a hub");
  await http(RELAY, "/relay/v1/sign-out", { method: "POST", headers: asB, body: { everywhere: true } });
  check("bob's old relay tokens stop working after sign-out everywhere", (await http(RELAY, "/v1/hub", { headers: asB })).status === 401);
  check("alice is unaffected", (await http(RELAY, "/v1/hub", { headers: asA })).json?.hub_id === A.hubId);
  // Hub B claims alice's account: only possible with alice's own fresh Google sign-in, and hub B's
  // owner check refuses it, so the relay route for alice can't be taken by bob's hub.
  const rnB = (await http(B.base, "/v1/relay/nonce", { method: "POST", headers: B.admin })).json?.nonce;
  const steal = await http(B.base, "/v1/relay/link", { method: "POST", headers: B.admin, body: { id_token: mint("alice", { nonce: rnB, relay: true }), relay_nonce: rnB } });
  check("hub B can't link itself to alice's relay account", steal.status >= 400, `${steal.status}`);
  check("alice still reaches hub A", (await http(RELAY, "/v1/hub", { headers: asA })).json?.hub_id === A.hubId);

  console.log(`\n${passes} passed, ${failures} failed`);
  if (KEEP) {
    console.log(`\nLeft running: relay ${RELAY}, fake Google ${GOOGLE}, hub A http://127.0.0.1:47701, hub B http://127.0.0.1:47702. Ctrl+C stops.`);
    process.on("SIGINT", () => { stopAll(); process.exit(0); });
    return;
  }
  stopAll();
  try { rmSync(tmp, { recursive: true, force: true }); } catch {}
  process.exit(failures ? 1 : 0);
}

main().catch((e) => {
  console.error(e);
  for (const p of procs) console.error(`--- ${p.label}\n${p.logs().slice(-3000)}`);
  stopAll();
  process.exit(1);
});
