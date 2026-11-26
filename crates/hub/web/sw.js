// Grouper's service worker: approval notifications only (gui-spec 5.14). Served
// at /sw.js so its scope is the whole app. It has no fetch handler and caches
// nothing.
//
// userVisibleOnly compliance:
//   WebKit (Safari on macOS, iOS/iPadOS Home Screen web apps) revokes a site's
//   push subscription permanently after 3 pushes that do not call
//   showNotification() (with no exemption for visible/focused tabs). Chromium
//   shows a generic "updated in the background" notice unless a visible,
//   focused client window is open. Therefore:
//   - The hub never sends silent `approval_cleared` pushes.
//   - On every `approval_requested` push, we only suppress showNotification()
//     when a visible, focused Grouper tab is on that chat AND the browser is
//     Chromium (which exempts focused visible tabs). On WebKit, Firefox and all
//     other browsers we always show the notification so userVisibleOnly is never
//     violated.
//   - Stale notifications are closed by the open page via getNotifications()
//     whenever its approvals change, and by this worker on each push via
//     GET /v1/approvals (same-origin session cookie).
//
// push
//   {"type":"approval_requested", approval_id, conversation_id, thread_id, tool,
//    device, thread_title, chat_title, ts}
//     Shows "Grouper needs your review" / "{tool} on {device} · {thread} · {chat}"
//     with tag grouper-approval-<id>, and prunes older notifications whose
//     approvals are no longer pending.
//   {"type":"test", ts}
//     Settings' "Send a test notification"; a tap opens Settings.
//   Payloads never contain the tool input or the reason; nothing else is shown.
//
// notificationclick
//   Focuses an open Grouper tab and moves it to the chat with the dock on that
//   request (#/chat/<id>?approval=<id>), or opens a new window there. The app
//   then checks the request against the hub; the notification is never trusted
//   for anything but the route.

const TITLE = 'Grouper needs your review';
const TAG_PREFIX = 'grouper-approval-';
const ASK_TIMEOUT_MS = 700;
const PRUNE_TIMEOUT_MS = 5000;

self.addEventListener('install', () => {
  self.skipWaiting();
});

self.addEventListener('activate', (event) => {
  event.waitUntil(self.clients.claim());
});

self.addEventListener('push', (event) => {
  let data = null;
  try {
    data = event.data ? event.data.json() : null;
  } catch {
    data = null;
  }
  event.waitUntil(handlePush(data));
});

self.addEventListener('notificationclick', (event) => {
  const note = event.notification;
  const data = note.data || {};
  note.close();
  event.waitUntil(openApp(routeFor(data)));
});

/** A short, plain string from the payload (anything else is dropped). */
function field(value, max = 200) {
  if (typeof value !== 'string') return '';
  const s = value.replace(/\s+/g, ' ').trim();
  return s.length > max ? `${s.slice(0, max - 1)}…` : s;
}

/**
 * Only Chromium exempts a focused, visible client window from userVisibleOnly
 * enforcement. WebKit (Safari / iOS Home Screen web apps) counts every push
 * without showNotification() towards permanent subscription revocation (limit 3),
 * and Firefox enforces a silent-push quota as well.
 */
function canSuppressOnFocusedClient() {
  const nav = self.navigator;
  if (!nav) return false;
  const brands = nav.userAgentData?.brands;
  if (Array.isArray(brands) && brands.length) {
    return brands.some((b) => /Chromium|Google Chrome|Microsoft Edge|Opera|Brave/i.test(String(b?.brand || '')));
  }
  const ua = String(nav.userAgent || '');
  // iOS browsers (including CriOS) all run WebKit.
  if (/iPhone|iPad|iPod|CriOS|FxiOS/i.test(ua)) return false;
  return /(?:Chrome|Edg|OPR)\//.test(ua);
}

async function handlePush(data) {
  if (!data || typeof data !== 'object') {
    await showFallback();
    return;
  }
  if (data.type === 'test') {
    // Settings > Notifications > "Send a test notification" (POST /v1/push/test).
    await self.registration.showNotification('Grouper test notification', {
      body: 'Notifications work in this browser. Approval alerts show names only, never the command.',
      tag: 'grouper-test',
      icon: '/app/icon.svg',
      data: { hash: '#/settings' },
    });
    return;
  }
  const id = field(data.approval_id, 120);
  if (data.type !== 'approval_requested' || !id) {
    await showFallback();
    return;
  }
  const conversationId = field(data.conversation_id, 120);
  if (canSuppressOnFocusedClient() && conversationId && (await chatOnScreen(conversationId))) {
    await pruneStaleNotifications(null);
    return;
  }
  const tool = field(data.tool, 80) || 'A tool';
  const device = field(data.device, 80);
  const body = [device ? `${tool} on ${device}` : tool, field(data.thread_title), field(data.chat_title)].filter(Boolean).join(' · ');
  // Show the notification immediately before any network check so a slow or
  // offline hub can never cause a userVisibleOnly timeout.
  await self.registration.showNotification(TITLE, {
    body,
    tag: TAG_PREFIX + id,
    icon: '/app/icon.svg',
    data: { approval_id: id, conversation_id: conversationId, thread_id: field(data.thread_id, 120) },
  });
  await pruneStaleNotifications(id);
}

