// Small shared helpers: encodings, HMAC, hashing, JSON responses.

export interface Env {
  ACCOUNT: DurableObjectNamespace<import("./account").Account>;
  RELAY_SECRET: string;
  GOOGLE_CLIENT_IDS: string;
  GOOGLE_WEB_CLIENT_ID: string;
  /** Optional: when set, /relay/auth/* uses and requires only this audience. */
  RELAY_WEB_CLIENT_ID?: string;
  GOOGLE_JWKS_URL?: string;
  GOOGLE_AUTH_URL?: string;
  GOOGLE_ISSUERS?: string;
}

const enc = new TextEncoder();

export function utf8(s: string): Uint8Array<ArrayBuffer> {
  return enc.encode(s) as Uint8Array<ArrayBuffer>;
}

export function b64urlEncode(bytes: Uint8Array): string {
  let s = "";
  for (let i = 0; i < bytes.length; i++) s += String.fromCharCode(bytes[i]!);
  return btoa(s).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

export function b64urlDecode(s: string): Uint8Array<ArrayBuffer> {
  if (!/^[A-Za-z0-9_-]*$/.test(s)) throw new Error("bad base64url");
  const pad = s.length % 4 === 0 ? "" : "=".repeat(4 - (s.length % 4));
  return b64Decode(s.replace(/-/g, "+").replace(/_/g, "/") + pad);
}

export function b64Encode(bytes: Uint8Array): string {
  let s = "";
  const CH = 0x8000;
  for (let i = 0; i < bytes.length; i += CH) {
    s += String.fromCharCode(...bytes.subarray(i, i + CH));
  }
  return btoa(s);
}

export function b64Decode(s: string): Uint8Array<ArrayBuffer> {
  const bin = atob(s);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

export function hex(bytes: Uint8Array): string {
  let s = "";
  for (const b of bytes) s += b.toString(16).padStart(2, "0");
  return s;
}

export function randomB64url(n: number): string {
  return b64urlEncode(crypto.getRandomValues(new Uint8Array(n)));
}

const keyCache = new Map<string, Promise<CryptoKey>>();
function hmacKey(secret: string): Promise<CryptoKey> {
  let k = keyCache.get(secret);
  if (!k) {
    k = crypto.subtle.importKey("raw", utf8(secret), { name: "HMAC", hash: "SHA-256" }, false, ["sign"]);
    keyCache.set(secret, k);
  }
  return k;
}

export async function hmac(secret: string, data: string): Promise<Uint8Array> {
  const sig = await crypto.subtle.sign("HMAC", await hmacKey(secret), utf8(data));
  return new Uint8Array(sig);
}

export async function sha256Hex(data: string): Promise<string> {
  return hex(new Uint8Array(await crypto.subtle.digest("SHA-256", utf8(data))));
}

/** Constant-time string comparison (length difference leaks, which is fine for fixed-size values). */
export function safeEqual(a: string, b: string): boolean {
  if (a.length !== b.length) return false;
  let d = 0;
  for (let i = 0; i < a.length; i++) d |= a.charCodeAt(i) ^ b.charCodeAt(i);
  return d === 0;
}

export function nowSec(): number {
  return Math.floor(Date.now() / 1000);
}

export function json(status: number, body: unknown, headers?: HeadersInit): Response {
  const h = new Headers(headers);
  h.set("content-type", "application/json");
  h.set("cache-control", "no-store");
  return new Response(JSON.stringify(body), { status, headers: h });
}

/** Error body shape used for every relay-originated failure. */
export function relayError(status: number, message: string, headers?: HeadersInit): Response {
  return json(status, { error: message, relay: true }, headers);
}

export const HOP_BY_HOP = new Set([
  "connection",
  "keep-alive",
  "transfer-encoding",
  "upgrade",
  "te",
  "trailer",
  "content-length",
]);

/** True for headers both sides drop: hop-by-hop, proxy-*, sec-websocket-*. */
export function isHopByHop(name: string): boolean {
  const n = name.toLowerCase();
  return HOP_BY_HOP.has(n) || n.startsWith("proxy-") || n.startsWith("sec-websocket-");
}
