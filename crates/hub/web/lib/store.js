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
  approvalsLoaded: false,
  answered: new Set(), // approval ids already answered (not dropped)
  resolutions: new Map(), // approval id -> {approved, reason}
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
    approvalsLoaded: false,
    models: [],
    pendingProposals: 0,
  });
  for (const key of ['threads', 'convs', 'drafts', 'live', 'approvals', 'resolutions']) store[key].clear();
  for (const key of ['replying', 'answered']) store[key].clear();
  emit('reset');
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

/**
 * Approval order everywhere (dock, inbox, every client): oldest request first,
 * equal times by approval id, so the queue reads the same on every device.
 */
export function byRequestOrder(a, b) {
  return (a.ts || 0) - (b.ts || 0) || (a.approval_id < b.approval_id ? -1 : a.approval_id > b.approval_id ? 1 : 0);
}

/** Pending approvals of one thread, oldest first. */
export function approvalsFor(threadId) {
  return [...store.approvals.values()].filter((a) => a.thread_id === threadId).sort(byRequestOrder);
}

/** Pending approvals in one conversation, oldest first. */
export function approvalsIn(conversationId) {
  if (!conversationId) return [];
  return [...store.approvals.values()].filter((a) => a.conversation_id === conversationId).sort(byRequestOrder);
}

/** Pending approvals across all conversations, oldest first. */
export function pendingApprovals() {
  return [...store.approvals.values()].sort(byRequestOrder);
}

/**
 * Threads waiting for a device that have no pending approval of their own (a
 * thread with an approval is counted, and listed, by that approval).
 */
export function waitingThreads() {
  const approvalThreads = new Set([...store.approvals.values()].map((a) => a.thread_id));
  return [...store.threads.values()].filter((t) => t.status === 'waiting_device' && !approvalThreads.has(t.id));
}

/** What needs the owner across all chats: pending approvals plus waiting threads. */
export function needsYouCount() {
  return store.approvals.size + waitingThreads().length;
}

export function isAdmin() {
  return Boolean(store.me?.admin);
}
