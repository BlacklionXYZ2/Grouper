// Stateless HMAC credentials: account keys, relay nonces, relay tokens, hub secrets.
// All times are unix seconds.

import { b64urlDecode, b64urlEncode, hex, hmac, nowSec, randomB64url, safeEqual, utf8 } from "./util";

/** Domain-separated Google `nonce` claim for a relay nonce: b64url(SHA-256("grouper-relay-v1:" + nonce)). */
export async function nonceClaim(relayNonce: string): Promise<string> {
  return b64urlEncode(new Uint8Array(await crypto.subtle.digest("SHA-256", utf8("grouper-relay-v1:" + relayNonce))));
}

export const NONCE_TTL = 600;
export const WEB_TOKEN_TTL = 30 * 86400;
export const APP_TOKEN_TTL = 365 * 86400;

export async function acctKey(secret: string, sub: string): Promise<string> {
  return hex(await hmac(secret, "acct:" + sub)).slice(0, 32);
}

export const ACCT_RE = /^[0-9a-f]{32}$/;

export async function makeNonce(secret: string): Promise<{ nonce: string; expires: number }> {
  const rand = randomB64url(16);
  const exp = nowSec() + NONCE_TTL;
  const sig = b64urlEncode(await hmac(secret, rand + "." + exp));
  return { nonce: `rn.${rand}.${exp}.${sig}`, expires: exp };
}

/** Returns the expiry if the nonce is well-formed, correctly signed and unexpired; else null. */
export async function checkNonce(secret: string, nonce: unknown): Promise<number | null> {
  if (typeof nonce !== "string") return null;
  const p = nonce.split(".");
  if (p.length !== 4 || p[0] !== "rn") return null;
  const [, rand, expStr, sig] = p as [string, string, string, string];
  if (!/^\d{1,12}$/.test(expStr)) return null;
  const exp = Number(expStr);
  if (exp <= nowSec()) return null;
  const want = b64urlEncode(await hmac(secret, rand + "." + exp));
  return safeEqual(want, sig) ? exp : null;
}

export interface RelayClaims {
  a: string; // account key
  e: string; // email
  k: "web" | "app";
  g: number; // generation
  x: number; // expires
}

export async function makeRelayToken(secret: string, claims: RelayClaims): Promise<string> {
  const body = b64urlEncode(utf8(JSON.stringify(claims)));
  const sig = b64urlEncode(await hmac(secret, "rt." + body));
  return `rt.${body}.${sig}`;
}

export async function readRelayToken(secret: string, token: string | null | undefined): Promise<RelayClaims | null> {
  if (!token) return null;
  const p = token.split(".");
  if (p.length !== 3 || p[0] !== "rt") return null;
  const [, body, sig] = p as [string, string, string];
  const want = b64urlEncode(await hmac(secret, "rt." + body));
  if (!safeEqual(want, sig)) return null;
  try {
    const c = JSON.parse(new TextDecoder().decode(b64urlDecode(body))) as RelayClaims;
    if (
      typeof c.a !== "string" ||
      !ACCT_RE.test(c.a) ||
      typeof c.e !== "string" ||
      (c.k !== "web" && c.k !== "app") ||
      typeof c.g !== "number" ||
      typeof c.x !== "number" ||
      c.x <= nowSec()
    ) {
      return null;
    }
    return c;
  } catch {
    return null;
  }
}

/** `hs.<acct>.<rand>.<tag>`; the tag lets the Worker reject forged secrets without touching a Durable Object. */
export async function makeHubSecret(secret: string, acct: string): Promise<string> {
  const rand = randomB64url(32);
  return `hs.${acct}.${rand}.${await hubTag(secret, acct, rand)}`;
}

async function hubTag(secret: string, acct: string, rand: string): Promise<string> {
  return b64urlEncode((await hmac(secret, `hs:${acct}.${rand}`)).slice(0, 16));
}

/** The account key in a hub secret whose tag verifies (constant time); null if malformed or forged. */
export async function verifyHubSecret(secret: string, hubSecret: string): Promise<string | null> {
  const p = hubSecret.split(".");
  if (p.length !== 4 || p[0] !== "hs" || !ACCT_RE.test(p[1]!) || !p[2] || !p[3]) return null;
  return safeEqual(await hubTag(secret, p[1]!, p[2]), p[3]) ? p[1]! : null;
}
