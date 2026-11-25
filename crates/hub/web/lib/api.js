// The hub's HTTP API, signed in with the session cookie.
// Every request sends `X-Grouper: 1`: the hub refuses cookie-authenticated changes
// without it, and other sites can't add it (see crates/hub/src/session.rs).

export class ApiError extends Error {
  constructor(status, message) {
    super(message);
    this.status = status;
  }
}

const listeners = new Set();
/** Called with every ApiError (used to notice a lost session). */
export function onApiError(fn) {
  listeners.add(fn);
}

export async function api(method, path, body) {
  const init = {
    method,
    credentials: 'same-origin',
    cache: 'no-store',
    headers: { 'X-Grouper': '1', Accept: 'application/json' },
  };
  if (body !== undefined) {
    init.headers['Content-Type'] = 'application/json';
    init.body = JSON.stringify(body);
  }
  let response;
  try {
    response = await fetch(path, init);
  } catch (e) {
    const error = new ApiError(0, "Can't reach the hub.");
    for (const fn of listeners) fn(error);
    throw error;
  }
  const text = await response.text();
  let data = null;
  if (text) {
    try {
      data = JSON.parse(text);
    } catch {
      data = text;
    }
  }
  if (!response.ok) {
    const message = (data && typeof data === 'object' && data.error) || (typeof data === 'string' && data) || response.statusText;
    const error = new ApiError(response.status, message);
    for (const fn of listeners) fn(error);
    throw error;
  }
  return data;
}

export const get = (path) => api('GET', path);
export const post = (path, body) => api('POST', path, body ?? {});
export const put = (path, body) => api('PUT', path, body ?? {});
export const del = (path) => api('DELETE', path);

/** Query string from an object, skipping empty values. */
export function qs(params) {
  const s = new URLSearchParams();
  for (const [k, v] of Object.entries(params)) {
    if (v !== undefined && v !== null && v !== '') s.set(k, v);
  }
  const text = s.toString();
  return text ? `?${text}` : '';
}

export const enc = encodeURIComponent;

/** A message for people from an error. */
export function describe(error) {
  if (!(error instanceof ApiError)) return String(error?.message || error);
  if (error.status === 403) return `${error.message}. This token can't act for you here; sign in with an admin token.`;
  return error.message;
}
