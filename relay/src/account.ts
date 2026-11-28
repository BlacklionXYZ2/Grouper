// One Durable Object per account: nonce/generation/hub state, plus the hub tunnel.

import { DurableObject } from "cloudflare:workers";
import {
  b64Decode,
  b64Encode,
  isHopByHop,
  nowSec,
  relayError,
  safeEqual,
  sha256Hex,
  type Env,
} from "./util";
import { hubOfflinePage } from "./pages";

const MAX_BODY = 16 * 1024 * 1024;
const RES_TIMEOUT_MS = 100_000;
const WS_OPEN_TIMEOUT_MS = 30_000;
const HUB_IDLE_MS = 90_000;
const IDLE_CHECK_MS = 60_000;
const ID_BLOCK = 1000;

interface HubFrame {
  t: string;
  id?: number;
  status?: number;
  headers?: [string, string][];
  b64?: string;
  text?: string;
  message?: string;
  code?: number;
  reason?: string;
  hub_id?: string;
  version?: string;
}

interface PendingReq {
  hub: WebSocket;
  settled: boolean; // `res` or a failure already delivered
  timer: ReturnType<typeof setTimeout>;
  resolve: (r: Response) => void;
  ctl?: ReadableStreamDefaultController<Uint8Array>;
  status?: number;
}

interface PendingWs {
  hub: WebSocket;
  timer: ReturnType<typeof setTimeout>;
  resolve: (r: { ok: true } | { ok: false; status: number; message: string }) => void;
}

export class Account extends DurableObject<Env> {
  private reqs = new Map<number, PendingReq>();
  private wss = new Map<number, PendingWs>();
  private nextId = 1;
  private idLimit = 0;
  private lastSeen = Date.now();

  constructor(ctx: DurableObjectState, env: Env) {
    super(ctx, env);
    ctx.storage.sql.exec("CREATE TABLE IF NOT EXISTS nonces (n TEXT PRIMARY KEY, exp INTEGER NOT NULL)");
    // Ids must stay unique per open socket across hibernation: restart from a reserved block.
    const reserved = (ctx.storage.kv.get("id_limit") as number | undefined) ?? 0;
    this.nextId = reserved + 1;
    this.idLimit = reserved;
    ctx.setWebSocketAutoResponse(new WebSocketRequestResponsePair('{"t":"ping"}', '{"t":"pong"}'));
  }

  private allocId(): number {
    if (this.nextId > this.idLimit) {
      this.idLimit = this.nextId + ID_BLOCK;
      this.ctx.storage.kv.put("id_limit", this.idLimit);
    }
    return this.nextId++;
  }

  // ---- RPC used by the Worker -------------------------------------------------------------

  /** Records a relay nonce as used. False if it was already used. */
  consumeNonce(nonce: string, exp: number): boolean {
    const sql = this.ctx.storage.sql;
    sql.exec("DELETE FROM nonces WHERE exp <= ?", nowSec());
    const seen = sql.exec("SELECT 1 FROM nonces WHERE n = ?", nonce).toArray().length > 0;
    if (seen) return false;
    sql.exec("INSERT INTO nonces (n, exp) VALUES (?, ?)", nonce, exp);
    return true;
  }

  generation(): number {
    return (this.ctx.storage.kv.get("generation") as number | undefined) ?? 0;
  }

  /** Invalidates every relay token and cuts live client traffic: sockets (4401), in-flight requests. */
  bumpGeneration(): number {
    const g = this.generation() + 1;
    this.ctx.storage.kv.put("generation", g);
    this.closeClients(4401, "signed out");
    for (const [id, p] of [...this.reqs]) {
      this.sendHub(p.hub, { t: "cancel", id });
      if (!p.settled) {
        p.settled = true;
        p.resolve(relayError(401, "signed out"));
      } else {
        try {
          p.ctl?.error(new Error("signed out"));
        } catch {}
      }
      this.finishReq(id);
    }
    return g;
  }

  hubInfo(): { hub_id: string | null; hub_connected: boolean } {
    return {
      hub_id: (this.ctx.storage.kv.get("hub_id") as string | undefined) ?? null,
      hub_connected: this.liveHubs().length > 0,
    };
  }

  /** Stores a new hub id + secret hash, replacing any previous one and closing its socket (4001). */
  claimHub(hubId: string, secretHash: string): void {
    this.ctx.storage.kv.put("hub_id", hubId);
    this.ctx.storage.kv.put("hub_secret_hash", secretHash);
    this.dropHubs(4001, "replaced");
  }

  /** Forgets the hub if the secret matches. */
  async forgetHub(secret: string): Promise<boolean> {
    if (!(await this.hubAuthorized(secret, null))) return false;
    this.ctx.storage.kv.delete("hub_id");
    this.ctx.storage.kv.delete("hub_secret_hash");
    this.dropHubs(4002, "forgotten");
    return true;
  }

