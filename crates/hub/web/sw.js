// Grouper's service worker: approval notifications only (gui-spec 5.14). Served
// at /sw.js so its scope is the whole app. It has no fetch handler, caches
// nothing and makes no network requests.
//
// push
//   {"type":"approval_requested", approval_id, conversation_id, thread_id, tool,
//    device, thread_title, chat_title, ts}
//     Shows "Grouper needs your review" / "{tool} on {device} · {thread} · {chat}"
//     with tag grouper-approval-<id> (a repeat replaces it quietly), unless the
//     id was already cleared (tombstone) or a visible, focused Grouper tab is on
//     that chat (the page is asked; it shows and announces the dock itself).
//   {"type":"approval_cleared", approval_id}
//     Closes that notification and records a tombstone, so a late or repeated
//     request push can't bring it back. Tombstones are bounded (count and age).
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
const TOMBSTONE_MAX = 200;
const TOMBSTONE_TTL_MS = 3 * 24 * 60 * 60 * 1000;
const ASK_TIMEOUT_MS = 700;

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

async function handlePush(data) {
  if (!data || typeof data !== 'object') return;
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
  if (!id) return;
  if (data.type === 'approval_cleared') {
    await addTombstone(id);
    await closeTagged(id);
    return;
  }
  if (data.type !== 'approval_requested') return;
  if (await isTombstoned(id)) return; // cleared before (or while) this arrived
  const conversationId = field(data.conversation_id, 120);
  if (conversationId && (await chatOnScreen(conversationId))) return;
  const tool = field(data.tool, 80) || 'A tool';
  const device = field(data.device, 80);
  const body = [device ? `${tool} on ${device}` : tool, field(data.thread_title), field(data.chat_title)].filter(Boolean).join(' · ');
  await self.registration.showNotification(TITLE, {
    body,
    tag: TAG_PREFIX + id,
    icon: '/app/icon.svg',
    data: { approval_id: id, conversation_id: conversationId, thread_id: field(data.thread_id, 120) },
  });
}

async function closeTagged(id) {
  const list = await self.registration.getNotifications({ tag: TAG_PREFIX + id });
  for (const n of list) n.close();
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

// ---- tombstones: bounded, in IndexedDB so they survive the worker being stopped

let memoryTombstones = [];

function openDb() {
  return new Promise((resolve, reject) => {
    const req = indexedDB.open('grouper-sw', 1);
    req.onupgradeneeded = () => req.result.createObjectStore('kv');
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error);
  });
}

async function readTombstones() {
  try {
    const db = await openDb();
    const list = await new Promise((resolve, reject) => {
      const req = db.transaction('kv', 'readonly').objectStore('kv').get('cleared');
      req.onsuccess = () => resolve(req.result);
      req.onerror = () => reject(req.error);
    });
    db.close();
    return Array.isArray(list) ? list : [];
  } catch {
    return memoryTombstones;
  }
}

async function writeTombstones(list) {
  memoryTombstones = list;
  try {
    const db = await openDb();
    await new Promise((resolve, reject) => {
      const tx = db.transaction('kv', 'readwrite');
      tx.objectStore('kv').put(list, 'cleared');
      tx.oncomplete = () => resolve();
      tx.onerror = () => reject(tx.error);
    });
    db.close();
  } catch {
    /* the in-memory copy still covers this worker's lifetime */
  }
}

async function addTombstone(id) {
  const now = Date.now();
  const list = (await readTombstones()).filter((t) => t && t.id !== id && now - t.at < TOMBSTONE_TTL_MS);
  list.push({ id, at: now });
  await writeTombstones(list.slice(-TOMBSTONE_MAX));
}

async function isTombstoned(id) {
  const now = Date.now();
  return (await readTombstones()).some((t) => t && t.id === id && now - t.at < TOMBSTONE_TTL_MS);
}
