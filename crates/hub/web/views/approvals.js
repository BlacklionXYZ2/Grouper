// The approval dock (gui-spec 4.3, 5.13). While a chat has pending approvals the
// dock takes the composer's place at the bottom of the chat; in a thread view it
// takes the place of that thread's step-in composer while the thread's own request
// waits. It is inline, never a modal, and can't be collapsed or swapped for a
// reply: the owner answers, stops the thread, or goes somewhere else.
//
// One request at a time, oldest first ("1 of N", Previous/Next, no wrapping).
// Order, top to bottom: shield + "Run this tool?"; tool · via device and thread
// with its model (family mark and name), plus the chat in a thread view; hairline;
// the hub's reason; the exact input behind a "View exact input" disclosure;
// Deny and Approve side by side at equal size (Approve locked until expanded);
// then the gate hint, queue controls, timeout, and Change it / Open thread / Stop.
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
const inFlight = new Set(); // approval ids with an answer or stop request in flight
let dockUid = 0;
let limitsCache = null; // {secs, action} from /v1/limits, for live requests
let limitsLoading = null;

/** Forget all dock state on sign-out / store reset. */
export function resetApprovalDockState() {
  acknowledged.clear();
  outcomes.clear();
  announcedIds.clear();
  inFlight.clear();
  limitsCache = null;
  limitsLoading = null;
}

