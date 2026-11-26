// The approval dock (gui-spec 4.3, 5.13). While a chat has pending approvals the
// dock takes the composer's place at the bottom of the chat; in a thread view it
// takes the place of that thread's step-in composer while the thread's own request
// waits. It is inline, never a modal, and can't be collapsed or swapped for a
// reply: the owner answers, stops the thread, or goes somewhere else.
//
// One request at a time, oldest first ("1 of N", Previous/Next, no wrapping).
// Order, top to bottom: "Run this tool?"; tool and device; thread with its model
// (family mark and name); the chat, in a thread view; the hub's reason, most
// prominent; the exact input behind a "Review exact input before approving"
// disclosure; the timeout and what happens then; then the queue controls, Approve
// / Deny, and Change it / Stop thread.
//
// Safety rules kept here:
//   - Approve stays disabled until the exact input has been expanded (an
//     acknowledgement gate, not proof it was read). Opening the dock never answers.
//   - On a Manual tool Approve is the primary button; when a risk rule asked,
//     Approve and Deny look the same.
//   - Enter never approves (it's swallowed on Approve) and there's no shortcut.
//     Escape only folds the input disclosure; it never answers or hides the dock.
//   - Approve/Deny turn off on the first press. A failed write is reconciled with
//     the hub (GET /v1/approvals) before the buttons come back; nothing is retried.
//   - Settled states: answered here (brief confirmation, then the next request),
//     "Answered on another device", "Thread ended; this request can't run".
//     Offline, after a reconnect until the hub's list is fetched again, and for
//     device-token sessions, the details stay and every write is off.
//
// The dock renders only from `store.approvals` (the hub's list plus live events);
// a deep link (`?approval=<id>` on a chat or thread route) refreshes that list
// before it says "This request is no longer waiting", and never builds a request
// from a notification's text.

import { h, icon, setChildren, setText, srOnly } from '../lib/dom.js';
import { get, post, enc, describe } from '../lib/api.js';
import { store, on, emit, deviceName, isAdmin, approvalsIn, approvalsFor } from '../lib/store.js';
import { loadApprovals } from '../lib/live.js';
import { announce } from '../lib/ui.js';
import { pretty, ago, duration, isActive } from '../lib/format.js';
import { familyOfId, familyMark, familyLabel, setFamilyClass } from '../lib/family.js';
import { notifyHintWanted } from '../lib/push.js';
import { setStepInDraft, stopThread } from './thread.js';

export const CHANGE_DRAFT = 'Don’t run that. Instead: ';

const SETTLE_HERE_MS = 1400; // "Approved." before the next request shows
const SETTLE_ELSEWHERE_MS = 3200; // "Answered on another device." stays a little longer
const COUNTDOWN_MS = 15000; // the timeout line is refreshed this often

// Shared by every dock (a chat and its thread view can show the same request).
const acknowledged = new Set(); // approval ids whose exact input was expanded
const outcomes = new Map(); // approval id -> what was done here ("Approved.")
const announcedIds = new Set(); // approval ids already announced as new
let dockUid = 0;
let limitsCache = null; // {secs, action} from /v1/limits, for live requests
let limitsLoading = null;

/**
 * The hub's reason for a Manual tool is "<tool> is set to Manual", maybe followed
 * by what Grouper changed (core-agent permission.rs). Anything else came from a
 * risk rule or a folder gate. No reason at all is treated as Manual, without
 * claiming a rule fired.
 */
export function isManualReason(reason) {
  return !reason || / is set to Manual( \(Grouper set [^)]*\))?$/.test(reason);
}

/** Where an approval is answered: its chat, with the dock on that request. */
export function approvalHref(a) {
  const cid = a.conversation_id || store.threads.get(a.thread_id)?.conversation_id;
  if (cid) return `#/chat/${enc(cid)}?approval=${enc(a.approval_id)}`;
  return `#/threads/${enc(a.thread_id)}?approval=${enc(a.approval_id)}`;
}

function alertOnce(text) {
  const region = document.getElementById('alert-region');
  if (!region) return;
  region.textContent = '';
  setTimeout(() => {
    region.textContent = text;
  }, 30);
}

