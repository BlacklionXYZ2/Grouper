// The live event stream (`/v1/events`, signed in by the session cookie), the
// reducer that folds events into the store, and catch-up by seq after reconnects.
//
// The socket's cursor starts at the hub's latest seq when it opens, so anything
// stored while it was down is fetched over HTTP (`?after=lastSeq`). Events that
// arrive on the socket while a fetch is in flight (the first load or a catch-up)
// are queued, then applied with those at or below the fetched seq dropped. A
// failed catch-up keeps queueing and is retried, so no gap is papered over.
// Deltas (seq 0) are never stored: the stored message replaces the draft.
//
// After a few failed reconnects the session is probed (`GET /v1/session`): if it
// expired or its token was revoked, `session_ended` is emitted instead of retrying
// forever. Device presence is polled here too (one poller for every view), paused
// while the page is hidden.

import { get, qs, enc } from './api.js';
import { store, emit, conv, liveOf } from './store.js';
import { isActive } from './format.js';

let socket = null;
let attempts = 0;
let reconnectTimer = null;
let keepAlive = null;
let stopped = true;
let abort = null; // window/document listeners of this sign-in
const retries = new Map(); // job name -> timer

export function startLive() {
  if (!stopped) return;
  stopped = false;
  abort = new AbortController();
  const { signal } = abort;
  window.addEventListener('online', () => reconnectNow(), { signal });
  document.addEventListener(
    'visibilitychange',
    () => {
      if (document.visibilityState !== 'visible') return;
      if (!store.online) reconnectNow();
      else if (Date.now() - store.devicesAt > pollEvery()) pollDevices();
    },
    { signal },
  );
  connect();
  schedulePoll();
}

export function stopLive() {
  stopped = true;
  abort?.abort();
  abort = null;
  clearTimeout(reconnectTimer);
  clearInterval(keepAlive);
  clearTimeout(pollTimer);
  clearTimeout(devicesTimer);
  for (const t of retries.values()) clearTimeout(t);
  retries.clear();
  attempts = 0;
  const ws = socket;
  socket = null;
  ws?.close();
  setOnline(false);
}

function reconnectNow() {
  if (stopped || store.online) return;
  clearTimeout(reconnectTimer);
  attempts = 0;
  connect();
}

function connect() {
  if (stopped) return;
  if (socket && (socket.readyState === WebSocket.OPEN || socket.readyState === WebSocket.CONNECTING)) return;
  const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
  const ws = new WebSocket(`${proto}//${location.host}/v1/events`);
  socket = ws;
  ws.addEventListener('open', () => {
    if (socket !== ws) return;
    attempts = 0;
    setOnline(true);
    clearInterval(keepAlive);
    // Proxies (Cloudflare) drop idle sockets; the hub ignores text it receives.
    keepAlive = setInterval(() => {
      if (ws.readyState === WebSocket.OPEN) ws.send('ping');
    }, 25000);
    resync();
  });
  ws.addEventListener('message', (m) => {
    let event;
    try {
      event = JSON.parse(m.data);
    } catch {
      return;
    }
    ingest(event, { derive: true });
  });
  ws.addEventListener('close', () => {
    if (socket !== ws) return;
    socket = null;
    clearInterval(keepAlive);
    setOnline(false);
    // Drafts can't be finished without the socket; the stored text repairs them.
    store.drafts.clear();
    for (const l of store.live.values()) l.draft = '';
    // Nor can "replying" end without it; catch-up works it out again.
    const replying = [...store.replying];
    store.replying.clear();
    for (const id of replying) emit('replying', id);
    if (stopped) return;
    attempts++;
    if (attempts >= 2) probeSession();
    const delay = Math.min(15000, 1000 * 2 ** (attempts - 1)) + Math.random() * 500;
    reconnectTimer = setTimeout(connect, delay);
  });
}