on('reset', resetApprovalDockState);

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
  const statuses = new Map(); // approval id -> status message scoped to that request
  let reconciling = false; // after a reconnect or a failed write, until the hub's list is back
  let reconcileError = null;
  let settleTimer = 0;
  let pendingDeepLink = null; // deep-link id waiting on retry/reconnect after a load error
  let selectSeq = 0;
  let active = false;
  let wasOffline = false;
  let destroyed = false;
  let started = false;
  // The owner pressed a dock control that then turned off (and lost focus):
  // the next card's heading takes focus, so it never drops out of the dock.
  let focusOwed = false;

  // ---- elements (the card's content is rebuilt per request; these persist)

  const noticeText = h('span', { class: 'grow' });
  let noticeRetryFn = null;
  const noticeRetry = h(
    'button',
    { class: 'btn btn-sm', type: 'button', hidden: true, onclick: () => noticeRetryFn?.() },
    'Try again',
  );
  const notice = h(
    'div',
    { class: 'banner info dock-notice', role: 'status', hidden: true },
    icon('shield'),
    noticeText,
    noticeRetry,
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

  function isBusy(id = shown?.a?.approval_id) {
    return Boolean(id && inFlight.has(id));
  }

  function setStatus(id, text) {
    if (!id) return;
    if (text) statuses.set(id, text);
    else statuses.delete(id);
  }

  function canWrite() {
    return isAdmin() && !offline() && !reconciling && Boolean(shown) && !shown.settled && !isBusy(shown.a.approval_id);
  }

  // ---- what to show

  function settledText(a) {
    if (outcomes.has(a.approval_id)) return outcomes.get(a.approval_id);
    const res = store.resolutions.get(a.approval_id);
    if (res?.reason === 'timeout_stop') return 'Timed out: denied and thread stopped.';
    if (res?.reason === 'timeout') return 'Timed out: denied.';
    if (res?.reason === 'dropped') {
      const t = store.threads.get(a.thread_id);
      if (t && !isActive(t.status)) return 'Thread ended; this request can’t run.';
      if (t && t.status === 'waiting_device') return 'Device disconnected; this request can’t run.';
      return 'This request is no longer waiting.';
    }
    if (store.answered.has(a.approval_id)) return 'Answered on another device.';
    const t = store.threads.get(a.thread_id);
    if (t && !isActive(t.status)) return 'Thread ended; this request can’t run.';
    return 'This request is no longer waiting.';
  }

  function settle(a, text) {
    shown = { a, settled: text };
    statuses.delete(a.approval_id);
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
    // Capture focus BEFORE draw() mutates or empties `card` (removing a focused
    // element drops activeElement to document.body).
    const hadCardFocus = card.contains(document.activeElement);
    items = currentItems();
    // The request on screen went away (answered, here or elsewhere, or its thread ended).
    // While our own POST for this request is still in flight, keep it on screen until
    // the POST finishes so we never claim "Approved." before the write succeeds or
    // misattribute a live timeout/other-device answer.
    if (shown && !shown.settled && !items.some((x) => x.approval_id === shown.a.approval_id) && !isBusy(shown.a.approval_id)) {
      settle(shown.a, settledText(shown.a));
    }
    const holdInFlight = shown && !shown.settled && isBusy(shown.a.approval_id) && (!selectedId || selectedId === shown.a.approval_id);
    if (!shown?.settled && !holdInFlight) {
      const a = pick();
      shown = a ? { a, settled: null } : null;
    }
    announceArrivals();
    const wantFocus = draw(focus, hadCardFocus);
    renderOthers();
    const nextActive = Boolean(shown);
    // Unhide card and wrapper BEFORE notifying the owner or moving focus: calling
    // .focus() inside a hidden element is a no-op.
    card.hidden = !nextActive;
    syncWrap();
    if (nextActive !== active) {
      const hadFocus = !nextActive && (hadCardFocus || (focusOwed && document.activeElement === document.body));
      if (!nextActive) focusOwed = false;
      active = nextActive;
      onActiveChange(active, { hadFocus });
    }
    if (shown && wantFocus && !card.hidden && !el.hidden) {
      refs?.heading.focus({ preventScroll: true });
      focusOwed = false;
    }
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

  // ---- drawing

  function draw(focus, hadCardFocus) {
    const key = shown ? `${shown.a.approval_id}|${shown.settled || ''}` : '';
    const hadFocus = hadCardFocus || (focusOwed && document.activeElement === document.body);
    let wantFocus = false;
    if (key !== shownKey) {
      shownKey = key;
      if (shown) {
        buildCard();
      } else {
        card.replaceChildren();
        refs = null;
      }
      if (shown && (hadFocus || focus)) wantFocus = true;
    } else if (focus && refs) {
      wantFocus = true;
    }
    if (shown) {
      paintContext();
      updateQueue();
      sync();
    }
    return wantFocus;
  }

  function buildCard() {
    const a = shown.a;
    const device = deviceName(a.device_id);
    const thread = store.threads.get(a.thread_id);
    const settled = shown.settled;
    const riskRule = !isManualReason(a.reason);

    const heading = h('h2', { class: 'dock-title', id: titleId, tabindex: '-1' }, 'Run this tool?');
    const head = h(
      'div',
      { class: 'dock-head' },
      h('span', { class: ['dock-shield', riskRule && 'risk'] }, icon('shield')),
      heading,
    );
    const toolLine = h(
      'p',
      { class: 'dock-tool' },
      h('span', { class: 'mono dock-tool-name' }, a.tool),
      h('span', { class: 'dock-sep', 'aria-hidden': 'true' }, ' · '),
      h('span', { class: 'dock-device' }, `via ${device}`),
    );
    const threadLine = h('p', { class: 'dock-thread has-fam fam-unknown' });
    const chatLine = threadId ? h('p', { class: 'hint dock-chat' }) : null;
    const context = h(
      'div',
      { class: 'dock-context' },
      h('span', { class: 'dock-tool-icon' }, icon('tool')),
      h('div', { class: 'dock-meta-lines' }, toolLine, threadLine, chatLine),
    );
    const reasonText = a.reason || 'This tool is set to Manual, so every call waits for you.';
    const reason = riskRule
      ? h(
          'div',
          { class: 'reason dock-reason risk' },
          icon('warning'),
          h('div', null, h('strong', { class: 'reason-kind' }, 'A safety rule asked. '), reasonText),
        )
      : h('p', { class: 'dock-reason-plain' }, reasonText);

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
      h('summary', null, icon('code'), h('span', { class: 'dock-input-label' }, 'View exact input'), chevron()),
      pre,
    );
    const time = h('p', { class: 'hint dock-time' }, timeoutText(a));
    const threadLink =
      !threadId && a.thread_id
        ? h('a', { class: 'link-btn dock-link dock-open', href: `#/chat/${enc(a.conversation_id || cid)}/thread/${enc(a.thread_id)}` }, 'Open thread')
        : null;

    const statusEl = h('p', { class: 'dock-status', role: 'status' });

    // Queue controls: one request at a time, oldest first, no wrapping.
    const prev = h('button', { class: 'btn btn-sm btn-ghost dock-nav', type: 'button', 'aria-label': 'Previous request', onclick: () => step(-1) }, icon('back'), h('span', { class: 'dock-nav-word' }, 'Previous'));
    const next = h('button', { class: 'btn btn-sm btn-ghost dock-nav', type: 'button', 'aria-label': 'Next request', onclick: () => step(1) }, h('span', { class: 'dock-nav-word' }, 'Next'), icon('chevron'));
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
        class: ['btn', 'btn-lg', 'dock-approve', riskRule ? 'btn-outline' : 'btn-primary'],
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
    const deny = h('button', { class: 'btn btn-lg btn-outline dock-deny', type: 'button', onclick: () => answer(false) }, icon('x'), 'Deny');
    const gate = h('p', { class: 'hint dock-gate', id: gateId });
    const change = h(
      'button',
      { class: 'link-btn dock-link', type: 'button', title: 'Deny and tell the thread what to do instead', onclick: () => answer(false, true) },
      'Change it',
      srOnly(': deny and tell the thread what to do instead'),
    );
    const stopBtn = h('button', { class: 'link-btn danger dock-link', type: 'button', onclick: () => stopIt() }, icon('stop'), 'Stop thread');
    const actions = h('div', { class: ['approval-actions', riskRule && 'equal'] }, deny, approve);
    const links = h('div', { class: 'approval-links' }, change, threadLink, stopBtn);
    const settledEl = settled
      ? h('div', { class: ['banner', outcomes.has(a.approval_id) ? 'info' : 'warn', 'dock-settled'] }, icon(outcomes.has(a.approval_id) ? 'check' : 'shield'), h('span', { class: 'grow' }, settled))
      : null;
    const hint =
      !threadId && notifyHintWanted()
        ? h('p', { class: 'hint dock-notify' }, 'Want an alert when a tool needs review? ', h('a', { href: '#/settings' }, 'Turn on notifications in Settings'), '.')
        : null;

    card.classList.toggle('is-risk', riskRule);
    card.classList.toggle('is-settled', Boolean(settled));
    setChildren(
      card,
      h('div', { class: 'dock-top' }, head, context),
      h('div', { class: 'dock-body' }, reason, details),
      h(
        'div',
        { class: 'dock-foot' },
        settled ? settledEl : [statusEl, actions, gate, queue, time, links, hint],
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
        h('span', { class: 'dock-thread-prefix' }, 'Thread: '),
        h('span', { class: 'dock-thread-title' }, title),
        h('span', { class: 'dock-sep', 'aria-hidden': 'true' }, ' · '),
        familyMark(fam),
        h('span', { class: 'dock-model' }, srOnly(`${familyLabel(fam)}: `), h('span', { class: 'mono' }, modelId || 'model not recorded')),
      );
    }
    if (refs.chatLine) {
      const chat = store.conversations.find((c) => c.id === (a.conversation_id || cid));
      setText(refs.chatLine, `In ${chat?.title || 'this chat'}`);
    }
  }

  function updateQueue() {
    if (!refs || !shown || shown.settled) return;
    const i = items.findIndex((x) => x.approval_id === shown.a.approval_id);
    const n = items.length;
    refs.queue.hidden = n < 2;
    setText(refs.pos, i >= 0 ? `${i + 1} of ${n}` : `${n} waiting`);
    refs.prev.disabled = i <= 0;
    refs.next.disabled = i < 0 || i >= n - 1;
  }

  /** Buttons follow who may answer, the connection, the gate and the write in flight. */
  function sync() {
    if (!refs || !shown || shown.settled) return;
    const a = shown.a;
    const writable = canWrite();
    const seen = acknowledged.has(a.approval_id);
    refs.approve.disabled = !writable || !seen;
    refs.deny.disabled = refs.change.disabled = refs.stopBtn.disabled = !writable;
    setText(refs.gate, seen ? '' : 'Approve turns on once you open “View exact input”.');
    refs.gate.hidden = seen;
    let text = statuses.get(a.approval_id) || '';
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
    focusOwed = false;
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
    const id = a.approval_id;
    const device = deviceName(a.device_id);
    focusOwed = card.contains(document.activeElement);
    inFlight.add(id); // buttons & queue nav turn off for this request only
    setStatus(id, 'Sending…');
    emit('approval_inflight');
    try {
      await post(`/v1/approvals/${enc(id)}`, { approve: approved });
      // Only record the local outcome once the hub confirms the POST succeeded.
      outcomes.set(id, approved ? `Approved. ${a.tool} runs on ${device}.` : `Denied. ${a.tool} won’t run.`);
      setStatus(id, '');
      inFlight.delete(id);
      emit('approval_inflight');
      alertOnce(approved ? `Approved ${a.tool} on ${device}.` : `Denied ${a.tool} on ${device}.`);
      store.answered.add(id);
      store.approvals.delete(id);
      emit('approvals'); // every view (this dock included) settles it
      if (thenChange) (onChangeIt || defaultChangeIt)(a);
    } catch (err) {
      inFlight.delete(id);
      emit('approval_inflight');
      if (err.status === 404) {
        // Already answered, timed out, or dropped on the hub.
        setStatus(id, '');
        store.approvals.delete(id);
        emit('approvals');
        return;
      }
      // If a live event already removed the request while the POST failed, settle with its actual outcome.
      if (!store.approvals.has(id)) {
        setStatus(id, '');
        emit('approvals');
        return;
      }
      // The answer may or may not have landed: ask the hub before offering it again.
      setStatus(id, `${describe(err)} Checking with the hub before you try again…`);
      await reconcile();
      if (shown && !shown.settled && shown.a.approval_id === id) {
        focusOwed = false;
        setStatus(id, reconcileError ? statuses.get(id) : 'The hub didn’t record an answer. You can answer again.');
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
    const id = a.approval_id;
    focusOwed = card.contains(document.activeElement);
    inFlight.add(id);
    setStatus(id, '');
    emit('approval_inflight');
    try {
      const stopped = await stopThread(a.thread_id);
      inFlight.delete(id);
      emit('approval_inflight');
      if (stopped) {
        outcomes.set(id, 'Thread stopped; this request can’t run.');
        store.approvals.delete(id);
        emit('approvals');
      } else {
        focusOwed = false;
        updateQueue();
        sync();
        if (shown?.a.approval_id === id) refs?.stopBtn.focus({ preventScroll: true });
      }
    } catch (err) {
      inFlight.delete(id);
      emit('approval_inflight');
      focusOwed = false;
      setStatus(id, describe(err));
      updateQueue();
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
    if (!reconciling && pendingDeepLink) {
      const retryId = pendingDeepLink;
      select(retryId, { focus: false });
    }
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

  function setNotice(text, { retry = null } = {}) {
    if (!text) pendingDeepLink = null;
    setText(noticeText, text);
    noticeRetryFn = retry;
    noticeRetry.hidden = !retry;
    notice.hidden = !text;
    syncWrap();
  }

  async function select(id, { focus }) {
    const seq = ++selectSeq;
    setNotice('');
    let a = store.approvals.get(id);
    if (!a) {
      // Push and links are best effort: ask the hub for its current list first.
      try {
        await loadApprovals({ strict: true });
      } catch (err) {
        if (destroyed || seq !== selectSeq) return;
        // Keep `?approval=<id>` so a retry or reconnect can open it once reachable.
        pendingDeepLink = id;
        const msg = offline()
          ? 'Offline. Couldn’t check this request with the hub.'
          : `Couldn’t check this request with the hub: ${describe(err)}`;
        setNotice(msg, { retry: () => select(id, { focus: true }) });
        render({ focus });
        if (focus && !shown) noticeRetry.focus({ preventScroll: true });
        return;
      }
      if (destroyed || seq !== selectSeq) return;
      a = store.approvals.get(id);
    }
    if (!a) {
      const res = store.resolutions.get(id);
      const msg =
        res?.reason === 'timeout_stop'
          ? 'That request timed out and the thread was stopped.'
          : res?.reason === 'timeout'
            ? 'That request timed out and was denied.'
            : res?.reason === 'dropped'
              ? 'This request is no longer waiting.'
              : store.answered.has(id)
                ? 'That request was already answered.'
                : 'This request is no longer waiting.';
      setNotice(msg);
      dropDeepLink(id);
      render({ focus });
      if (focus && !shown) notice.querySelector('button:not([hidden])')?.focus({ preventScroll: true });
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
    on('approval_inflight', () => {
      updateQueue();
      sync();
    }),
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