function loadLimits() {
  if (limitsCache || limitsLoading) return limitsLoading;
  limitsLoading = get('/v1/limits')
    .then((report) => {
      limitsCache = { secs: report?.limits?.approval_timeout_secs ?? null, action: report?.limits?.approval_timeout_action ?? null };
    })
    .catch(() => {
      /* keep "Waiting for an answer" */
    })
    .finally(() => {
      limitsLoading = null;
    });
  return limitsLoading;
}

/** The timeout line: factual when the hub gave a limit, otherwise only "Waiting for an answer." */
function timeoutText(a) {
  let secs = a.timeout_secs;
  let action = a.timeout_action;
  const deadline = a.timeout_at;
  if (secs === undefined) {
    // A live request doesn't carry the limit; the hub's configured one applies.
    secs = limitsCache?.secs ?? null;
    action = limitsCache?.action ?? null;
  }
  if (!secs || !a.ts) return 'Waiting for an answer.';
  const target = deadline || a.ts + secs;
  const left = Math.round(target - Date.now() / 1000);
  if (left <= 0) return `Requested ${ago(a.ts)}. The ${duration(secs)} limit has passed; waiting for the hub to close the request.`;
  const then = action === 'stop' ? 'it’s denied and the thread is stopped' : 'it’s denied and the thread goes on';
  return `Requested ${ago(a.ts)}. If nobody answers within ${duration(secs)} (about ${duration(left)} left), ${then}.`;
}

/**
 * Mount a dock. `threadId` makes it a thread view's dock (that thread's requests
 * only, plus a way back to the chat's queue when the chat has others).
 *
 * - `onActiveChange(active, {hadFocus})`: the dock appeared or went away; the
 *   owner hides or restores its composer (and its draft and selection).
 * - `onChangeIt(approval)`: after "Change it" denied the request. The default
 *   opens the thread with the draft in its step-in field.
 *
 * Returns {el, start, isActive, focusHeading, destroy}; call `start()` once the
 * owner has placed `el` (the first draw may call `onActiveChange`).
 */