/** After failed reconnects: is the session still good? A 401 here is handled by the API listener too. */
async function probeSession() {
  try {
    const me = await get('/v1/session');
    if (!stopped && !me?.signed_in) sessionEnded();
  } catch (e) {
    if (!stopped && e.status === 401) sessionEnded();
    // Anything else (the hub is down): keep reconnecting.
  }
}

function sessionEnded() {
  stopLive();
  emit('session_ended');
}

function setOnline(online) {
  if (store.online === online) return;
  store.online = online;
  if (online) store.everOnline = true;
  emit('online', online);
}

/**
 * Run `job` now; if it fails while we're live, try again with backoff
 * (2 s, 4 s … 30 s) until it works, the socket drops (resync runs it again) or
 * live updates stop.
 */
function retrying(name, job, attempt = 0) {
  clearTimeout(retries.get(name));
  retries.delete(name);
  return job().catch((e) => {
    if (stopped || !store.online) return;
    const delay = Math.min(30000, 2000 * 2 ** attempt);
    retries.set(
      name,
      setTimeout(() => {
        retries.delete(name);
        if (!stopped && store.online) retrying(name, job, attempt + 1);
      }, delay),
    );
    console.warn(`${name} failed; retrying in ${Math.round(delay / 1000)} s`, e);
  });
}

/** After (re)connecting: fill gaps in loaded conversations and refresh lists. */
function resync() {
  retrying('threads', () => loadThreads().then(() => scanApprovals({ strict: true })));
  retrying('conversations', loadConversations);
  retrying('devices', loadDevices);
  for (const [id, c] of store.convs) {
    if (c.loaded) retrying(`catch-up ${id}`, () => catchUp(id));
  }
}

// ---- reducer

/** Fold one event into the store and tell the views. */
export function ingest(e, { derive }) {
  if (derive) applyDerived(e);
  if (e.seq > 0) {
    const c = store.convs.get(e.conversation_id);
    // While the first load or a catch-up is fetching, queue: the fetch may end
    // before this event, and dropping it would leave a hole.
    if (c && c.catching) c.queue.push(e);
    else if (c && c.loaded) store_(c, e);
  }
  emit('event', e);
}

function store_(c, e) {
  if (e.seq <= c.lastSeq) return false;
  c.events.push(e);
  c.lastSeq = e.seq;
  emit('stored', e);
  return true;
}

function applyDerived(e) {
  const cid = e.conversation_id;
  const tid = e.thread_id;
  switch (e.kind) {
    case 'user_message':
      store.replying.add(cid);
      bumpConversation(cid);
      emit('replying', cid);
      break;
    case 'assistant_delta':
      store.drafts.set(cid, (store.drafts.get(cid) || '') + e.text);
      if (!store.replying.has(cid)) {
        store.replying.add(cid);
        emit('replying', cid);
      }
      break;
    case 'assistant_message':
      store.drafts.delete(cid);
      break;
    case 'turn_finished':
      store.drafts.delete(cid);
      store.replying.delete(cid);
      emit('replying', cid);
      break;
    case 'error':
      store.drafts.delete(cid);
      break;
    case 'thread_started':
      if (!store.devices.some((d) => d.id === e.device_id && d.online)) refreshDevicesSoon();
      upsertThread(tid, {
        id: tid,
        conversation_id: cid,
        title: e.title,
        device_id: e.device_id,
        model: e.model,
        reason: e.reason,
        status: 'running',
        created: e.ts,
      });
      break;
    case 'thread_activity': {
      const l = liveOf(tid);
      const a = e.event;
      if (a.type === 'text_delta') l.draft += a.text;
      else if (a.type === 'text') {
        l.lastText = a.text;
        l.draft = '';
      } else if (a.type === 'tool_call') l.tool = a.name;
      else if (a.type === 'tool_result') l.tool = null;
      break;
    }
    case 'thread_waiting':
      upsertThread(tid, { status: 'waiting_device', checkpoint: false });
      refreshDevicesSoon();
      break;
    case 'thread_checkpoint':
      upsertThread(tid, { status: 'waiting_device', checkpoint: true });
      break;
    case 'thread_handed_over':
      upsertThread(tid, { status: 'running' });
      refreshDevicesSoon();
      break;
    case 'thread_finished':
      upsertThread(tid, { status: e.status, summary: e.summary, finished: e.ts });
      for (const [id, a] of store.approvals) if (a.thread_id === tid) store.approvals.delete(id);
      liveOf(tid).tool = null;
      liveOf(tid).draft = '';
      emit('approvals');
      break;
    case 'approval_requested':
      if (!store.answered.has(e.approval_id)) {
        store.approvals.set(e.approval_id, {
          approval_id: e.approval_id,
          device_id: e.device_id,
          tool: e.tool,
          input: e.input,
          reason: e.reason,
          thread_id: tid,
          conversation_id: cid,
          ts: e.ts,
        });
        emit('approvals');
      }
      break;
    case 'approval_resolved':
      store.answered.add(e.approval_id);
      if (store.approvals.delete(e.approval_id)) emit('approvals');
      emit('approval_resolved', e);
      break;
    default:
      break;
  }
}