  private async hubAuthorized(secret: string, hubId: string | null): Promise<boolean> {
    const hash = this.ctx.storage.kv.get("hub_secret_hash") as string | undefined;
    const id = this.ctx.storage.kv.get("hub_id") as string | undefined;
    if (!hash || !id) return false;
    const ok = safeEqual(await sha256Hex(secret), hash);
    return hubId === null ? ok : ok && safeEqual(hubId, id);
  }

  // ---- fetch: hub connect and proxied client traffic -----------------------------------------

  async fetch(request: Request): Promise<Response> {
    const kind = request.headers.get("x-relay-kind");
    if (kind === "hub-connect") return this.hubConnect(request);
    if (kind === "proxy") return this.proxy(request);
    return relayError(404, "not found");
  }

  private liveHubs(): WebSocket[] {
    return this.ctx.getWebSockets("hub").filter((w) => w.readyState === WebSocket.OPEN);
  }

  private async hubConnect(request: Request): Promise<Response> {
    if (request.headers.get("upgrade")?.toLowerCase() !== "websocket") {
      return relayError(426, "expected a WebSocket upgrade");
    }
    const secret = (request.headers.get("authorization") ?? "").replace(/^Bearer\s+/i, "");
    const hubId = request.headers.get("grouper-hub-id") ?? "";
    if (!secret || !(await this.hubAuthorized(secret, hubId))) {
      return relayError(401, "wrong hub secret or hub id");
    }
    // A reconnect replaces any previous socket; client sockets belonged to that session.
    this.dropHubs(4001, "replaced");
    this.closeClients(1012, "hub reconnected");

    const pair = new WebSocketPair();
    const [client, server] = [pair[0], pair[1]];
    this.ctx.acceptWebSocket(server, ["hub"]);
    this.lastSeen = Date.now();
    void this.ctx.storage.setAlarm(Date.now() + IDLE_CHECK_MS);
    return new Response(null, { status: 101, webSocket: client });
  }

  async alarm(): Promise<void> {
    const hubs = this.liveHubs();
    if (hubs.length === 0) return;
    const now = Date.now();
    for (const h of hubs) {
      const auto = this.ctx.getWebSocketAutoResponseTimestamp(h)?.getTime() ?? 0;
      if (now - Math.max(this.lastSeen, auto) > HUB_IDLE_MS) {
        try {
          h.close(4003, "idle");
        } catch {}
        this.hubGone(h);
      }
    }
    if (this.liveHubs().length > 0) await this.ctx.storage.setAlarm(now + IDLE_CHECK_MS);
  }

  // ---- proxying -------------------------------------------------------------------------------

  private async proxy(request: Request): Promise<Response> {
    const hub = this.liveHubs()[0];
    const isWs = request.headers.get("upgrade")?.toLowerCase() === "websocket";
    if (!hub) return this.offline(request);

    const url = new URL(request.url);
    const path = url.pathname + url.search;
    const headers = forwardHeaders(request.headers);

    if (isWs) return this.proxyWebSocket(hub, path, headers);

    const len = Number(request.headers.get("content-length") ?? "0");
    if (len > MAX_BODY) return relayError(413, "request body too large");
    let body: Uint8Array | null = null;
    if (request.body && request.method !== "GET" && request.method !== "HEAD") {
      const buf = new Uint8Array(await request.arrayBuffer());
      if (buf.length > MAX_BODY) return relayError(413, "request body too large");
      if (buf.length > 0) body = buf;
    }
    return this.proxyHttp(hub, request, path, headers, body);
  }

  private offline(request: Request): Response {
    const accept = request.headers.get("accept") ?? "";
    if (request.method === "GET" && accept.includes("text/html")) return hubOfflinePage();
    return relayError(503, "your hub isn't connected to the relay");
  }

  private proxyHttp(
    hub: WebSocket,
    request: Request,
    path: string,
    headers: [string, string][],
    body: Uint8Array | null,
  ): Promise<Response> {
    const id = this.allocId();
    return new Promise<Response>((resolve) => {
      const p: PendingReq = {
        hub,
        settled: false,
        resolve,
        timer: setTimeout(() => {
          if (p.settled) return;
          this.finishReq(id);
          resolve(relayError(504, "the hub did not answer in time"));
        }, RES_TIMEOUT_MS),
      };
      this.reqs.set(id, p);

      const abort = () => {
        const cur = this.reqs.get(id);
        if (!cur) return;
        this.sendHub(hub, { t: "cancel", id });
        this.finishReq(id);
        if (!cur.settled) resolve(new Response(null, { status: 499 }));
        else
          try {
            cur.ctl?.error(new Error("client aborted"));
          } catch {}
      };
      if (request.signal.aborted) return abort();
      request.signal.addEventListener("abort", abort, { once: true });

      const frame: Record<string, unknown> = { t: "req", id, method: request.method, path, headers };
      if (body) frame.body = b64Encode(body);
      if (!this.sendHub(hub, frame)) {
        this.finishReq(id);
        resolve(relayError(502, "the hub connection dropped"));
      }
    });
  }