export function mountApprovalDock({ conversationId, threadId = null, onActiveChange = () => {}, onChangeIt = null }) {
  const uid = ++dockUid;
  const titleId = `dock-title-${uid}`;
  const inputId = `dock-input-${uid}`;
  const subs = [];
  let cid = conversationId;
  let items = [];
  let selectedId = null; // the request the owner is on (null: the oldest)
  let lastIndex = 0;
  let shown = null; // {a, settled: null | text}
  let shownKey = '';
  let busy = false;
  let reconciling = false; // after a reconnect or a failed write, until the hub's list is back
  let reconcileError = null;
  let status = '';
  let settleTimer = 0;
  let active = false;
  let wasOffline = false;
  let destroyed = false;
  let started = false;
  // The owner pressed a dock control that then turned off (and lost focus):
  // the next card's heading takes focus, so it never drops out of the dock.
  let focusOwed = false;

  // ---- elements (the card's content is rebuilt per request; these persist)

  const noticeText = h('span', { class: 'grow' });
  const notice = h(
    'div',
    { class: 'banner info dock-notice', role: 'status', hidden: true },
    icon('shield'),
    noticeText,
    h('button', { class: 'btn btn-sm btn-ghost', type: 'button', onclick: () => setNotice('') }, 'Dismiss'),
  );
  // In a thread view: the chat's other requests (they don't block this thread).
  const others = threadId
    ? h('button', { class: 'btn btn-sm dock-others', type: 'button', hidden: true, onclick: () => goToChatQueue() }, icon('shield'), h('span'))
    : null;
  const card = h('section', { class: 'approval-dock', 'aria-labelledby': titleId, hidden: true });
  const el = h('div', { class: 'dock-wrap' }, notice, others, card);

  // Refs into the current card (set by buildCard).
  let refs = null;

  function currentItems() {
    return threadId ? approvalsFor(threadId) : approvalsIn(cid);
  }

  function offline() {
    return !store.online && store.everOnline;
  }

  function canWrite() {
    return isAdmin() && !offline() && !reconciling && !busy && Boolean(shown) && !shown.settled;
  }

  // ---- what to show

  function settledText(a) {
    if (outcomes.has(a.approval_id)) return outcomes.get(a.approval_id);
    if (store.answered.has(a.approval_id)) return 'Answered on another device.';
    const t = store.threads.get(a.thread_id);
    if (t && !isActive(t.status)) return 'Thread ended; this request can’t run.';
    return 'This request is no longer waiting.';
  }

  function settle(a, text) {
    shown = { a, settled: text };
    busy = false;
    clearTimeout(settleTimer);
    const here = outcomes.has(a.approval_id);
    settleTimer = setTimeout(() => {
      if (destroyed) return;
      shown = null;
      selectedId = null;
      render();
    }, here ? SETTLE_HERE_MS : SETTLE_ELSEWHERE_MS);
    dropDeepLink(a.approval_id);
  }

  function pick() {
    if (selectedId) {
      const i = items.findIndex((x) => x.approval_id === selectedId);
      if (i >= 0) {
        lastIndex = i;
        return items[i];
      }
    }
    if (!items.length) return null;
    lastIndex = Math.min(lastIndex, items.length - 1);
    const a = items[lastIndex];
    selectedId = a.approval_id;
    return a;
  }

  function render({ focus = false } = {}) {
    if (destroyed || !started) return;
    items = currentItems();
    // The request on screen went away (answered, here or elsewhere, or its thread ended).
    if (shown && !shown.settled && !items.some((x) => x.approval_id === shown.a.approval_id)) {
      settle(shown.a, settledText(shown.a));
    }
    if (!shown?.settled) {
      const a = pick();
      shown = a ? { a, settled: null } : null;
    }
    announceArrivals();
    draw(focus);
    renderOthers();
    setActive(Boolean(shown));
    syncWrap();
  }

  /** The wrapper takes no room when it shows nothing. */
  function syncWrap() {
    el.hidden = card.hidden && notice.hidden && (!others || others.hidden);
  }

  // One polite announcement per new request (live arrivals only), without moving focus.
  function announceArrivals() {
    const fresh = [];
    for (const a of items) {
      if (announcedIds.has(a.approval_id)) continue;
      announcedIds.add(a.approval_id);
      if (a.arrivedAt) fresh.push(a);
    }
    if (fresh.length === 1) announce(`${fresh[0].tool} on ${deviceName(fresh[0].device_id)} needs review.`, { force: true });
    else if (fresh.length > 1) announce(`${fresh.length} tool calls need review.`, { force: true });
  }

  function setActive(next) {
    const hadFocus = card.contains(document.activeElement) || document.activeElement === document.body;
    card.hidden = !next;
    if (next === active) return;
    active = next;
    onActiveChange(active, { hadFocus });
  }

  // ---- drawing

  function draw(focus) {
    const key = shown ? `${shown.a.approval_id}|${shown.settled || ''}` : '';
    const hadFocus = card.contains(document.activeElement) || (focusOwed && document.activeElement === document.body);
    if (key !== shownKey) {
      shownKey = key;
      if (shown) {
        status = '';
        buildCard();
      } else {
        card.replaceChildren();
        refs = null;
      }
      if (shown && (hadFocus || focus)) {
        refs?.heading.focus({ preventScroll: true });
        focusOwed = false;
      }
    } else if (focus && refs) {
      refs.heading.focus({ preventScroll: true });
    }
    if (shown) {
      paintContext();
      updateQueue();
      sync();
    }
  }

  function buildCard() {
    const a = shown.a;
    const device = deviceName(a.device_id);
    const thread = store.threads.get(a.thread_id);
    const settled = shown.settled;
    const riskRule = !isManualReason(a.reason);

    const heading = h('h2', { class: 'dock-title', id: titleId, tabindex: '-1' }, 'Run this tool?');
    const toolLine = h('p', { class: 'dock-tool' }, h('span', { class: 'mono dock-tool-name' }, a.tool), ` on ${device}`);
    const threadLine = h('p', { class: 'dock-thread has-fam fam-unknown' });
    const chatLine = threadId ? h('p', { class: 'hint dock-chat' }) : null;
    const reasonText = a.reason || 'This tool is set to Manual, so every call waits for you.';
    const reason = h(
      'div',
      { class: ['reason', 'dock-reason', riskRule && 'risk'] },
      icon(riskRule ? 'warning' : 'shield'),
      h('div', null, riskRule ? h('strong', { class: 'reason-kind' }, 'A safety rule asked. ') : null, reasonText),
    );

    // The exact input: collapsed until the owner opens it, which turns Approve on.
    const pre = h('pre', { class: 'code dock-code', id: inputId, tabindex: '0', 'aria-label': `Exact input for ${a.tool}` }, pretty(a.input));
    const details = h(
      'details',
      {
        class: 'dock-input',
        open: acknowledged.has(a.approval_id),
        ontoggle: () => {
          if (details.open && !acknowledged.has(a.approval_id)) {
            acknowledged.add(a.approval_id);
            sync();
          }
        },
        onkeydown: (e) => {
          // Escape folds the disclosure and nothing else (never answers, never hides the dock).
          if (e.key === 'Escape' && details.open) {
            e.preventDefault();
            e.stopPropagation();
            details.open = false;
            details.querySelector('summary').focus();
          }
        },
      },
      h('summary', null, chevron(), 'Review exact input before approving'),
      pre,
    );
    const time = h('p', { class: 'hint dock-time' }, timeoutText(a));
    const threadLink =
      !threadId && a.thread_id
        ? h('p', { class: 'hint dock-open' }, h('a', { href: `#/chat/${enc(a.conversation_id || cid)}/thread/${enc(a.thread_id)}` }, 'Open the thread'))
        : null;

    const statusEl = h('p', { class: 'dock-status', role: 'status' });

    // Queue controls: one request at a time, oldest first, no wrapping.
    const prev = h('button', { class: 'btn btn-sm dock-nav', type: 'button', 'aria-label': 'Previous request', onclick: () => step(-1) }, icon('back'), h('span', { class: 'dock-nav-word' }, 'Previous'));
    const next = h('button', { class: 'btn btn-sm dock-nav', type: 'button', 'aria-label': 'Next request', onclick: () => step(1) }, h('span', { class: 'dock-nav-word' }, 'Next'), icon('chevron'));
    const pos = h('span', { class: 'dock-pos' });
    const queue = h(
      'div',
      {
        class: 'dock-queue',
        role: 'group',
        'aria-label': 'Pending requests',
        onkeydown: (e) => {
          // Arrow keys move through the queue only while focus is on its controls.
          if (e.altKey || e.ctrlKey || e.metaKey) return;
          if (e.key === 'ArrowLeft' || e.key === 'ArrowUp') {
            e.preventDefault();
            step(-1);
          } else if (e.key === 'ArrowRight' || e.key === 'ArrowDown') {
            e.preventDefault();
            step(1);
          }
        },
      },
      prev,
      pos,
      next,
    );

    const gateId = `dock-gate-${uid}`;
    const approve = h(
      'button',
      {
        class: ['btn', 'btn-lg', 'dock-approve', riskRule ? null : 'btn-primary'],
        type: 'button',
        'aria-describedby': gateId,
        onclick: () => answer(true),
        // Enter never approves: only a click, a tap or Space does.
        onkeydown: (e) => {
          if (e.key === 'Enter') e.preventDefault();
        },
      },
      icon('check'),
      'Approve',
    );
    const deny = h('button', { class: 'btn btn-lg dock-deny', type: 'button', onclick: () => answer(false) }, icon('x'), 'Deny');
    const gate = h('p', { class: 'hint dock-gate', id: gateId });
    const change = h('button', { class: 'link-btn', type: 'button', onclick: () => answer(false, true) }, 'Change it: deny and tell the thread what to do instead');
    const stopBtn = h('button', { class: 'link-btn danger', type: 'button', onclick: () => stopIt() }, icon('stop'), 'Stop thread');
    const actions = h('div', { class: ['approval-actions', riskRule && 'equal'] }, approve, deny);
    const links = h('div', { class: 'approval-links' }, change, stopBtn);
    const settledEl = settled
      ? h('div', { class: ['banner', outcomes.has(a.approval_id) ? 'info' : 'warn', 'dock-settled'] }, icon(outcomes.has(a.approval_id) ? 'check' : 'shield'), h('span', { class: 'grow' }, settled))
      : null;
    const hint =
      !threadId && notifyHintWanted()
        ? h('p', { class: 'hint dock-notify' }, 'Want an alert when a tool needs review? ', h('a', { href: '#/settings' }, 'Turn on notifications in Settings'), '.')
        : null;

    setChildren(
      card,
      h('div', { class: 'dock-head' }, heading),
      h('div', { class: 'dock-body' }, toolLine, threadLine, chatLine, reason, details, time, threadLink),
      h(
        'div',
        { class: 'dock-foot' },
        settled ? settledEl : [queue, statusEl, actions, gate, links, hint],
      ),
    );
    refs = { heading, details, time, prev, next, pos, queue, approve, deny, change, stopBtn, statusEl, gate, threadLine, chatLine, a };
    paintContext();
  }

  /** The thread (title, model and family) and chat lines change in place as the store learns them. */
  function paintContext() {
    if (!refs) return;
    const a = shown.a;
    const thread = store.threads.get(a.thread_id);
    const modelId = thread?.model || '';
    const fam = familyOfId(modelId);
    const title = thread?.title || 'Thread';
    const key = `${title}|${modelId}|${fam}`;
    if (refs.threadLine.dataset.key !== key) {
      refs.threadLine.dataset.key = key;
      setFamilyClass(refs.threadLine, fam);
      setChildren(
        refs.threadLine,
        familyMark(fam),
        h('span', { class: 'dock-thread-title' }, title),
        h('span', { class: 'dock-model' }, srOnly(`, ${familyLabel(fam)}: `), h('span', { class: 'mono' }, modelId || 'model not recorded')),
      );
    }
    if (refs.chatLine) {
      const chat = store.conversations.find((c) => c.id === (a.conversation_id || cid));
      setText(refs.chatLine, `In ${chat?.title || 'this chat'}`);
    }
  }

  function updateQueue() {
    if (!refs || shown.settled) return;
    const i = items.findIndex((x) => x.approval_id === shown.a.approval_id);
    const n = items.length;
    refs.queue.hidden = n < 2;
    setText(refs.pos, i >= 0 ? `${i + 1} of ${n}` : `${n} waiting`);
    refs.prev.disabled = i <= 0;
    refs.next.disabled = i < 0 || i >= n - 1;
  }

  /** Buttons follow who may answer, the connection, the gate and the write in flight. */
  function sync() {
    if (!refs || shown.settled) return;
    const a = shown.a;
    const writable = canWrite();
    const seen = acknowledged.has(a.approval_id);
    refs.approve.disabled = !writable || !seen;
    refs.deny.disabled = refs.change.disabled = refs.stopBtn.disabled = !writable;
    setText(refs.gate, seen ? '' : 'Approve turns on once you open “Review exact input”.');
    refs.gate.hidden = seen;
    let text = status;
    if (!isAdmin()) text = 'This session uses a device token, which can’t answer approvals. Sign in with an admin token to answer.';
    else if (offline()) text = 'Offline. The request is shown as last synced; reconnect to answer it.';
    else if (reconciling) text = reconcileError ? `Couldn’t check this request with the hub: ${reconcileError}` : 'Checking this request with the hub…';
    setText(refs.statusEl, text);
    refs.statusEl.hidden = !text;
    if (reconcileError && reconciling && !refs.statusEl.querySelector('button')) {
      refs.statusEl.append(' ', h('button', { class: 'link-btn', type: 'button', onclick: () => reconcile() }, 'Check again'));
    }
  }

  function updateTime() {
    if (refs && shown && !shown.settled) setText(refs.time, timeoutText(shown.a));
  }

  // ---- queue navigation

  function step(delta) {
    if (!shown || shown.settled) return;
    const i = items.findIndex((x) => x.approval_id === shown.a.approval_id);
    const j = i + delta;
    if (i < 0 || j < 0 || j >= items.length) return;
    selectedId = items[j].approval_id;
    lastIndex = j;
    const focusOn = delta < 0 ? 'prev' : 'next';
    render();
    rememberSelection();
    // Stay on the control that moved, or on the heading when it's now disabled.
    const btn = refs?.[focusOn];
    if (btn && !btn.disabled && !refs.queue.hidden) btn.focus({ preventScroll: true });
    else refs?.heading.focus({ preventScroll: true });
    const a = shown?.a;
    if (a) announce(`Request ${j + 1} of ${items.length}: ${a.tool} on ${deviceName(a.device_id)}.`, { force: true });
  }

  /** Keep the chat route's `?approval=` on the selection, so Back returns to it. */
  function rememberSelection() {
    if (threadId || !shown || shown.settled) return;
    const want = `#/chat/${enc(cid)}?approval=${enc(shown.a.approval_id)}`;
    if (location.hash === want) return;
    const r = routeOf(location.hash);
    if (r.conversation !== cid || r.thread) return;
    history.replaceState(history.state, '', want);
  }

  /** A deep-linked request that settled: drop it from the URL (a reload would say it's gone). */
  function dropDeepLink(approvalId) {
    const m = /[?&]approval=([^&]*)/.exec(location.hash);
    if (!m) return;
    let id = m[1];
    try {
      id = decodeURIComponent(id);
    } catch {
      /* keep it raw */
    }
    if (id !== approvalId) return;
    const r = routeOf(location.hash);
    if (threadId ? r.thread !== threadId : r.conversation !== cid || r.thread) return;
    history.replaceState(history.state, '', location.hash.replace(/[?&]approval=[^&]*/, '').replace(/\?$/, ''));
  }

  // ---- answering

  async function answer(approved, thenChange = false) {
    if (!canWrite() || (approved && !acknowledged.has(shown.a.approval_id))) return;
    const a = shown.a;
    const device = deviceName(a.device_id);
    focusOwed = card.contains(document.activeElement);
    busy = true; // both buttons go off at once: no duplicate writes
    status = approved ? 'Approving…' : 'Denying…';
    // Recorded before the write: the hub's live "resolved" event can arrive before
    // its HTTP reply, and this answer must not read as "answered on another device".
    outcomes.set(a.approval_id, approved ? `Approved. ${a.tool} runs on ${device}.` : `Denied. ${a.tool} won’t run.`);
    sync();
    try {
      await post(`/v1/approvals/${enc(a.approval_id)}`, { approve: approved });
      alertOnce(approved ? `Approved ${a.tool} on ${device}.` : `Denied ${a.tool} on ${device}.`);
      store.answered.add(a.approval_id);
      store.approvals.delete(a.approval_id);
      busy = false;
      emit('approvals'); // every view (this dock included) settles it
      if (thenChange) (onChangeIt || defaultChangeIt)(a);
    } catch (err) {
      outcomes.delete(a.approval_id);
      if (shown?.settled && shown.a.approval_id === a.approval_id) {
        // Settled by a live event while the write failed: it wasn't this answer.
        shown = { a, settled: settledText(a) };
        draw(false);
      }
      if (err.status === 404) {
        // Already answered (or dropped) on the hub.
        busy = false;
        store.answered.add(a.approval_id);
        store.approvals.delete(a.approval_id);
        emit('approvals');
        return;
      }
      // The answer may or may not have landed: ask the hub before offering it again.
      status = `${describe(err)} Checking with the hub before you try again…`;
      busy = false;
      await reconcile();
      if (shown && !shown.settled && shown.a.approval_id === a.approval_id) {
        status = reconcileError ? status : 'The hub didn’t record an answer. You can answer again.';
        sync();
      }
    }
  }

  function defaultChangeIt(a) {
    setStepInDraft(a.thread_id, CHANGE_DRAFT);
    location.hash = `#/chat/${enc(a.conversation_id || cid)}/thread/${enc(a.thread_id)}`;
  }

  async function stopIt() {
    if (!canWrite()) return;
    const a = shown.a;
    focusOwed = card.contains(document.activeElement);
    busy = true;
    status = '';
    sync();
    try {
      const stopped = await stopThread(a.thread_id);
      busy = false;
      if (stopped) {
        outcomes.set(a.approval_id, 'Thread stopped; this request can’t run.');
        store.approvals.delete(a.approval_id);
        emit('approvals');
      } else {
        sync();
        refs?.stopBtn.focus({ preventScroll: true });
      }
    } catch (err) {
      busy = false;
      status = describe(err);
      sync();
    }
  }

  /** Fetch the hub's pending list before any answer is offered again. */
  async function reconcile() {
    reconciling = true;
    reconcileError = null;
    sync();
    try {
      await loadApprovals({ strict: true });
      reconciling = false;
    } catch (err) {
      reconcileError = describe(err);
    }
    if (destroyed) return;
    render();
    sync();
  }

  // ---- the chat's other requests, from a thread view

  function renderOthers() {
    if (!others) return;
    const rest = approvalsIn(cid).filter((x) => x.thread_id !== threadId);
    others.hidden = !rest.length;
    if (!rest.length) return;
    setText(others.querySelector('span'), rest.length === 1 ? '1 other request in this chat needs review' : `${rest.length} other requests in this chat need review`);
  }

  function goToChatQueue() {
    const rest = approvalsIn(cid).filter((x) => x.thread_id !== threadId);
    location.hash = rest.length ? `#/chat/${enc(cid)}?approval=${enc(rest[0].approval_id)}` : `#/chat/${enc(cid)}`;
  }

  // ---- deep links

  function setNotice(text) {
    setText(noticeText, text);
    notice.hidden = !text;
    syncWrap();
  }

  async function select(id, { focus }) {
    setNotice('');
    let a = store.approvals.get(id);
    if (!a) {
      // Push and links are best effort: ask the hub for its current list first.
      await loadApprovals().catch(() => {});
      if (destroyed) return;
      a = store.approvals.get(id);
    }
    if (!a) {
      setNotice(store.answered.has(id) ? 'That request was already answered.' : 'This request is no longer waiting.');
      dropDeepLink(id);
      render({ focus });
      if (focus && !shown) notice.querySelector('button')?.focus({ preventScroll: true });
      return;
    }
    const inScope = threadId ? a.thread_id === threadId : a.conversation_id === cid;
    if (!inScope) {
      // Never show a request in a chat it doesn't belong to.
      if (!threadId && a.conversation_id && a.conversation_id !== cid) location.replace(`#/chat/${enc(a.conversation_id)}?approval=${enc(id)}`);
      return;
    }
    if (shown?.settled) {
      clearTimeout(settleTimer);
      shown = null;
    }
    selectedId = id;
    render({ focus });
    if (focus && shown && !shown.settled) {
      const r = !isManualReason(a.reason) ? 'A safety rule asked: ' : '';
      announce(`Run this tool? ${a.tool} on ${deviceName(a.device_id)}. ${r}${a.reason || 'This tool is set to Manual.'}`, { force: true });
    }
  }

  subs.push(
    on('route', (r) => {
      if (r.view !== 'chat' || !r.approval) return;
      if (threadId) {
        if (r.thread === threadId) select(r.approval, { focus: true });
      } else if (r.conversation === cid) {
        // On a thread route the thread view's dock takes focus; this one only follows.
        select(r.approval, { focus: !r.thread });
      }
    }),
    on('approvals', () => render()),
    on('threads', () => {
      const t = threadId ? store.threads.get(threadId) : null;
      if (t?.conversation_id) cid = t.conversation_id;
      render();
    }),
    on('conversations', () => paintContext()),
    on('models', () => paintContext()),
    on('online', (online) => {
      if (!online) {
        wasOffline = store.everOnline;
        sync();
        return;
      }
      if (wasOffline) {
        wasOffline = false;
        reconcile(); // don't offer answers until the hub's list is fetched again
      } else sync();
    }),
  );

  // The timeout line counts down slowly while a request is shown.
  const countdown = setInterval(() => {
    if (!document.hidden) updateTime();
  }, COUNTDOWN_MS);

  if (limitsCache === null) {
    loadLimits()?.then(() => {
      if (!destroyed) updateTime();
    });
  }

  el.hidden = true;

  return {
    el,
    /** Draw the first time (after the owner has wired its callbacks to this object). */
    start() {
      if (started) return;
      started = true;
      render();
    },
    isActive: () => active,
    focusHeading: () => refs?.heading.focus({ preventScroll: true }),
    destroy() {
      destroyed = true;
      for (const off of subs) off();
      clearInterval(countdown);
      clearTimeout(settleTimer);
    },
  };
}

function chevron() {
  const c = icon('chevron');
  c.classList.add('chev');
  return c;
}

/** Just enough of the router to tell which chat/thread a hash is on. */
function routeOf(hash) {
  const raw = String(hash || '').replace(/^#\/?/, '').split('?')[0];
  const parts = raw.split('/').map((p) => {
    try {
      return decodeURIComponent(p);
    } catch {
      return p;
    }
  });
  return parts[0] === 'chat' ? { conversation: parts[1], thread: parts[2] === 'thread' ? parts[3] : undefined } : {};
}