/**
 * Whether the orchestrator is mid-turn, from the stored events: a user message
 * (or a thread result it wakes on) after the last finished turn.
 */
function reconcileReplying(id) {
  const c = store.convs.get(id);
  if (!c?.loaded) return;
  let replying = false;
  for (const e of c.events) {
    if (e.kind === 'user_message') replying = true;
    else if (e.kind === 'turn_finished') replying = false;
  }
  if (replying === store.replying.has(id)) return;
  if (replying) store.replying.add(id);
  else store.replying.delete(id);
  emit('replying', id);
}

// Presence isn't on the event stream; re-read devices when a thread hints it changed.
let devicesTimer = null;
function refreshDevicesSoon() {
  clearTimeout(devicesTimer);
  devicesTimer = setTimeout(() => loadDevices().catch(() => {}), 300);
}

function upsertThread(id, fields) {
  if (!id) return;
  const t = { ...(store.threads.get(id) || {}), ...fields };
  store.threads.set(id, t);
  emit('threads');
}

function bumpConversation(id) {
  if (!store.conversations.some((c) => c.id === id)) loadConversations().catch(() => {});
}

// ---- device presence poller (one for the whole app)

let pollTimer = null;
let fastPollers = 0;

function pollEvery() {
  return fastPollers > 0 ? 15000 : 30000;
}

function schedulePoll() {
  clearTimeout(pollTimer);
  if (stopped) return;
  pollTimer = setTimeout(pollDevices, pollEvery());
}

async function pollDevices() {
  clearTimeout(pollTimer);
  if (!stopped && store.online && !document.hidden) await loadDevices().catch(() => {});
  schedulePoll();
}

/** A view that shows presence asks for the 15 s cadence; call the result to stop. */
export function wantFastDevicePolling() {
  fastPollers++;
  schedulePoll();
  let done = false;
  return () => {
    if (done) return;
    done = true;
    fastPollers--;
    schedulePoll();
  };
}

// ---- loaders

export async function loadConversations() {
  store.conversations = (await get('/v1/conversations')) || [];
  store.conversations.sort((a, b) => b.created - a.created);
  emit('conversations');
}

export async function loadThreads() {
  const list = (await get('/v1/threads')) || [];
  const next = new Map();
  for (const t of list) {
    const old = store.threads.get(t.id);
    next.set(t.id, { ...t, checkpoint: t.status === 'waiting_device' ? Boolean(old?.checkpoint) : false });
  }
  store.threads = next;
  emit('threads');
}

export async function loadDevices() {
  store.devices = (await get('/v1/devices')) || [];
  store.devicesAt = Date.now();
  emit('devices');
}

export async function loadModels() {
  store.models = (await get('/v1/models')) || [];
  emit('models');
}

