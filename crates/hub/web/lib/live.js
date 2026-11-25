// The live event stream (`/v1/events`, signed in by the session cookie), the
// reducer that folds events into the store, and catch-up by seq after reconnects.
//
// The socket's cursor starts at the hub's latest seq when it opens, so anything
// stored while it was down is fetched over HTTP (`?after=lastSeq`). Events that
// arrive on the socket while a fetch is in flight are queued, then applied with
// those at or below the fetched seq dropped. Deltas (seq 0) are never stored: the
// stored message replaces the draft.

import { get, qs, enc } from './api.js';
import { store, emit, conv, liveOf } from './store.js';
import { isActive } from './format.js';

let socket = null;
let attempts = 0;
let reconnectTimer = null;
let keepAlive = null;
let stopped = false;

export function startLive() {
  stopped = false;
  connect();
  window.addEventListener('online', () => reconnectNow());
  document.addEventListener('visibilitychange', () => {
    if (document.visibilityState === 'visible' && !store.online) reconnectNow();
  });
}

export function stopLive() {
  stopped = true;
  clearTimeout(reconnectTimer);
  clearInterval(keepAlive);
  if (socket) socket.close();
  socket = null;
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
    if (!stopped) {
      const delay = Math.min(15000, 1000 * 2 ** attempts) + Math.random() * 500;
      attempts++;
      reconnectTimer = setTimeout(connect, delay);
    }
  });
}

function setOnline(online) {
  if (store.online === online) return;
  store.online = online;
  if (online) store.everOnline = true;
  emit('online', online);
}

/** After (re)connecting: fill gaps in loaded conversations and refresh lists. */
async function resync() {
  const jobs = [loadThreads().then(scanApprovals), loadConversations(), loadDevices()];
  for (const [id, c] of store.convs) {
    if (c.loaded) jobs.push(catchUp(id));
  }
  await Promise.allSettled(jobs);
}

// ---- reducer

/** Fold one event into the store and tell the views. */
export function ingest(e, { derive }) {
  if (derive) applyDerived(e);
  if (e.seq > 0) {
    const c = store.convs.get(e.conversation_id);
    if (c && c.loaded) {
      if (c.catching) {
        c.queue.push(e);
      } else {
        store_(c, e);
      }
    }
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
  if (!store.conversations.some((c) => c.id === id)) loadConversations();
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
  } finally {
    c.catching = false;
    const queued = c.queue;
    c.queue = [];
    for (const e of queued) store_(c, e);
  }
  emit('loaded', id);
}

async function catchUp(id) {
  const c = conv(id);
  if (!c.loaded || c.catching) return;
  c.catching = true;
  c.queue = [];
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
    c.catching = false;
    const queued = c.queue;
    c.queue = [];
    for (const e of queued) store_(c, e);
  }
}

/**
 * Approvals waiting now. There is no list route, so read the events of each
 * running or waiting thread and keep the requests nobody has answered.
 */
export async function scanApprovals() {
  const active = [...store.threads.values()].filter((t) => isActive(t.status));
  const found = new Map();
  await Promise.allSettled(
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
  // Keep requests that arrived on the socket meanwhile.
  for (const [id, a] of store.approvals) {
    if (!found.has(id) && !store.answered.has(id) && isActive(store.threads.get(a.thread_id)?.status)) found.set(id, a);
  }
  store.approvals = found;
  emit('approvals');
  emit('threads');
}
