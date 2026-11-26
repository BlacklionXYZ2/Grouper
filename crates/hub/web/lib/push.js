// Approval notifications in this browser (gui-spec 5.14).
//
// Web Push goes through the service worker at /sw.js (scope /), subscribed with the
// hub's VAPID key and registered with the hub (`/v1/push/...`). The owner opts in
// from Settings > Notifications only: permission is never asked on load, at
// sign-in or because an approval arrived. Each browser opts in for itself
// (`grouper.notify` in this browser's storage).
//
// Notifications carry only names (tool, device, thread, chat), never the input or
// the reason, and have no Approve/Deny actions: a tap opens the chat with the
// dock on that request (`#/chat/<id>?approval=<approval id>`).
//
// The page:
//   - answers the service worker's "where are you?" (so a push for the chat that
//     is visible and focused here is not shown; the dock announces it instead), and
//     its "open this route" after a notification tap;
//   - shows a notification itself only when this browser has no push (Notification
//     permission granted but no PushManager) and the request's chat isn't the one
//     on screen in a focused tab;
//   - closes a request's notification when it is answered or its thread ends;
//   - on sign-out, deletes this browser's subscription on the hub and unsubscribes
//     (best effort, bounded so sign-out never hangs).

import { get, post, del, qs, enc } from './api.js';
import { store, on, deviceName } from './store.js';
import { navigate } from './ui.js';

const OPT_IN = 'grouper.notify'; // '1': this browser opted in
const SUB_ID = 'grouper.push.id'; // the hub's id for this browser's subscription
const SW_URL = '/sw.js';
export const TITLE = 'Grouper needs your review';

function readKey(key) {
  try {
    return localStorage.getItem(key);
  } catch {
    return null;
  }
}

function writeKey(key, value) {
  try {
    if (value === null || value === undefined) localStorage.removeItem(key);
    else localStorage.setItem(key, String(value));
  } catch {
    /* storage unavailable */
  }
}

export function optedIn() {
  return readKey(OPT_IN) === '1';
}

export function storedSubscriptionId() {
  return readKey(SUB_ID);
}

export function isIos() {
  const ua = navigator.userAgent || '';
  return /iPhone|iPad|iPod/.test(ua) || (navigator.platform === 'MacIntel' && navigator.maxTouchPoints > 1);
}

export function isStandalone() {
  return navigator.standalone === true || Boolean(window.matchMedia?.('(display-mode: standalone)').matches);
}

/**
 * What this browser can do:
 *   'push'             Web Push (service worker + PushManager + Notification)
 *   'page-only'        notifications only while Grouper is open (no PushManager)
 *   'ios-home-screen'  iPhone/iPad Safari: needs Grouper on the Home Screen first
 *   'insecure'         not a secure context (plain http to a non-localhost hub)
 *   'unsupported'      no web notifications at all
 */
export function support() {
  const hasNotification = 'Notification' in window;
  const hasPush = window.isSecureContext && 'serviceWorker' in navigator && 'PushManager' in window && hasNotification;
  if (hasPush) return 'push';
  if (isIos() && !isStandalone()) return 'ios-home-screen';
  if (!window.isSecureContext) return 'insecure';
  if (hasNotification) return 'page-only';
  return 'unsupported';
}

export function permission() {
  return 'Notification' in window ? Notification.permission : 'unsupported';
}

/** Show the dock's "turn on notifications" hint: possible here, not decided yet. */
export function notifyHintWanted() {
  const kind = support();
  return Boolean(store.me?.admin) && (kind === 'push' || kind === 'page-only') && permission() === 'default' && !optedIn();
}

async function existingRegistration() {
  if (!('serviceWorker' in navigator)) return null;
  try {
    return (await navigator.serviceWorker.getRegistration('/')) || null;
  } catch {
    return null;
  }
}

/** This browser's push subscription, if any. */
export async function currentSubscription() {
  const reg = await existingRegistration();
  if (!reg?.pushManager) return null;
  try {
    return await reg.pushManager.getSubscription();
  } catch {
    return null;
  }
}