  private proxyWebSocket(hub: WebSocket, path: string, headers: [string, string][]): Promise<Response> {
    const id = this.allocId();
    const pair = new WebSocketPair();
    const [client, server] = [pair[0], pair[1]];
    // Accept before asking the hub so early hub frames find the socket.
    this.ctx.acceptWebSocket(server, [`c:${id}`]);
    return new Promise<Response>((resolve) => {
      const fail = (status: number, message: string) => {
        try {
          server.close(1011, "open failed");
        } catch {}
        resolve(relayError(status, message));
      };
      const p: PendingWs = {
        hub,
        timer: setTimeout(() => {
          this.wss.delete(id);
          this.sendHub(hub, { t: "ws_close", id, code: 1001, reason: "timeout" });
          fail(504, "the hub did not accept the WebSocket in time");
        }, WS_OPEN_TIMEOUT_MS),
        resolve: (r) => {
          clearTimeout(p.timer);
          this.wss.delete(id);
          if (r.ok) resolve(new Response(null, { status: 101, webSocket: client }));
          else fail(r.status, r.message);
        },
      };
      this.wss.set(id, p);
      if (!this.sendHub(hub, { t: "ws_open", id, path, headers })) {
        clearTimeout(p.timer);
        this.wss.delete(id);
        fail(502, "the hub connection dropped");
      }
    });
  }

  // ---- WebSocket events (Hibernation API) -----------------------------------------------------

  async webSocketMessage(ws: WebSocket, message: string | ArrayBuffer): Promise<void> {
    const tags = this.ctx.getTags(ws);
    if (tags.includes("hub")) {
      this.lastSeen = Date.now();
      if (typeof message !== "string") return;
      let f: HubFrame;
      try {
        f = JSON.parse(message) as HubFrame;
      } catch {
        return;
      }
      this.onHubFrame(ws, f);
      return;
    }
    const tag = tags.find((t) => t.startsWith("c:"));
    if (!tag) return;
    const id = Number(tag.slice(2));
    const hub = this.liveHubs()[0];
    if (!hub) {
      ws.close(1012, "hub offline");
      return;
    }
    if (typeof message === "string") this.sendHub(hub, { t: "ws_msg", id, text: message });
    else this.sendHub(hub, { t: "ws_msg", id, b64: b64Encode(new Uint8Array(message)) });
  }

  async webSocketClose(ws: WebSocket, code: number, reason: string): Promise<void> {
    this.onSocketGone(ws, code, reason);
  }

  async webSocketError(ws: WebSocket): Promise<void> {
    this.onSocketGone(ws, 1006, "error");
  }

  private onSocketGone(ws: WebSocket, code: number, reason: string): void {
    const tags = this.ctx.getTags(ws);
    if (tags.includes("hub")) {
      try {
        ws.close(1000, "bye");
      } catch {}
      this.hubGone(ws);
      return;
    }
    const tag = tags.find((t) => t.startsWith("c:"));
    if (!tag) return;
    const id = Number(tag.slice(2));
    const hub = this.liveHubs()[0];
    if (hub) this.sendHub(hub, { t: "ws_close", id, code: safeCode(code), reason: reason.slice(0, 100) });
    try {
      ws.close(safeCode(code), reason.slice(0, 100));
    } catch {}
  }

  // ---- hub frames -----------------------------------------------------------------------------

  private onHubFrame(hub: WebSocket, f: HubFrame): void {
    switch (f.t) {
      case "ping":
        this.sendHub(hub, { t: "pong" });
        return;
      case "res":
        return this.onRes(f);
      case "data": {
        const p = this.reqs.get(f.id ?? -1);
        if (!p?.ctl || typeof f.b64 !== "string") return;
        try {
          p.ctl.enqueue(b64Decode(f.b64));
        } catch {}
        return;
      }
      case "end": {
        const p = this.reqs.get(f.id ?? -1);
        if (!p) return;
        try {
          p.ctl?.close();
        } catch {}
        this.finishReq(f.id!);
        return;
      }
      case "err": {
        const p = this.reqs.get(f.id ?? -1);
        if (!p) return;
        if (!p.settled) {
          p.settled = true;
          p.resolve(relayError(502, f.message ? `hub error: ${f.message}` : "hub error"));
        } else {
          try {
            p.ctl?.error(new Error(f.message ?? "hub error"));
          } catch {}
        }
        this.finishReq(f.id!);
        return;
      }
      case "ws_ok":
        this.wss.get(f.id ?? -1)?.resolve({ ok: true });
        return;
      case "ws_fail":
        this.wss.get(f.id ?? -1)?.resolve({
          ok: false,
          status: f.status && f.status >= 400 && f.status < 600 ? f.status : 502,
          message: f.message ?? "the hub refused the WebSocket",
        });
        return;
      case "ws_msg": {
        const c = this.ctx.getWebSockets(`c:${f.id}`)[0];
        if (!c) return;
        try {
          if (typeof f.text === "string") c.send(f.text);
          else if (typeof f.b64 === "string") c.send(b64Decode(f.b64));
        } catch {}
        return;
      }
      case "ws_close": {
        const c = this.ctx.getWebSockets(`c:${f.id}`)[0];
        if (!c) return;
        try {
          c.close(safeCode(f.code ?? 1000), (f.reason ?? "").slice(0, 100));
        } catch {}
        return;
      }
      default:
        return; // hello, pong, unknown
    }
  }