async function showFallback() {
  await self.registration.showNotification(TITLE, {
    body: 'Open Grouper to check pending approvals.',
    tag: 'grouper-fallback',
    icon: '/app/icon.svg',
    data: { hash: '#/inbox' },
  });
}

/**
 * Ask the hub for currently pending approvals (using this browser's session
 * cookie) and close any older approval notifications that are no longer pending.
 * Never throws: network failures or signed-out responses are ignored.
 */
async function pruneStaleNotifications(keepId) {
  if (typeof fetch !== 'function' || typeof self.registration?.getNotifications !== 'function') return;
  const controller = typeof AbortController === 'function' ? new AbortController() : null;
  const timer = controller ? setTimeout(() => controller.abort(), PRUNE_TIMEOUT_MS) : null;
  try {
    const res = await fetch('/v1/approvals', {
      credentials: 'same-origin',
      cache: 'no-store',
      ...(controller ? { signal: controller.signal } : null),
    });
    if (!res || !res.ok) return;
    const list = await res.json();
    if (!Array.isArray(list)) return;
    const pending = new Set();
    for (const item of list) {
      if (item && typeof item.approval_id === 'string') pending.add(item.approval_id);
    }
    const notes = await self.registration.getNotifications();
    for (const n of notes || []) {
      const tag = String(n.tag || '');
      if (!tag.startsWith(TAG_PREFIX)) continue;
      const nid = tag.slice(TAG_PREFIX.length);
      if (!nid || nid === keepId) continue;
      if (!pending.has(nid)) n.close();
    }
  } catch {
    /* offline, timed out or signed out: keep notifications as they are */
  } finally {
    if (timer !== null) clearTimeout(timer);
  }
}

// ---- is the request's chat already on screen?

async function chatOnScreen(conversationId) {
  const list = await self.clients.matchAll({ type: 'window', includeUncontrolled: true });
  const candidates = list.filter((c) => c.visibilityState === 'visible' && c.focused);
  const answers = await Promise.all(candidates.map((c) => ask(c, { type: 'grouper:where' })));
  return answers.some((a) => a && a.signedIn && a.visible && a.focused && a.conversation === conversationId);
}

/** Post a message to a page and wait briefly for its reply on a private channel. */
function ask(client, message) {
  return new Promise((resolve) => {
    const channel = new MessageChannel();
    const timer = setTimeout(() => resolve(null), ASK_TIMEOUT_MS);
    channel.port1.onmessage = (e) => {
      clearTimeout(timer);
      resolve(e.data);
    };
    try {
      client.postMessage(message, [channel.port2]);
    } catch {
      clearTimeout(timer);
      resolve(null);
    }
  });
}

// ---- opening the app on a tap

function routeFor(data) {
  if (typeof data.hash === 'string' && /^#\/(chat\/[^#\s]*|inbox|settings)$/.test(data.hash)) return data.hash;
  const cid = field(data.conversation_id, 120);
  const id = field(data.approval_id, 120);
  if (cid && id) return `#/chat/${encodeURIComponent(cid)}?approval=${encodeURIComponent(id)}`;
  if (cid) return `#/chat/${encodeURIComponent(cid)}`;
  return '#/inbox';
}

async function openApp(hash) {
  const url = new URL(`/${hash}`, self.location.origin).href;
  const list = await self.clients.matchAll({ type: 'window', includeUncontrolled: true });
  const mine = list.filter((c) => new URL(c.url).origin === self.location.origin);
  const client = mine.find((c) => c.focused) || mine.find((c) => c.visibilityState === 'visible') || mine[0];
  if (client) {
    try {
      await client.focus();
    } catch {
      /* focusing needs the click's activation; carry on */
    }
    const reply = await ask(client, { type: 'grouper:open', hash });
    if (reply && reply.ok) return;
    if (typeof client.navigate === 'function') {
      try {
        await client.navigate(url);
        return;
      } catch {
        /* not controlled by this worker: open a window instead */
      }
    }
  }
  await self.clients.openWindow(url);
}