function base64UrlBytes(text) {
  const s = String(text || '').replace(/-/g, '+').replace(/_/g, '/');
  const bin = atob(s + '='.repeat((4 - (s.length % 4)) % 4));
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

function sameKey(buffer, bytes) {
  if (!buffer) return false;
  const a = new Uint8Array(buffer);
  return a.length === bytes.length && a.every((v, i) => v === bytes[i]);
}

/** "Chrome on Windows": how this browser is listed on the hub. */
export function browserLabel() {
  const ua = navigator.userAgent || '';
  const browser = /Edg\//.test(ua) ? 'Edge' : /OPR\//.test(ua) ? 'Opera' : /Firefox\//.test(ua) ? 'Firefox' : /Chrome\//.test(ua) ? 'Chrome' : /Safari\//.test(ua) ? 'Safari' : 'Browser';
  const os = isIos() ? 'iOS' : /Android/.test(ua) ? 'Android' : /Windows/.test(ua) ? 'Windows' : /Mac OS X/.test(ua) ? 'macOS' : /Linux/.test(ua) ? 'Linux' : '';
  return os ? `${browser} on ${os}` : browser;
}

/** Subscribe (or refresh the subscription) and register it with the hub. Permission must already be granted. */
async function subscribeAndRegister() {
  const reg = await navigator.serviceWorker.register(SW_URL, { scope: '/' });
  await navigator.serviceWorker.ready;
  const { public_key: publicKey } = (await get('/v1/push/vapid-key')) || {};
  if (!publicKey) throw new Error('The hub has no notification key yet.');
  const key = base64UrlBytes(publicKey);
  let sub = await reg.pushManager.getSubscription();
  if (sub && !sameKey(sub.options?.applicationServerKey, key)) {
    // The hub's key changed: the old subscription can't be used any more.
    await sub.unsubscribe().catch(() => {});
    sub = null;
  }
  if (!sub) sub = await reg.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: key });
  const row = await post('/v1/push/subscriptions', { ...sub.toJSON(), label: browserLabel() });
  writeKey(SUB_ID, row?.id ?? null);
  writeKey(OPT_IN, '1');
  return row;
}

/**
 * Turn approval notifications on for this browser. Call only from a click: the
 * browser's permission prompt is the first thing it does.
 * Resolves {permission, mode: 'push' | 'page' | null, row}.
 */
export async function enableNotifications() {
  const kind = support();
  if (kind !== 'push' && kind !== 'page-only') return { permission: permission(), mode: null };
  let perm = Notification.permission;
  if (perm === 'default') perm = await Notification.requestPermission();
  if (perm !== 'granted') return { permission: perm, mode: null };
  if (kind === 'page-only') {
    writeKey(OPT_IN, '1');
    return { permission: perm, mode: 'page' };
  }
  const row = await subscribeAndRegister();
  return { permission: perm, mode: 'push', row };
}

/** Turn them off here: delete the hub's row for this browser, then unsubscribe. */
export async function disableNotifications() {
  writeKey(OPT_IN, null);
  closeAll();
  const sub = await currentSubscription();
  writeKey(SUB_ID, null);
  if (!sub) return;
  try {
    await del(`/v1/push/subscriptions${qs({ endpoint: sub.endpoint })}`);
  } finally {
    // Even if the hub couldn't be told, this browser stops receiving: the hub
    // drops the endpoint when the push service reports it gone.
    await sub.unsubscribe().catch(() => {});
  }
}

/**
 * Sign-out or a revoked session. With `server` (the session still works), delete
 * the hub's row first. Never takes more than a couple of seconds.
 */
export async function forgetNotifications({ server = true } = {}) {
  writeKey(OPT_IN, null);
  writeKey(SUB_ID, null);
  closeAll();
  const work = (async () => {
    const sub = await currentSubscription();
    if (!sub) return;
    if (server) await del(`/v1/push/subscriptions${qs({ endpoint: sub.endpoint })}`).catch(() => {});
    await sub.unsubscribe().catch(() => {});
  })().catch(() => {});
  await Promise.race([work, new Promise((resolve) => setTimeout(resolve, 2500))]);
}

export function listSubscriptions(opts) {
  return get('/v1/push/subscriptions', opts);
}

export function removeSubscription(id) {
  return del(`/v1/push/subscriptions/${enc(id)}`);
}

/** Ask the hub to push a test to this browser (or every subscription without an id). */
export function sendTest(id) {
  return post('/v1/push/test', id ? { id } : {});
}

/** A local test for browsers without push (the page shows it itself). */
export async function showLocalTest() {
  await show('test', { body: 'Test notification. Approval alerts look like this and never show the command.' }, '#/settings');
}

// ---- notifications the page shows itself (browsers without push), and closing

const pageNotes = new Map(); // tag -> Notification

function tagOf(approvalId) {
  return `grouper-approval-${approvalId}`;
}