  private onRes(f: HubFrame): void {
    const id = f.id ?? -1;
    const p = this.reqs.get(id);
    if (!p || p.settled) return;
    p.settled = true;
    clearTimeout(p.timer);
    let status = f.status ?? 200;
    if (!Number.isInteger(status) || status < 200 || status > 599) status = 502;
    const headers = new Headers();
    for (const [k, v] of f.headers ?? []) {
      if (isHopByHop(k)) continue;
      // A hub must not be able to set or clear the relay's own cookies.
      if (k.toLowerCase() === "set-cookie" && /^\s*grouper_relay(_state)?\s*=/i.test(v)) continue;
      try {
        headers.append(k, v);
      } catch {}
    }
    if (status === 204 || status === 205 || status === 304) {
      this.finishReq(id);
      p.resolve(new Response(null, { status, headers }));
      return;
    }
    const stream = new ReadableStream<Uint8Array>({
      start: (ctl) => {
        p.ctl = ctl;
      },
      cancel: () => {
        if (this.reqs.has(id)) {
          this.sendHub(p.hub, { t: "cancel", id });
          this.finishReq(id);
        }
      },
    });
    p.resolve(new Response(stream, { status, headers }));
  }

  private finishReq(id: number): void {
    const p = this.reqs.get(id);
    if (!p) return;
    clearTimeout(p.timer);
    this.reqs.delete(id);
  }

  // ---- teardown -------------------------------------------------------------------------------

  private hubGone(hub: WebSocket): void {
    for (const [id, p] of [...this.reqs]) {
      if (p.hub !== hub) continue;
      if (!p.settled) {
        p.settled = true;
        p.resolve(relayError(502, "the hub connection dropped"));
      } else {
        try {
          p.ctl?.error(new Error("hub disconnected"));
        } catch {}
      }
      this.finishReq(id);
    }
    for (const p of [...this.wss.values()]) {
      if (p.hub === hub) p.resolve({ ok: false, status: 502, message: "the hub connection dropped" });
    }
    if (this.liveHubs().filter((h) => h !== hub).length === 0) this.closeClients(1012, "hub disconnected");
  }

  private dropHubs(code: number, reason: string): void {
    for (const h of this.ctx.getWebSockets("hub")) {
      try {
        h.close(code, reason);
      } catch {}
      this.hubGone(h);
    }
  }

  private closeClients(code: number, reason: string): void {
    for (const ws of this.ctx.getWebSockets()) {
      if (this.ctx.getTags(ws).includes("hub")) continue;
      try {
        ws.close(code, reason);
      } catch {}
    }
  }

  private sendHub(hub: WebSocket, frame: unknown): boolean {
    try {
      hub.send(JSON.stringify(frame));
      return true;
    } catch {
      return false;
    }
  }
}

/** WebSocket.close() in Workers accepts 1000 and 3000-4999; map anything else to 1000. */
function safeCode(code: number): number {
  return code === 1000 || (code >= 3000 && code <= 4999) ? code : 1000;
}

/** Headers for a `req` / `ws_open` frame. Credentials are already stripped by the Worker. */
function forwardHeaders(h: Headers): [string, string][] {
  const out: [string, string][] = [];
  const host = h.get("x-relay-host");
  if (host) out.push(["host", host]);
  h.forEach((v, k) => {
    const n = k.toLowerCase();
    if (
      n === "host" ||
      isHopByHop(n) ||
      n.startsWith("x-relay-") ||
      n.startsWith("cf-") ||
      n === "x-forwarded-for" ||
      n === "x-forwarded-proto" ||
      n === "x-real-ip" ||
      n === "forwarded" ||
      n === "authorization-relay"
    ) {
      return;
    }
    out.push([n, v]);
  });
  out.push(["x-forwarded-for", h.get("x-relay-ip") || "0.0.0.0"]);
  out.push(["x-forwarded-proto", h.get("x-relay-proto") || "https"]);
  return out;
}
