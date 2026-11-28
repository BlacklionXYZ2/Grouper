// Google ID token verification with WebCrypto (RS256). No dependencies.

import { b64urlDecode, nowSec, utf8, type Env } from "./util";

export class AuthError extends Error {}

export interface GoogleClaims {
  iss: string;
  aud: string;
  sub: string;
  email: string;
  email_verified: boolean | string;
  nonce?: string;
  exp: number;
  iat: number;
}

interface Jwk {
  kid?: string;
  kty: string;
  n?: string;
  e?: string;
  alg?: string;
  use?: string;
}

interface JwksCache {
  url: string;
  keys: Map<string, Jwk>;
  expires: number; // ms
  fetchedAt: number; // ms
}

// Per-isolate caches.
let jwks: JwksCache | null = null;
const imported = new Map<string, Promise<CryptoKey>>();
const MIN_REFETCH_MS = 30_000; // unknown-kid refetch throttle, so junk kids can't hammer Google

export const DEFAULT_JWKS_URL = "https://www.googleapis.com/oauth2/v3/certs";
export const DEFAULT_AUTH_URL = "https://accounts.google.com/o/oauth2/v2/auth";
export const DEFAULT_ISSUERS = "https://accounts.google.com,accounts.google.com";

function list(s: string | undefined): string[] {
  return (s ?? "")
    .split(",")
    .map((x) => x.trim())
    .filter(Boolean);
}

async function loadJwks(url: string): Promise<JwksCache> {
  const res = await fetch(url);
  if (!res.ok) throw new AuthError("could not fetch Google signing keys");
  const body = (await res.json()) as { keys?: Jwk[] };
  const keys = new Map<string, Jwk>();
  for (const k of body.keys ?? []) if (k.kid && k.kty === "RSA") keys.set(k.kid, k);
  const cc = res.headers.get("cache-control") ?? "";
  const m = /max-age=(\d+)/i.exec(cc);
  const ttl = m ? Number(m[1]) * 1000 : 300_000;
  const now = Date.now();
  return { url, keys, expires: now + ttl, fetchedAt: now };
}

async function findKey(env: Env, kid: string): Promise<{ jwk: Jwk; cacheId: string }> {
  const url = env.GOOGLE_JWKS_URL || DEFAULT_JWKS_URL;
  const now = Date.now();
  if (!jwks || jwks.url !== url || jwks.expires <= now) jwks = await loadJwks(url);
  let jwk = jwks.keys.get(kid);
  if (!jwk && now - jwks.fetchedAt >= MIN_REFETCH_MS) {
    jwks = await loadJwks(url);
    jwk = jwks.keys.get(kid);
  }
  if (!jwk) throw new AuthError("unknown signing key");
  return { jwk, cacheId: url + "#" + kid + "#" + jwk.n };
}

function importKey(cacheId: string, jwk: Jwk): Promise<CryptoKey> {
  let k = imported.get(cacheId);
  if (!k) {
    k = crypto.subtle.importKey(
      "jwk",
      { kty: "RSA", n: jwk.n!, e: jwk.e!, alg: "RS256", ext: true },
      { name: "RSASSA-PKCS1-v1_5", hash: "SHA-256" },
      false,
      ["verify"],
    );
    imported.set(cacheId, k);
    if (imported.size > 50) imported.delete(imported.keys().next().value!);
  }
  return k;
}

function parseJson(bytes: Uint8Array): Record<string, unknown> {
  const v = JSON.parse(new TextDecoder().decode(bytes));
  if (typeof v !== "object" || v === null || Array.isArray(v)) throw new Error("not an object");
  return v as Record<string, unknown>;
}

/**
 * Verifies signature, iss, aud, exp, iat, email_verified. The nonce is checked by the caller
 * (it needs the relay secret and account storage).
 */
export async function verifyIdToken(env: Env, token: unknown): Promise<GoogleClaims> {
  if (typeof token !== "string" || token.length > 8192) throw new AuthError("missing id_token");
  const parts = token.split(".");
  if (parts.length !== 3) throw new AuthError("malformed id_token");
  let header: Record<string, unknown>;
  let claims: Record<string, unknown>;
  let sig: Uint8Array<ArrayBuffer>;
  try {
    header = parseJson(b64urlDecode(parts[0]!));
    claims = parseJson(b64urlDecode(parts[1]!));
    sig = b64urlDecode(parts[2]!);
  } catch {
    throw new AuthError("malformed id_token");
  }
  if (header.alg !== "RS256" || typeof header.kid !== "string") throw new AuthError("unsupported id_token algorithm");

  const { jwk, cacheId } = await findKey(env, header.kid);
  const key = await importKey(cacheId, jwk);
  const ok = await crypto.subtle.verify("RSASSA-PKCS1-v1_5", key, sig, utf8(parts[0] + "." + parts[1]));
  if (!ok) throw new AuthError("bad id_token signature");

  const c = claims as unknown as GoogleClaims;
  const issuers = list(env.GOOGLE_ISSUERS || DEFAULT_ISSUERS);
  if (typeof c.iss !== "string" || !issuers.includes(c.iss)) throw new AuthError("wrong issuer");
  const auds = list(env.GOOGLE_CLIENT_IDS);
  if (typeof c.aud !== "string" || !auds.includes(c.aud)) throw new AuthError("wrong audience");
  const now = nowSec();
  if (typeof c.exp !== "number" || c.exp <= now) throw new AuthError("id_token expired");
  if (typeof c.iat !== "number" || c.iat < now - 600 || c.iat > now + 300) throw new AuthError("id_token not fresh");
  if (c.email_verified !== true && c.email_verified !== "true") throw new AuthError("email not verified");
  if (typeof c.sub !== "string" || !c.sub || typeof c.email !== "string" || !c.email) {
    throw new AuthError("id_token missing sub or email");
  }
  return c;
}

/** Test hook: forget cached keys. */
export function resetGoogleCache(): void {
  jwks = null;
  imported.clear();
}
