// The thread view (inspector on wide screens, a full-height sheet on phones). It
// reads like a chat (owner decision, 26 Nov): the orchestrator's brief is the first
// message, the thread model's text is the assistant's, tool calls are compact rows
// in between, and step-ins, questions to the orchestrator and its answers are
// messages. Around the transcript: the waiting/checkpoint card, the result and
// rating, files and raw events. The composer at the bottom messages the thread;
// Stop sits beside it. While this thread has a pending approval, the approval dock
// (views/approvals.js) takes the composer's place; the step-in draft and its
// selection come back afterwards. The chat's other requests don't block this
// thread: the dock shows a way to the chat's queue instead.
//
// The header follows the concept art: title with pop-out and close; a status chip
// with the live elapsed time; device and model chips (the model the orchestrator
// chose, fixed, no picker here). Pop-out shows the same chat at full width over the
// chat it came from; Escape or the button brings the panel back.
//
// The plan: a thread keeps its own checklist with the built-in tool `update_plan`,
// whose input is always the whole list:
//   {"items": [{"content": "...", "status": "pending" | "in_progress" | "completed",
//               "active_form"?: "..."}]}
// The plan is the input of the thread's latest accepted update_plan call (stored
// history and live events alike); a call the hub refuses (ok=false) keeps the plan
// before it, an empty list clears it, and no call means no checklist. Those calls
// are not tool rows. The checklist is pinned at the top of the chat and updates in
// place.
//
// The view is built once. Stored events are appended as they arrive (a tool row is
// filled in place when its result lands); deltas only refill the streaming-draft
// node, at most every 100 ms. The small sections (plan, waiting, result, files) are
// redrawn only when their own inputs change. The elapsed time in the header ticks
// through the shared one-second timer while the thread is live.

import { h, icon, setChildren, throttle, setText, setAttr } from '../lib/dom.js';
import { get, post, qs, enc, describe } from '../lib/api.js';
import { renderMarkdown } from '../lib/md.js';
import { store, on, liveOf, deviceName, deviceOnline, approvalsFor, isAdmin } from '../lib/store.js';
import { statusPill, skeleton, confirmAction, toast, autoGrow, errorBanner, announce, sentenceSpeaker, onTick, isNarrow, syncInert } from '../lib/ui.js';
import { ago, dateTime, time, num, pretty, isActive, elapsed, isoDuration, threadElapsed } from '../lib/format.js';
import { familyOfId, familyMark, familyInfo, setFamilyClass } from '../lib/family.js';
import { mountApprovalDock, CHANGE_DRAFT } from './approvals.js';

const drafts = new Map();
const draftListeners = new Map(); // threadId -> (text) => void for mounted thread views
const planOpen = new Map(); // threadId -> false once the owner folded its plan

on('reset', () => {
  drafts.clear();
  planOpen.clear();
});

/** Put text in a thread's step-in field (used by the approval dock's "Change it"). */
export function setStepInDraft(threadId, text) {
  const apply = draftListeners.get(threadId);
  if (apply) {
    apply(text);
    return;
  }
  drafts.set(threadId, text);
}

/** Stop a thread, asking first while it runs. Resolves true when it was stopped. */
export async function stopThread(threadId) {
  const th = store.threads.get(threadId) || {};
  if (th.status === 'running') {
    const ok = await confirmAction({
      title: 'Stop this thread?',
      body: [`“${th.title || 'This thread'}” stops on ${deviceName(th.device_id)}. Work it hasn’t saved is lost, and the orchestrator is told you stopped it.`],
      confirmLabel: 'Stop thread',
      danger: true,
    });
    if (!ok) return false;
  }
  await post(`/v1/threads/${enc(threadId)}/stop`);
  toast('Thread stopped.');
  return true;
}

// ---- the plan (shared with the threads list; kept here, not in lib/, because W3
// owns only this view, the threads list and css/thread.css)

export const PLAN_TOOL = 'update_plan';
const PLAN_STATES = new Set(['pending', 'in_progress', 'completed']);

/** Is this event an update_plan call or its result? */
export function isPlanEvent(e) {
  return e?.kind === 'thread_activity' && (e.event?.type === 'tool_call' || e.event?.type === 'tool_result') && e.event.name === PLAN_TOOL;
}

/**
 * The checklist from an update_plan input: [{content, status, active}] with
 * unknown statuses read as pending and empty items dropped; [] for an empty list
 * (the plan is cleared); null when the input isn't a plan at all.
 */
export function parsePlan(input) {
  let v = input;
  if (typeof v === 'string') {
    try {
      v = JSON.parse(v);
    } catch {
      return null;
    }
  }
  if (!v || !Array.isArray(v.items)) return null;
  return v.items
    .filter((i) => i && typeof i.content === 'string' && i.content.trim())
    .map((i) => ({
      content: i.content.trim(),
      status: PLAN_STATES.has(i.status) ? i.status : 'pending',
      active: typeof i.active_form === 'string' && i.active_form.trim() ? i.active_form.trim() : null,
    }));
}

/**
 * Follows one thread's plan through its events, stored or live. A call that
 * reads as a plan shows at once (the hub answers in a moment); if the hub then
 * refuses it (ok=false: invalid input), the plan goes back to what it was. An
 * empty list clears it. `plan` is the items, or null for no checklist.
 */
export function planTracker() {
  const before = new Map(); // call_id -> the plan before that call
  return {
    plan: null,
    /** Fold one event in; true when the plan changed. */
    apply(e) {
      if (!isPlanEvent(e)) return false;
      const a = e.event;
      if (a.type === 'tool_call') {
        const items = parsePlan(a.input);
        if (!items) return false; // the hub refuses it: keep the plan
        before.set(a.call_id, this.plan);
        this.plan = items.length ? items : null;
        return true;
      }
      if (!before.has(a.call_id)) return false;
      const prev = before.get(a.call_id);
      before.delete(a.call_id);
      if (a.ok) return false;
      this.plan = prev;
      return true;
    },
  };
}

/** Each thread's current plan from a conversation's events: Map threadId -> items. */
export function plansIn(events) {
  const trackers = new Map();
  for (const e of events) {
    if (!e.thread_id || !isPlanEvent(e)) continue;
    if (!trackers.has(e.thread_id)) trackers.set(e.thread_id, planTracker());
    trackers.get(e.thread_id).apply(e);
  }
  const out = new Map();
  for (const [id, tr] of trackers) if (tr.plan) out.set(id, tr.plan);
  return out;
}

