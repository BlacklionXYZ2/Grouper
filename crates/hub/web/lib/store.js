// Client state and a tiny pub/sub. Stored events (seq > 0) are kept per
// conversation; live-only deltas (seq 0) only fill drafts in memory.

export const store = {
  me: null, // {device_id, name, admin}
  hub: null, // GET /v1/hub
  online: false,
  everOnline: false,
  devices: [],
  devicesAt: 0,
  conversations: [],
  currentConversation: null, // id of the conversation currently open, or null
  threads: new Map(), // id -> ThreadInfo (+ checkpoint flag)
  threadsLoaded: false,
  models: [],
  convs: new Map(), // conversation id -> {events, lastSeq, loaded, catching, queue}
  drafts: new Map(), // conversation id -> streamed assistant text
  replying: new Set(), // conversation ids waiting for the orchestrator
  live: new Map(), // thread id -> {draft, lastText, tool}
  approvals: new Map(), // approval id -> {approval_id, device_id, tool, input, reason, thread_id, conversation_id, ts}
  answered: new Set(), // approval ids already resolved
  pendingProposals: 0,
};

/** Forget everything the last session loaded, so another sign-in starts clean. */
export function resetStore() {
  Object.assign(store, {
    me: null,
    hub: null,
    online: false,
    everOnline: false,
    devices: [],
    devicesAt: 0,
    conversations: [],
    currentConversation: null,
    threadsLoaded: false,
    models: [],
    pendingProposals: 0,
  });
  for (const key of ['threads', 'convs', 'drafts', 'live', 'approvals']) store[key].clear();
  for (const key of ['replying', 'answered']) store[key].clear();
}

const subs = new Map();

/** Subscribe to a topic; returns an unsubscribe function. */
export function on(topic, fn) {
  if (!subs.has(topic)) subs.set(topic, new Set());
  subs.get(topic).add(fn);
  return () => subs.get(topic)?.delete(fn);
}

export function emit(topic, payload) {
  for (const fn of [...(subs.get(topic) || [])]) {
    try {
      fn(payload);
    } catch (e) {
      console.error(`listener for ${topic} failed`, e);
    }
  }
}

export function conv(id) {
  if (!store.convs.has(id)) store.convs.set(id, { events: [], lastSeq: 0, loaded: false, catching: false, queue: [] });
  return store.convs.get(id);
}

export function liveOf(threadId) {
  if (!store.live.has(threadId)) store.live.set(threadId, { draft: '', lastText: '', tool: null });
  return store.live.get(threadId);
}

export function deviceName(id) {
  if (!id) return 'unknown device';
  if (id === 'hub') return 'Hub';
  return store.devices.find((d) => d.id === id)?.name || id;
}

export function deviceOnline(id) {
  if (id === 'hub') return true;
  return Boolean(store.devices.find((d) => d.id === id)?.online);
}

export function threadsIn(conversationId) {
  if (!conversationId) return [];
  return [...store.threads.values()].filter((t) => t.conversation_id === conversationId);
}

export function approvalsFor(threadId) {
  return [...store.approvals.values()].filter((a) => a.thread_id === threadId);
}

/** Pending approvals in one conversation, oldest first. */
export function approvalsIn(conversationId) {
  if (!conversationId) return [];
  return [...store.approvals.values()].filter((a) => a.conversation_id === conversationId).sort((a, b) => a.ts - b.ts);
}

/** Pending approvals across all conversations, oldest first. */
export function pendingApprovals() {
  return [...store.approvals.values()].sort((a, b) => a.ts - b.ts);
}

export function isAdmin() {
  return Boolean(store.me?.admin);
}