/** Load a conversation's stored events once; later calls catch up. */
export async function loadConversation(id) {
  const c = conv(id);
  if (c.loaded) return catchUp(id);
  if (c.catching) return undefined;
  c.catching = true;
  c.queue = [];
  try {
    const events = (await get(`/v1/conversations/${enc(id)}/events`)) || [];
    c.events = events;
    c.lastSeq = events.length ? events[events.length - 1].seq : 0;
    c.loaded = true;
    for (const e of events) {
      if (e.kind === 'thread_checkpoint' && store.threads.get(e.thread_id)?.status === 'waiting_device') {
        store.threads.get(e.thread_id).checkpoint = true;
      }
      if (e.kind === 'thread_activity' && e.event.type === 'text') liveOf(e.thread_id).lastText = e.event.text;
    }
  } catch (err) {
    c.catching = false;
    c.queue = [];
    throw err;
  }
  c.catching = false;
  const queued = c.queue;
  c.queue = [];
  for (const e of queued) store_(c, e);
  reconcileReplying(id);
  emit('loaded', id);
  return undefined;
}

async function catchUp(id) {
  const c = conv(id);
  if (!c.loaded || c.fetching) return;
  c.fetching = true;
  if (!c.catching) {
    c.catching = true;
    c.queue = [];
  }
  try {
    const events = (await get(`/v1/conversations/${enc(id)}/events${qs({ after: c.lastSeq })}`)) || [];
    for (const e of events) {
      if (e.seq > c.lastSeq) {
        applyDerived(e);
        store_(c, e);
        emit('event', e);
      }
    }
  } finally {
    c.fetching = false;
  }
  // Only a successful fetch ends the catch-up: on failure the queue keeps the live
  // events until the retry fills the gap before them.
  c.catching = false;
  const queued = c.queue;
  c.queue = [];
  for (const e of queued) store_(c, e);
  reconcileReplying(id);
}

/**
 * Approvals waiting now. There is no list route, so read the events of each
 * running or waiting thread and keep the requests nobody has answered.
 */
export async function scanApprovals({ strict = false } = {}) {
  const active = [...store.threads.values()].filter((t) => isActive(t.status));
  const found = new Map();
  const results = await Promise.allSettled(
    active.map(async (t) => {
      const events = (await get(`/v1/conversations/${enc(t.conversation_id)}/events${qs({ thread: t.id })}`)) || [];
      const open = new Map();
      for (const e of events) {
        if (e.kind === 'approval_requested') open.set(e.approval_id, e);
        if (e.kind === 'approval_resolved') {
          open.delete(e.approval_id);
          store.answered.add(e.approval_id);
        }
        if (e.kind === 'thread_checkpoint') store.threads.get(t.id).checkpoint = true;
        if (e.kind === 'thread_handed_over') store.threads.get(t.id).checkpoint = false;
        if (e.kind === 'thread_activity' && e.event.type === 'text') liveOf(t.id).lastText = e.event.text;
      }
      for (const e of open.values()) {
        found.set(e.approval_id, {
          approval_id: e.approval_id,
          device_id: e.device_id,
          tool: e.tool,
          input: e.input,
          reason: e.reason,
          thread_id: e.thread_id,
          conversation_id: e.conversation_id,
          ts: e.ts,
        });
      }
    }),
  );
  // Keep requests that arrived on the socket meanwhile, and those of threads we
  // couldn't read this time.
  const unread = new Set(active.filter((_, i) => results[i].status === 'rejected').map((t) => t.id));
  for (const [id, a] of store.approvals) {
    if (!found.has(id) && !store.answered.has(id) && (unread.has(a.thread_id) || isActive(store.threads.get(a.thread_id)?.status))) found.set(id, a);
  }
  store.approvals = found;
  emit('approvals');
  emit('threads');
  const failed = results.find((r) => r.status === 'rejected');
  if (failed && strict) throw failed.reason;
}