/** Done and total of a plan. */
export function planProgress(items) {
  return { done: items.filter((i) => i.status === 'completed').length, total: items.length };
}

const ASK = 'ask_orchestrator';
const WIDE = '(max-width: 1180px)';

export function renderThread(container, { threadId, conversationId, onClose, onWrongConversation }) {
  const subs = [];
  const aborter = new AbortController();
  const initial = store.threads.get(threadId) || null;
  let verified = !conversationId || Boolean(initial?.conversation_id && initial.conversation_id === conversationId);
  let info = verified ? initial : null;
  let cid = conversationId || info?.conversation_id;
  let events = [];
  let lastSeq = 0;
  const toThumb = (r) => (r === 1 || r === 'up' ? 'up' : r === -1 || r === 'down' ? 'down' : null);
  let loaded = false;
  let loadError = null;
  let rating = toThumb(initial?.rating);
  let files = null; // null: not loaded; {error} or listing
  let filesLoading = false;
  let tokensIn = 0;
  let tokensOut = 0;
  let lastStatus = null;
  let sendingStep = false;
  const calls = new Map(); // call_id -> {details, state, body, call, result} or {ask, el} or {plan}
  const openCalls = new Set();
  const speaker = sentenceSpeaker();
  // Who spoke last in the transcript: 'thread', 'orchestrator' or 'user'. A label
  // with the model goes above the thread's first item after someone else spoke.
  let lastSpeaker = null;
  // The orchestrator's latest message to the thread, to recognise it again as the
  // answer to an ask_orchestrator question (the hub delivers it both ways).
  let lastOrchestratorInput = null;
  let planT = planTracker();
  let plan = null; // the current plan's items (planT.plan), or null
  let popped = false; // shown full width over the chat

  container.classList.add('thread-view');

  // ---- header (built once): title, pop-out, close; status chip; device and model
  const titleEl = h('h2', { id: 'thread-title', tabindex: '-1' });
  const popBtn = h(
    'button',
    { class: 'icon-btn tv-pop', type: 'button', 'aria-label': 'Full width', 'aria-pressed': 'false', title: 'Show the thread full width', onclick: () => setPopped(!popped) },
    icon('popout'),
  );
  const closeBtn = h('button', { class: 'icon-btn', type: 'button', 'aria-label': 'Close thread', title: 'Close', onclick: onClose }, icon('close'));
  const pillSlot = h('span', { class: 'thread-pill' });
  const elapsedLabel = h('span', { class: 'visually-hidden' });
  const elapsedEl = h('time', { class: 'thread-elapsed' });
  const elapsedWrap = h('span', { class: 'thread-time', hidden: true }, h('span', { class: 'tv-sep', 'aria-hidden': 'true' }, '·'), elapsedLabel, elapsedEl);
  const deviceText = h('span', { class: 'tv-chip-text' });
  const deviceState = h('span', { class: 'tv-chip-note', hidden: true });
  const deviceChip = h('span', { class: 'tv-chip tv-device' }, icon('devices'), h('span', { class: 'visually-hidden' }, 'Device: '), deviceText, deviceState);
  const chipMark = h('span', { class: 'tv-mark' });
  const chipName = h('span', { class: 'tv-chip-text mono' });
  const chipNote = h('span', { class: 'visually-hidden' });
  const modelChip = h(
    'span',
    { class: 'tv-chip tv-model has-fam fam-unknown', hidden: true },
    chipMark,
    h('span', { class: 'visually-hidden' }, 'Model chosen by the orchestrator: '),
    chipName,
    chipNote,
  );
  const startedEl = h('span', { class: 'thread-started' });
  const tokensEl = h('span', { class: 'thread-tokens', hidden: true });
  const metaEl = h('p', { class: 'meta tv-meta' }, startedEl, tokensEl);
  const reasonEl = h('p', { class: 'hint tv-reason', hidden: true });
  const head = h(
    'header',
    { class: 'sheet-head thread-head' },
    h('div', { class: 'sheet-head-row' }, titleEl, popBtn, closeBtn),
    h('div', { class: 'tv-chips' }, pillSlot, deviceChip, modelChip),
    reasonEl,
    metaEl,
  );

  // ---- the plan checklist, pinned at the top of the chat
  const planCount = h('span', { class: 'tv-plan-count' });
  const planNow = h('span', { class: 'tv-plan-now' });
  const planFill = h('span', { class: 'tv-plan-fill' });
  const planList = h('ol', { class: 'tv-plan-list' });
  const planBox = h(
    'details',
    {
      class: 'tv-plan',
      open: planOpen.get(threadId) !== false,
      ontoggle: () => planOpen.set(threadId, planBox.open),
    },
    h(
      'summary',
      null,
      h('span', { class: 'tv-plan-head' }, h('h3', { class: 'tv-plan-title', id: 'plan-title' }, 'Plan'), planCount, cardChev()),
      planNow,
      h('span', { class: 'tv-plan-bar', 'aria-hidden': 'true' }, planFill),
    ),
    planList,
  );
  const planSec = h('section', { class: 'tv-plan-wrap', 'aria-labelledby': 'plan-title', hidden: true }, planBox);

  // ---- body: plan, attention cards, the transcript, result, files, raw events
  const statusSlot = h('div', { class: 'sheet-status' }); // loading and errors
  const waitingSlot = h('section', { 'aria-label': 'Waiting for device', class: 'thread-attention', hidden: true });
  // Not a live region: the draft changes many times a second. Screen readers hear
  // it through the throttled #live-region (sentences, then the finish).
  const draftEl = h('div', { class: 'msg msg-assistant md', 'aria-label': 'Thread output so far', hidden: true });
  // The draft's own model label, shown while it streams after someone else spoke.
  // Kept apart from the transcript's labels so a step-in arriving mid-stream goes
  // above both and never leaves a label with nothing under it.
  const draftWho = h('div', { class: 'msg-who thread-who has-fam', hidden: true });
  const emptyEl = h('p', { class: 'hint thread-empty' });
  const convo = h('div', { class: 'transcript thread-transcript', role: 'region', 'aria-label': 'Thread transcript', hidden: true }, draftWho, draftEl, emptyEl);
  const resultSlot = h('section', { class: 'tv-result', 'aria-labelledby': 'res-title', hidden: true });
  const filesSlot = h('section', { class: 'tv-files', 'aria-labelledby': 'files-title', hidden: true });
  const rawCount = h('span', { class: 'tool-name' });
  const rawBody = h('pre', { class: 'code' });
  const raw = h(
    'details',
    {
      class: 'tool-row',
      hidden: true,
      ontoggle: () => {
        if (raw.open) fillRaw();
      },
    },
    h('summary', null, chev(), rawCount),
    h('div', { class: 'tool-body' }, rawBody),
  );
  const body = h('div', { class: 'sheet-body thread-body' }, statusSlot, planSec, waitingSlot, convo, resultSlot, filesSlot, raw);

  // ---- foot: the composer (step in) with Stop, or a note once it's over
  const stepText = h('textarea', {
    rows: 1,
    placeholder: 'Message this thread',
    'aria-label': 'Message this thread',
    onkeydown: (e) => {
      if (e.key === 'Enter' && !e.shiftKey && !e.isComposing) {
        e.preventDefault();
        sendStepIn();
      }
    },
    oninput: () => updateSendBtn(),
  });
  autoGrow(stepText, 140);
  // A draft carried over from the approval dock takes focus once the footer shows,
  // after the first load (it's hidden until then, so focusing earlier does nothing).
  let focusDraft = false;
  if (drafts.has(threadId)) {
    stepText.value = drafts.get(threadId);
    drafts.delete(threadId);
    focusDraft = true;
  }
  function applyStepInDraft(text) {
    const raw = stepText.value;
    const kept = raw.trim();
    stepText.value = !kept ? text : kept.startsWith(text.trim()) ? raw : `${text}${kept}`;
    stepSel = null;
    focusDraft = true;
    updateSendBtn();
    if (loaded && !loadError && !foot.hidden && !stepForm.hidden && !stepText.disabled) {
      focusDraft = false;
      stepText.focus({ preventScroll: true });
      stepText.setSelectionRange(stepText.value.length, stepText.value.length);
    }
  }
  draftListeners.set(threadId, applyStepInDraft);
  const stepBtn = h('button', { class: 'send-btn', type: 'submit', 'aria-label': 'Send to thread', disabled: true }, icon('send'));
  const stopBtn = h('button', { class: 'btn btn-sm', type: 'button', onclick: () => stop() }, icon('stop'), 'Stop thread');
  const stepError = h('div', { class: 'composer-error', role: 'alert', hidden: true });
  const stepForm = h(
    'form',
    {
      class: 'composer thread-composer',
      onsubmit: (e) => {
        e.preventDefault();
        sendStepIn();
      },
    },
    stepText,
    h('div', { class: 'composer-row' }, stopBtn, h('span', { class: 'spacer' }), stepBtn),
  );
  const footNote = h('p', { class: 'hint foot-note' });
  const foot = h('div', { class: 'sheet-foot thread-foot', hidden: true }, stepError, stepForm, footNote);

  // The approval dock: replaces the foot while this thread's own request waits.
  let stepSel = null;
  const dock = mountApprovalDock({
    conversationId: conversationId || initial?.conversation_id,
    threadId,
    onActiveChange: (docked, { hadFocus }) => {
      if (docked) {
        const typing = foot.contains(document.activeElement);
        stepSel = [stepText.selectionStart, stepText.selectionEnd];
        renderAll();
        if (typing) dock.focusHeading();
      } else {
        renderAll();
        if ((hadFocus || focusDraft) && !foot.hidden && !stepForm.hidden && !stepText.disabled) {
          focusDraft = false;
          stepText.focus({ preventScroll: true });
          if (stepSel) stepText.setSelectionRange(...stepSel);
          else stepText.setSelectionRange(stepText.value.length, stepText.value.length);
        } else if (hadFocus && !container.contains(document.activeElement)) {
          // The thread ended meanwhile (no step-in field): keep focus in this view.
          titleEl.focus({ preventScroll: true });
        }
        stepSel = null;
      }
    },
    // "Change it" here: the draft goes straight into this thread's step-in field.
    onChangeIt: () => {
      applyStepInDraft(CHANGE_DRAFT);
    },
  });
  dock.el.classList.add('sheet-foot', 'thread-dock');

  container.append(head, body, dock.el, foot);
  container.setAttribute('aria-labelledby', 'thread-title');

  function t() {
    if (!verified) return {};
    return store.threads.get(threadId) || info || {};
  }

  function redirectIfWrong(thread) {
    if (!conversationId || !thread?.conversation_id || thread.conversation_id === conversationId) return false;
    aborter.abort();
    onWrongConversation?.(thread.conversation_id);
    return true;
  }

  async function load() {
    const { signal } = aborter;
    const pre = store.threads.get(threadId);
    if (redirectIfWrong(pre)) return;
    try {
      // Only fetch events in parallel once we know the thread belongs to this
      // conversation; otherwise verify the thread's conversation first so a
      // thread from another chat is never shown here.
      let thread;
      let evs;
      if (verified && cid) {
        [thread, evs] = await Promise.all([get(`/v1/threads/${enc(threadId)}`, { signal }), get(`/v1/conversations/${enc(cid)}/events${qs({ thread: threadId })}`, { signal })]);
        if (signal.aborted) return;
        const known = store.threads.get(threadId);
        store.threads.set(threadId, { ...known, ...thread, checkpoint: known?.checkpoint });
        if (redirectIfWrong(thread)) return;
      } else {
        thread = await get(`/v1/threads/${enc(threadId)}`, { signal });
        if (signal.aborted) return;
        const known = store.threads.get(threadId);
        store.threads.set(threadId, { ...known, ...thread, checkpoint: known?.checkpoint });
        if (redirectIfWrong(thread)) return;
        verified = true;
        cid = thread.conversation_id;
        evs = await get(`/v1/conversations/${enc(cid)}/events${qs({ thread: threadId })}`, { signal });
        if (signal.aborted) return;
      }
      info = thread;
      rating = toThumb(thread.rating);
      verified = true;
      cid = thread.conversation_id;
      // Keep events that arrived on the socket while loading.
      const fetched = evs || [];
      const extra = events.filter((e) => e.seq > (fetched.at(-1)?.seq || 0));
      events = [...fetched, ...extra];
      lastSeq = events.at(-1)?.seq || 0;
      loaded = true;
      loadError = null;
      rebuildTranscript();
    } catch (err) {
      if (signal.aborted) return;
      loadError = err;
    }
    renderAll();
  }

  // ---- pop-out: the same chat at full width

  // Popped out over the chat on a wide screen: the chat underneath is covered, so
  // it leaves the tab order (the sidebar stays usable). The shell's own rules run
  // first (syncInert); this only adds to them. Phones are full screen already.
  function syncShell() {
    syncInert();
    if (!popped || isNarrow() || container.hidden) return;
    const main = container.closest('.shell')?.querySelector('.main');
    if (main) main.inert = true;
  }

  function applyMode() {
    container.classList.toggle('is-popout', popped && !isNarrow());
    setAttr(popBtn, 'aria-pressed', popped ? 'true' : 'false');
    setAttr(popBtn, 'title', popped ? 'Back to the side panel' : 'Show the thread full width');
    syncShell();
  }

  function setPopped(next) {
    if (next === popped) return;
    popped = next;
    applyMode();
  }

  function onModeKey(e) {
    // Escape steps back from full width to the panel before it closes the thread.
    if (e.key !== 'Escape' || !popped || isNarrow() || e.defaultPrevented) return;
    if (document.querySelector('dialog[open]')) return;
    e.stopPropagation();
    setPopped(false);
    popBtn.focus({ preventScroll: true });
  }
  container.addEventListener('keydown', onModeKey);

  const narrowMq = matchMedia('(max-width: 760px)');
  const wideMq = matchMedia(WIDE);
  narrowMq.addEventListener('change', applyMode);
  wideMq.addEventListener('change', syncShell);

  // ---- whole-view refresh (cheap parts only; never the transcript)

  function renderAll() {
    const th = t();
    renderHead(th);
    if (loadError) {
      setChildren(
        statusSlot,
        errorBanner(loadError, () => {
          loadError = null;
          renderAll();
          load();
        }),
      );
    } else if (!loaded) {
      setChildren(statusSlot, h('div', { 'aria-busy': 'true', 'aria-label': 'Loading thread' }, skeleton(3), skeleton(3)));
    } else {
      statusSlot.replaceChildren();
    }
    const ready = loaded && !loadError;
    for (const el of [convo, filesSlot, raw]) el.hidden = !ready;
    // While the dock is up it is this view's foot.
    foot.hidden = !ready || dock.isActive();
    if (!ready) {
      planSec.hidden = waitingSlot.hidden = resultSlot.hidden = true;
      return;
    }
    renderPlan(th);
    renderWaiting(th);
    renderResult(th);
    renderFiles();
    renderRawCount();
    renderFoot(th);
    if (focusDraft && !foot.hidden) {
      focusDraft = false;
      if (!stepForm.hidden && !stepText.disabled) {
        stepText.focus();
        stepText.setSelectionRange(stepText.value.length, stepText.value.length);
      }
    }
    refreshToolStates(th);
    renderEmpty(th);
    lastStatus = th.status;
  }

  // ---- header: title, status chip with elapsed time, device, model, reason

  let offTick = null;

  function paintElapsed() {
    const { secs, live } = threadElapsed(t());
    elapsedWrap.hidden = secs === null;
    if (secs === null) return;
    setText(elapsedLabel, live ? ', elapsed ' : ', ');
    setText(elapsedEl, live ? elapsed(secs) : `took ${elapsed(secs)}`);
    setAttr(elapsedEl, 'datetime', isoDuration(secs));
  }

  function syncTicker(th) {
    const want = verified && isActive(th.status) && Boolean(th.created);
    if (want && !offTick) offTick = onTick(paintElapsed);
    else if (!want && offTick) {
      offTick();
      offTick = null;
    }
  }

  function startedEvent() {
    return events.find((e) => e.kind === 'thread_started');
  }

  function renderHead(th) {
    if (!verified) {
      setText(titleEl, 'Thread');
      pillSlot.replaceChildren();
      delete pillSlot.dataset.key;
      deviceChip.hidden = true;
      setText(startedEl, '');
      elapsedWrap.hidden = true;
      modelChip.hidden = true;
      reasonEl.hidden = true;
      syncTicker(th);
      return;
    }
    const approval = approvalsFor(threadId).length > 0;
    const started = startedEvent();
    const reason = th.reason || started?.reason;
    const modelId = th.model || started?.model || '';
    const fam = familyOfId(modelId);
    setText(titleEl, th.title || started?.title || 'Thread');
    const pillKey = `${th.status}|${th.checkpoint ? 1 : 0}|${approval ? 1 : 0}|${fam}`;
    if (pillSlot.dataset.key !== pillKey) {
      pillSlot.dataset.key = pillKey;
      // The art's "Running · 2m": the status pill (family-tinted while it runs)
      // carries the elapsed time.
      const pill = statusPill(th.status, { checkpoint: th.checkpoint, approval, family: fam });
      pill.classList.add('tv-pill');
      pill.append(elapsedWrap);
      setChildren(pillSlot, pill);
    }
    const deviceId = th.device_id || started?.device_id;
    deviceChip.hidden = !deviceId;
    setText(deviceText, deviceName(deviceId));
    const offline = Boolean(deviceId) && !deviceOnline(deviceId);
    deviceState.hidden = !offline;
    setText(deviceState, offline ? 'offline' : '');
    deviceChip.classList.toggle('is-offline', offline);
    setText(startedEl, th.created ? `Started ${time(th.created)} (${ago(th.created)})` : '');
    paintElapsed();
    syncTicker(th);
    renderModelChip(modelId);
    setText(reasonEl, reason || '');
    reasonEl.hidden = !reason;
  }

  function renderModelChip(modelId) {
    modelChip.hidden = false;
    const fam = familyOfId(modelId);
    const entry = store.models.find((m) => m.id === modelId);
    const key = `${modelId}|${fam}|${entry?.provider_id || ''}|${entry?.model || ''}`;
    if (modelChip.dataset.key === key) return;
    modelChip.dataset.key = key;
    setFamilyClass(modelChip, fam);
    setChildren(chipMark, familyMark(fam));
    // The chip stays a fixed label; only a registry model's name links to its entry.
    setChildren(chipName, entry ? h('a', { href: `#/models/${enc(modelId)}` }, modelId) : modelId || 'model not recorded');
    const f = familyInfo(fam);
    const via = entry?.provider_id ? `via ${entry.provider_id}` : '';
    const note = [fam === 'unknown' ? '' : f.label, via, 'fixed'].filter(Boolean).join(' · ');
    setText(chipNote, ` (${note})`);
    setAttr(modelChip, 'title', [entry ? `${entry.provider_id} / ${entry.model}` : modelId, note].filter(Boolean).join(' · '));
  }

  function renderTokens() {
    tokensEl.hidden = !(tokensIn || tokensOut);
    setText(tokensEl, tokensIn || tokensOut ? `${num(tokensIn)} in · ${num(tokensOut)} out tokens` : '');
  }

  // ---- the plan checklist

  let lastPlanDone = null;

  function renderPlan(th) {
    planSec.hidden = !plan;
    if (!plan) {
      delete planList.dataset.key;
      return;
    }
    const active = isActive(th.status);
    const key = `${JSON.stringify(plan)}|${active}`;
    if (planList.dataset.key === key) return;
    planList.dataset.key = key;
    const { done, total } = planProgress(plan);
    setText(planCount, `· ${done} of ${total} done`);
    planFill.style.width = `${Math.round((done / total) * 100)}%`;
    planBox.classList.toggle('is-complete', done === total);
    // While folded, the summary still says what the thread is on.
    const current = plan.find((i) => i.status === 'in_progress');
    setText(planNow, current ? (active ? current.active || current.content : `Stopped at: ${current.content}`) : '');
    planNow.hidden = !current;
    setChildren(
      planList,
      plan.map((item) => {
        // An item still "in progress" on a thread that's over didn't finish: no spinner.
        const state = item.status === 'completed' ? 'done' : item.status === 'in_progress' ? (active ? 'active' : 'unfinished') : 'pending';
        const words = { done: 'Done: ', active: 'In progress: ', unfinished: 'Not finished: ', pending: 'To do: ' }[state];
        const text = state === 'active' && item.active ? item.active : item.content;
        return h(
          'li',
          { class: ['tv-plan-item', `is-${state}`], 'aria-current': state === 'active' ? 'step' : null },
          h('span', { class: 'tv-plan-mark', 'aria-hidden': 'true' }, state === 'done' ? icon('check') : null),
          h('span', { class: 'tv-plan-text' }, h('span', { class: 'visually-hidden' }, words), text),
        );
      }),
    );
    if (lastPlanDone !== null && done !== lastPlanDone) announce(`Plan: ${done} of ${total} done`);
    lastPlanDone = done;
  }

  // ---- attention: waiting and checkpoint

  function renderWaiting(th) {
    const waiting = events.filter((e) => e.kind === 'thread_waiting').at(-1);
    const checkpoint = events.filter((e) => e.kind === 'thread_checkpoint').at(-1);
    const handed = events.filter((e) => e.kind === 'thread_handed_over').at(-1);
    if (th.status !== 'waiting_device' && !checkpoint && !handed) {
      waitingSlot.hidden = true;
      waitingSlot.replaceChildren();
      return;
    }
    const device = store.devices.find((d) => d.id === th.device_id);
    const parts = [];
    if (th.status === 'waiting_device') {
      parts.push(
        h(
          'div',
          { class: 'callout warn tv-state' },
          h('h3', null, icon('offline'), `Waiting for ${deviceName(th.device_id)}`),
          h('p', null, `${deviceName(th.device_id)} is offline. ${device?.last_seen ? `Last seen ${ago(device.last_seen)}.` : 'Not seen yet.'}`),
          waiting?.reason ? h('p', { class: 'hint' }, waiting.reason) : null,
          h('p', { class: 'hint' }, checkpoint ? 'The hub reached a checkpoint. The thread goes on when the device is back.' : 'Meanwhile the hub works towards a checkpoint, then waits for the device.'),
        ),
      );
    }
    if (checkpoint) {
      parts.push(h('div', { class: 'callout tv-state tv-checkpoint' }, h('h3', null, icon('flag'), 'Checkpoint on the hub'), h('div', { class: 'md' }, renderMarkdown(checkpoint.summary))));
    }
    if (handed) {
      parts.push(
        h(
          'div',
          { class: 'callout tv-state' },
          h('h3', null, icon('check'), `Handed over to ${deviceName(handed.device_id)}`),
          handed.files.length ? h('div', null, `Copied: ${handed.files.join(', ')}`) : h('div', null, 'No files to copy.'),
          handed.skipped.length ? h('div', { class: 'hint' }, `Left out: ${handed.skipped.join('; ')}`) : null,
        ),
      );
    }
    waitingSlot.hidden = false;
    setChildren(waitingSlot, parts);
  }

  // ---- the transcript (appended once, never rebuilt while streaming)

  function rebuildTranscript() {
    for (const node of [...convo.children]) if (node !== draftWho && node !== draftEl && node !== emptyEl) node.remove();
    calls.clear();
    planT = planTracker();
    plan = null;
    lastPlanDone = null;
    tokensIn = tokensOut = 0;
    lastSpeaker = null;
    lastOrchestratorInput = null;
    for (const e of events) appendEvent(e);
    renderTokens();
    renderDraft();
  }

  function addItem(node) {
    convo.insertBefore(node, draftWho);
  }

  function threadModel() {
    return t().model || startedEvent()?.model || '';
  }

  /** The thread is about to speak: label it with its model if someone else spoke last. */
  function threadSpeaks() {
    if (lastSpeaker === 'thread') return;
    lastSpeaker = 'thread';
    const modelId = threadModel();
    const fam = familyOfId(modelId);
    addItem(h('div', { class: ['msg-who', 'thread-who', 'has-fam', `fam-${fam}`] }, familyMark(fam), h('span', { class: 'mono' }, modelId || 'Thread model')));
  }

  /** Label the streaming draft with the model unless the thread spoke last. */
  function syncDraftWho() {
    const show = !draftEl.hidden && lastSpeaker !== 'thread';
    draftWho.hidden = !show;
    if (!show) return;
    const modelId = threadModel();
    const fam = familyOfId(modelId);
    if (draftWho.dataset.key === `${modelId}|${fam}`) return;
    draftWho.dataset.key = `${modelId}|${fam}`;
    setFamilyClass(draftWho, fam);
    setChildren(draftWho, familyMark(fam), h('span', { class: 'mono' }, modelId || 'Thread model'));
  }

  /** A message to the thread from the orchestrator or the owner (right-hand bubble). */
  function inbound(from, label, content, { markdown = false } = {}) {
    lastSpeaker = from;
    const who = h('div', { class: 'msg-who' }, label);
    const text = markdown ? h('div', { class: 'md' }, renderMarkdown(content)) : h('div', { class: 'msg-text' }, content);
    const el = h('div', { class: ['msg', 'msg-user', 'msg-inbound', from === 'user' ? 'from-user' : 'from-orchestrator'] }, who, text);
    addItem(el);
    return el;
  }

  /** Fold one stored event into the transcript (and the plan). */
  function appendEvent(e) {
    if (planT.apply(e)) plan = planT.plan;
    if (e.kind === 'thread_started') {
      if (e.brief) inbound('orchestrator', 'Orchestrator · brief', e.brief, { markdown: true });
    } else if (e.kind === 'thread_activity') {
      const a = e.event;
      if (a.type === 'text') {
        threadSpeaks();
        addItem(h('div', { class: 'msg msg-assistant md' }, renderMarkdown(a.text)));
      } else if (a.type === 'tool_call' && a.name === PLAN_TOOL) {
        // The plan is the checklist at the top, not a tool row (planT has it).
        calls.set(a.call_id, { plan: true });
      } else if (a.type === 'tool_call' && a.name === ASK) {
        threadSpeaks();
        const question = typeof a.input?.question === 'string' ? a.input.question : pretty(a.input);
        const el = h('div', { class: 'msg msg-assistant msg-ask' }, h('div', { class: 'msg-ask-label' }, icon('chat'), 'Asked the orchestrator'), h('div', { class: 'msg-text' }, question));
        calls.set(a.call_id, { ask: true, el });
        addItem(el);
      } else if (a.type === 'tool_call') {
        threadSpeaks();
        const row = toolRow(a);
        calls.set(a.call_id, row);
        addItem(row.details);
        updateToolRow(row);
      } else if (a.type === 'tool_result' && (a.name === PLAN_TOOL || calls.get(a.call_id)?.plan)) {
        // Nothing to show: the plan is the call's input.
      } else if (a.type === 'tool_result' && (a.name === ASK || calls.get(a.call_id)?.ask)) {
        answerAsk(a);
      } else if (a.type === 'tool_result') {
        let row = calls.get(a.call_id);
        if (!row) {
          threadSpeaks();
          row = toolRow({ call_id: a.call_id, name: a.name, input: null });
          calls.set(a.call_id, row);
          addItem(row.details);
        }
        row.result = a;
        updateToolRow(row);
      } else if (a.type === 'usage') {
        tokensIn += a.input_tokens + a.cache_read_tokens + a.cache_write_tokens;
        tokensOut += a.output_tokens;
      } else if (a.type === 'notice') addItem(h('div', { class: 'msg msg-note' }, a.text));
    } else if (e.kind === 'thread_input') {
      const fromUser = e.from === 'user';
      const el = inbound(fromUser ? 'user' : 'orchestrator', fromUser ? 'You' : capitalize(e.from || 'orchestrator'), e.text);
      if (!fromUser) lastOrchestratorInput = { text: String(e.text || '').trim(), el, used: false };
    } else if (e.kind === 'thread_handed_over') {
      addItem(h('div', { class: 'msg msg-note' }, icon('check'), `Continued on ${deviceName(e.device_id)}.`));
    } else if (e.kind === 'approval_resolved') {
      const note =
        e.reason === 'timeout' || e.reason === 'timeout_stop'
          ? 'A tool call timed out and was denied.'
          : e.approved
            ? 'You approved a tool call.'
            : 'A tool call was denied.';
      addItem(h('div', { class: 'msg msg-note' }, icon('shield'), note));
    } else if (e.kind === 'error') {
      addItem(h('div', { class: 'msg msg-error' }, icon('warning'), h('div', null, e.message)));
    }
  }

  // The answer to ask_orchestrator. The orchestrator answers with a message to the
  // thread, which already shows as its bubble; label that one instead of repeating it.
  function answerAsk(a) {
    const output = String(a.output || '').trim();
    if (!a.ok) {
      addItem(h('div', { class: 'msg msg-note' }, icon('warning'), `No answer: ${output || 'the question failed'}`));
      return;
    }
    const prev = lastOrchestratorInput;
    if (prev && !prev.used && prev.text === output) {
      prev.used = true;
      setText(prev.el.querySelector('.msg-who'), 'Orchestrator · answer');
      return;
    }
    inbound('orchestrator', 'Orchestrator · answer', output || '(empty answer)');
  }

  function toolRow(call) {
    const state = h('span');
    const preview = h('span', { class: 'tool-arg mono' }, argPreview(call.input));
    const bodyEl = h('div', { class: 'tool-body' });
    const details = h(
      'details',
      {
        class: 'tool-row tool-inline',
        open: openCalls.has(call.call_id),
        ontoggle: () => {
          if (details.open) openCalls.add(call.call_id);
          else openCalls.delete(call.call_id);
        },
      },
      h('summary', null, chev(), icon('tool'), h('span', { class: 'tool-name' }, call.name), preview, state),
      bodyEl,
    );
    return { details, state, body: bodyEl, call, result: null };
  }

  function updateToolRow(row) {
    const { call, result } = row;
    setChildren(
      row.body,
      call.input !== null && call.input !== undefined ? [h('span', { class: 'label' }, 'Input'), h('pre', { class: 'code', 'aria-label': `${call.name} input` }, pretty(call.input))] : null,
      result ? [h('span', { class: 'label' }, 'Output'), h('pre', { class: 'code', 'aria-label': `${call.name} output` }, result.output || '(empty)')] : null,
    );
    setToolState(row, t());
  }

  // The outcome: an icon plus the word (never colour alone).
  function setToolState(row, th) {
    const [cls, word, iconName] = row.result
      ? row.result.ok
        ? ['ok-word', 'ok', 'check']
        : ['fail-word', 'failed', 'x']
      : isActive(th.status)
        ? ['run-word', 'running', null]
        : ['hint', 'no result', null];
    const key = `${cls}|${word}`;
    if (row.state.dataset.key === key) return;
    row.state.dataset.key = key;
    row.state.className = `tv-tool-state ${cls}`;
    setChildren(row.state, iconName ? icon(iconName) : cls === 'run-word' ? h('span', { class: 'live-dot', 'aria-hidden': 'true' }) : null, word);
  }

  function refreshToolStates(th) {
    if (th.status === lastStatus) return;
    for (const row of calls.values()) if (row.details && !row.result) setToolState(row, th);
  }

  function renderEmpty(th) {
    const has = convo.childElementCount > 3 || !draftEl.hidden;
    emptyEl.hidden = has;
    setText(emptyEl, isActive(th.status) ? 'Working… output appears here as it streams.' : 'No output.');
  }

  function renderDraft() {
    scheduleDraft.cancel();
    const draft = liveOf(threadId).draft;
    if (!draft) {
      if (!draftEl.hidden) {
        draftEl.hidden = true;
        draftEl.replaceChildren();
      }
    } else {
      const atEnd = nearEnd();
      draftEl.replaceChildren(renderMarkdown(draft));
      const lastBlock = draftEl.lastElementChild;
      (lastBlock && /^(P|LI|H\d)$/.test(lastBlock.tagName) ? lastBlock : draftEl).append(h('span', { class: 'caret', 'aria-hidden': 'true' }));
      draftEl.hidden = false;
      if (atEnd) body.scrollTop = body.scrollHeight;
    }
    syncDraftWho();
    renderEmpty(t());
  }

  const scheduleDraft = throttle(renderDraft, 100);

  function nearEnd() {
    return body.scrollHeight - body.scrollTop - body.clientHeight < 60;
  }

  // ---- result (done, failed, stopped), rating, files, raw events

  function ratingWords(value) {
    if (value === 'up') return 'Rated: good result';
    if (value === 'down') return 'Rated: poor result';
    return 'Not rated yet';
  }

  function renderResult(th) {
    if (isActive(th.status)) {
      resultSlot.hidden = true;
      resultSlot.replaceChildren();
      delete resultSlot.dataset.key;
      return;
    }
    const finished = events.filter((e) => e.kind === 'thread_finished').at(-1);
    const lastError = events.filter((e) => e.kind === 'error').at(-1);
    const summary = th.summary || finished?.summary;
    const key = `${th.status}|${th.finished}|${summary}|${lastError?.seq}|${isAdmin()}`;
    if (resultSlot.dataset.key === key) {
      for (const b of resultSlot.querySelectorAll('.thumb')) b.setAttribute('aria-pressed', b.dataset.value === rating ? 'true' : 'false');
      const state = resultSlot.querySelector('.rating-state');
      if (state) state.textContent = ratingWords(rating);
      return; // keep focus on the thumbs
    }
    resultSlot.dataset.key = key;
    const thumb = (value, iconName, label) =>
      h(
        'button',
        {
          class: 'icon-btn thumb',
          type: 'button',
          'aria-label': label,
          'aria-pressed': rating === value ? 'true' : 'false',
          'data-value': value,
          disabled: !isAdmin(),
          onclick: () => rate(rating === value ? 'clear' : value),
        },
        icon(iconName),
      );
    // Each ending has its own look: done (mint), failed (red), stopped (neutral).
    const look =
      th.status === 'done'
        ? { cls: ['tv-done'], icon: 'check', word: 'Result' }
        : th.status === 'failed'
          ? { cls: ['danger', 'tv-failed'], icon: 'x', word: 'Failed' }
          : { cls: ['tv-stopped'], icon: 'stop', word: 'Stopped' };
    const after = th.status === 'failed' ? 'Ask in the chat to try again.' : th.status === 'stopped' ? 'It stopped before it finished. Ask in the chat to pick it up again.' : null;
    // A failure's own error, when the summary doesn't already say it.
    const err = th.status === 'failed' && lastError?.message && !String(summary || '').includes(lastError.message) ? lastError.message : null;
    resultSlot.hidden = false;
    setChildren(
      resultSlot,
      h(
        'div',
        { class: ['callout', 'tv-state', ...look.cls] },
        h('h3', { id: 'res-title' }, icon(look.icon), look.word, th.finished ? h('span', { class: 'tv-when' }, dateTime(th.finished)) : null),
        summary ? h('div', { class: 'md tv-summary' }, renderMarkdown(summary)) : h('p', { class: 'hint' }, 'No summary.'),
        err ? h('p', { class: 'tv-error' }, icon('warning'), h('span', null, err)) : null,
        after ? h('p', { class: 'hint' }, after) : null,
        h('div', { class: 'rating', role: 'group', 'aria-label': 'Rate this result' }, h('span', { class: 'hint rating-state' }, ratingWords(rating)), thumb('up', 'up', 'Good result'), thumb('down', 'downvote', 'Poor result')),
      ),
    );
  }

  async function rate(value) {
    try {
      const res = await post(`/v1/threads/${enc(threadId)}/rating`, { rating: value });
      rating = value === 'clear' ? null : value;
      const numeric = res && 'rating' in res ? res.rating : value === 'up' ? 1 : value === 'down' ? -1 : null;
      if (info) info = { ...info, rating: numeric };
      const cached = store.threads.get(threadId);
      if (cached) store.threads.set(threadId, { ...cached, rating: numeric });
      for (const b of resultSlot.querySelectorAll('.thumb')) b.setAttribute('aria-pressed', b.dataset.value === rating ? 'true' : 'false');
      const state = resultSlot.querySelector('.rating-state');
      if (state) state.textContent = ratingWords(rating);
      toast(value === 'clear' ? 'Rating cleared.' : 'Thanks. Ratings feed the model registry.');
    } catch (err) {
      toast(describe(err), 'error');
    }
  }

  function renderFiles() {
    let content;
    if (filesLoading) content = skeleton(2);
    else if (!files) content = h('button', { class: 'btn btn-sm', type: 'button', disabled: !isAdmin(), onclick: loadFiles }, icon('file'), 'Show files');
    else if (files.error) content = h('div', null, h('p', { class: 'hint' }, files.error), h('button', { class: 'btn btn-sm', type: 'button', onclick: loadFiles }, 'Try again'));
    else if (!files.files.length) content = h('p', { class: 'hint' }, 'No files in this thread’s workspace.');
    else {
      content = h(
        'div',
        { class: 'files' },
        files.files.map((f) =>
          h(
            'a',
            {
              class: 'file-row',
              href: `/v1/threads/${enc(threadId)}/files/${f.path.split('/').map(enc).join('/')}`,
              download: f.path.split('/').pop(),
            },
            icon('file'),
            h('span', { class: 'name' }, f.path),
            h('span', { class: 'size' }, size(f.size)),
            icon('download'),
          ),
        ),
        files.truncated ? h('p', { class: 'hint' }, 'More files exist than are listed.') : null,
        h('p', { class: 'hint' }, `On ${deviceName(files.device_id)}.`),
      );
    }
    const key = `${filesLoading}|${files ? JSON.stringify(files) : ''}|${isAdmin()}`;
    if (filesSlot.dataset.key === key) return;
    const hadFocus = filesSlot.contains(document.activeElement);
    filesSlot.dataset.key = key;
    const count = files && !files.error ? ` (${files.files.length}${files.truncated ? '+' : ''})` : '';
    setChildren(filesSlot, h('h3', { class: 'sub-title', id: 'files-title' }, `Files${count}`), content);
    if (hadFocus) (filesSlot.querySelector('a, button') || titleEl).focus({ preventScroll: true });
  }

  async function loadFiles() {
    filesLoading = true;
    renderFiles();
    try {
      files = await get(`/v1/threads/${enc(threadId)}/files`, { signal: aborter.signal });
    } catch (err) {
      if (aborter.signal.aborted) return;
      files = { error: describe(err) };
    }
    filesLoading = false;
    renderFiles();
  }

  function renderRawCount() {
    setText(rawCount, `Show events (${events.length})`);
  }

  function fillRaw() {
    setText(rawBody, events.map((e) => JSON.stringify(e)).join('\n'));
  }

  // ---- foot: step in and stop, or a note once the thread is over

  function canAct() {
    return isAdmin() && store.online;
  }

  function updateSendBtn() {
    stepBtn.disabled = !canAct() || sendingStep || !stepText.value.trim();
  }

  function renderFoot(th) {
    const active = isActive(th.status);
    stepForm.hidden = !active;
    stepText.disabled = !canAct();
    stopBtn.disabled = !canAct();
    updateSendBtn();
    stepText.placeholder = !isAdmin() ? 'Sign in with an admin token to message threads' : 'Message this thread';
    stopBtn.className = th.status === 'running' ? 'btn btn-sm' : 'btn btn-sm btn-ghost';
    let note = '';
    if (!active) note = th.status === 'done' ? 'This thread has finished. Ask in the chat to follow up.' : `This thread has ${th.status === 'failed' ? 'failed' : 'stopped'}. Ask in the chat to try again.`;
    else if (!isAdmin()) note = 'Reading only: sign in with an admin token to message or stop threads.';
    else if (!store.online) note = 'Offline. You can message or stop the thread once the hub reconnects.';
    setText(footNote, note);
    footNote.hidden = !note;
  }

  async function sendStepIn() {
    const text = stepText.value.trim();
    if (!text || sendingStep || !canAct() || dock.isActive()) return;
    sendingStep = true;
    updateSendBtn();
    stepError.hidden = true;
    try {
      await post(`/v1/threads/${enc(threadId)}/message`, { text });
      stepText.value = '';
      toast('Sent to the thread. The orchestrator is told too.');
    } catch (err) {
      // The draft stays in the field so nothing typed is lost.
      stepError.textContent = describe(err);
      stepError.hidden = false;
    } finally {
      sendingStep = false;
      renderFoot(t());
    }
  }

  async function stop() {
    stopBtn.disabled = true;
    try {
      await stopThread(threadId);
    } catch (err) {
      stepError.textContent = describe(err);
      stepError.hidden = false;
    } finally {
      renderFoot(t());
    }
  }

  // ---- live updates

  const scheduleAll = throttle(() => {
    if (!container.isConnected) return;
    renderAll();
  }, 100);

  subs.push(
    on('event', (e) => {
      if (e.thread_id !== threadId) return;
      if (conversationId && e.conversation_id && e.conversation_id !== conversationId) return;
      if (e.seq > 0) {
        if (e.seq <= lastSeq) return;
        events.push(e);
        lastSeq = e.seq;
        if (!loaded) return; // the load keeps it and draws everything
        const atEnd = nearEnd();
        appendEvent(e);
        syncDraftWho(); // e.g. a step-in mid-stream: the draft below it needs its label
        if (isPlanEvent(e)) renderPlan(t());
        if (e.kind === 'thread_activity' && e.event.type === 'usage') renderTokens();
        if (e.kind === 'thread_activity' && e.event.type === 'text') {
          speaker.reset();
          renderDraft();
        }
        if (raw.open) fillRaw();
        renderRawCount();
        if (e.kind === 'thread_finished') {
          announce(`Thread ${e.status === 'done' ? 'finished' : e.status}`, { force: true });
          renderDraft();
        }
        if (!['thread_activity', 'thread_input'].includes(e.kind)) scheduleAll();
        else renderEmpty(t());
        if (atEnd) body.scrollTop = body.scrollHeight;
      } else if (e.kind === 'thread_activity' && e.event.type === 'text_delta') {
        if (!verified) return;
        scheduleDraft();
        speaker.speak(liveOf(threadId).draft);
      }
    }),
    on('threads', () => {
      const th = store.threads.get(threadId);
      if (redirectIfWrong(th)) return;
      if (!verified && th?.conversation_id === conversationId) {
        verified = true;
        info = th;
      }
      scheduleAll();
    }),
    on('approvals', scheduleAll),
    on('devices', scheduleAll),
    on('models', scheduleAll),
    on('online', (online) => {
      // A dropped socket clears the draft in the store (the stored text repairs it):
      // take the stale partial text and its caret off the screen too.
      if (!online && loaded) {
        speaker.reset();
        renderDraft();
      }
      scheduleAll();
    }),
    // The shell recomputes what is inert on every route; keep a covered chat covered.
    on('route', () => {
      if (popped) syncShell();
    }),
  );

  applyMode();
  renderAll();
  dock.start();
  if (initial && conversationId && initial.conversation_id && initial.conversation_id !== conversationId) {
    queueMicrotask(() => {
      if (!aborter.signal.aborted) redirectIfWrong(initial);
    });
  } else {
    load();
  }
  requestAnimationFrame(() => {
    // A draft carried over from the approval dock already took focus, and a deep
    // link to a request focused the dock's heading.
    if (document.activeElement !== stepText && !dock.el.contains(document.activeElement)) titleEl.focus();
  });

  return () => {
    if (draftListeners.get(threadId) === applyStepInDraft) draftListeners.delete(threadId);
    dock.destroy();
    for (const off of subs) off();
    offTick?.();
    offTick = null;
    aborter.abort();
    scheduleAll.cancel();
    scheduleDraft.cancel();
    container.removeEventListener('keydown', onModeKey);
    narrowMq.removeEventListener('change', applyMode);
    wideMq.removeEventListener('change', syncShell);
    container.classList.remove('thread-view', 'is-popout');
    if (popped) {
      popped = false;
      syncInert();
    }
  };
}

function chev() {
  const c = icon('chevron');
  c.classList.add('chev');
  return c;
}

/** The plan's fold chevron (down when folded, up when open). */
function cardChev() {
  const c = icon('chevron');
  c.classList.add('tv-chev');
  return c;
}

/** A short look at a tool call's input for its collapsed row: the first string argument. */
function argPreview(input) {
  if (input === null || input === undefined) return '';
  let value = input;
  if (typeof input === 'object') {
    value = Object.values(input).find((v) => typeof v === 'string' && v.trim()) ?? '';
  }
  const line = String(value).replace(/\s+/g, ' ').trim();
  return line.length > 60 ? `${line.slice(0, 60)}…` : line;
}

function capitalize(s) {
  return s ? s[0].toUpperCase() + s.slice(1) : s;
}

function size(bytes) {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}