async function show(id, { body }, hash) {
  const tag = tagOf(id);
  const reg = await existingRegistration();
  if (reg?.showNotification) {
    await reg.showNotification(TITLE, { body, tag, icon: '/app/icon.svg', data: { hash } });
    return;
  }
  try {
    const n = new Notification(TITLE, { body, tag, icon: '/app/icon.svg' });
    n.onclick = () => {
      window.focus();
      navigate(hash);
      n.close();
    };
    n.onclose = () => pageNotes.delete(tag);
    pageNotes.set(tag, n);
  } catch {
    /* this browser only shows notifications from a service worker */
  }
}

async function closeTagged(approvalId) {
  const tag = tagOf(approvalId);
  pageNotes.get(tag)?.close();
  pageNotes.delete(tag);
  const reg = await existingRegistration();
  try {
    for (const n of (await reg?.getNotifications({ tag })) || []) n.close();
  } catch {
    /* nothing to close */
  }
}

function closeAll() {
  for (const n of pageNotes.values()) n.close();
  pageNotes.clear();
  existingRegistration()
    .then((reg) => reg?.getNotifications())
    .then((list) => {
      for (const n of list || []) if (String(n.tag).startsWith('grouper-approval-')) n.close();
    })
    .catch(() => {});
}

function bodyOf(e) {
  const thread = store.threads.get(e.thread_id);
  const chat = store.conversations.find((c) => c.id === e.conversation_id);
  return [`${e.tool} on ${deviceName(e.device_id)}`, thread?.title, chat?.title].filter(Boolean).join(' · ');
}

// ---- wiring for one sign-in

let hasPush = false; // this browser has a live push subscription (then the hub's push is used)
let off = null;

/** Where the page is, for the service worker's "suppress if focused on that chat". */
function whereAmI() {
  return {
    signedIn: Boolean(store.me),
    conversation: store.me ? store.currentConversation : null,
    visible: document.visibilityState === 'visible',
    focused: document.hasFocus(),
  };
}

/** Only our own routes, from our own service worker. */
function safeHash(hash) {
  return typeof hash === 'string' && /^#\/(chat\/[^#\s]*|inbox|settings)$/.test(hash) ? hash : null;
}

function onWorkerMessage(e) {
  const msg = e.data;
  if (!msg || typeof msg !== 'object') return;
  if (msg.type === 'grouper:where') {
    e.ports?.[0]?.postMessage(whereAmI());
  } else if (msg.type === 'grouper:open') {
    const hash = safeHash(msg.hash);
    e.ports?.[0]?.postMessage({ ok: Boolean(hash) });
    if (hash) {
      window.focus();
      navigate(hash);
    }
  }
}

/** Start for this sign-in: listen to the worker, refresh an opted-in subscription, close settled ones. */
export function startNotifications() {
  stopNotifications();
  const subs = [];
  if ('serviceWorker' in navigator) {
    navigator.serviceWorker.addEventListener('message', onWorkerMessage);
    navigator.serviceWorker.startMessages?.();
    subs.push(() => navigator.serviceWorker.removeEventListener('message', onWorkerMessage));
  }
  currentSubscription().then((sub) => {
    hasPush = Boolean(sub);
    // Re-register an opted-in browser with the hub, without a prompt (permission is already granted).
    if (optedIn() && support() === 'push' && permission() === 'granted' && store.me?.admin) {
      subscribeAndRegister()
        .then(() => {
          hasPush = true;
        })
        .catch(() => {
          /* Settings shows the state; the dock and inbox still work */
        });
    }
  });
  subs.push(
    on('event', (e) => {
      if (e.kind === 'approval_requested') maybeShowFromPage(e);
      else if (e.kind === 'approval_resolved') closeTagged(e.approval_id);
    }),
    on('approvals', () => {
      // A thread that ended drops its requests: close their notifications too.
      for (const tag of pageNotes.keys()) {
        const id = tag.slice('grouper-approval-'.length);
        if (!store.approvals.has(id)) closeTagged(id);
      }
    }),
  );
  off = () => subs.forEach((fn) => fn());
}

export function stopNotifications() {
  off?.();
  off = null;
}

/** Browsers without push: the open page notifies, unless that chat is on screen and focused. */
function maybeShowFromPage(e) {
  if (hasPush || !optedIn() || permission() !== 'granted' || store.answered.has(e.approval_id)) return;
  const here = document.visibilityState === 'visible' && document.hasFocus() && store.currentConversation === e.conversation_id;
  if (here) return;
  const hash = e.conversation_id ? `#/chat/${enc(e.conversation_id)}?approval=${enc(e.approval_id)}` : '#/inbox';
  show(e.approval_id, { body: bodyOf(e) }, hash).catch(() => {});
}

/** Settings calls this after turning push on or off here. */
export function setHasPush(value) {
  hasPush = Boolean(value);
}
