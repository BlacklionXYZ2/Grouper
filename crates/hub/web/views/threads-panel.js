// Threads for the open chat: a labelled region docked on the right on wide
// screens, and a modal bottom sheet on phones. The chat view owns the URL
// (`#/chat/<id>/threads`); this module only shows, hides and fills the panel.
// Live updates go through syncList so focus and the panel's scroll stay put.
//
// Each section is a card like the chat's threads card (W1's .thread-group), and a
// thread row is a .thread-line: family dot, title (with device, time and plan
// progress under it), model chip, status pill tinted by family while it runs, and
// a chevron. Running and waiting rows tick through the shared one-second timer
// (`onTick`), which only rewrites the time text and runs only while the panel is
// open and a row is live. Plan progress ("2/5") comes from the thread's latest
// update_plan call in the chat's stored events (see views/thread.js).

import { h, icon, setChildren, setText, setAttr, syncList } from '../lib/dom.js';
import { enc } from '../lib/api.js';
import { store, on, deviceName, threadsIn, approvalsIn } from '../lib/store.js';
import { isNarrow, statusPill, skeletonBlocks, onTick, navigate } from '../lib/ui.js';
import { ago, isActive, elapsed, isoDuration, threadElapsed } from '../lib/format.js';
import { familyOfId, familyLabel, setFamilyClass } from '../lib/family.js';
import { approvalHref } from './approvals.js';
import { isPlanEvent, plansIn, planProgress } from './thread.js';

const NARROW = '(max-width: 760px)';

/**
 * @param {HTMLElement} host the chat view (panel and dialog are appended here)
 * @param {{ cid: string, trigger: HTMLElement, onRequestClose: () => void }} opts
 */
export function mountThreadsPanel(host, { cid, trigger, onRequestClose }) {
  let open = false;

  const titleEl = h('h2', { id: 'threads-panel-title', tabindex: '-1' }, 'Threads');
  const captionEl = h('p', { class: 'caption' });
  const closeBtn = h(
    'button',
    { class: 'icon-btn', type: 'button', 'aria-label': 'Close threads', onclick: () => onRequestClose() },
    icon('close'),
  );
  const head = h('header', { class: 'sheet-head' }, h('div', { class: 'sheet-head-row' }, titleEl, closeBtn), captionEl);

  const skeleton = skeletonBlocks(3);
  const emptyTitle = h('h3', null, 'No threads yet');
  const emptyBody = h('p', null, '');
  const emptyEl = h('div', { class: 'empty', hidden: true }, emptyTitle, emptyBody);

  const needsList = h('ul', { class: 'thread-group-list' });
  const runningList = h('ul', { class: 'thread-group-list' });
  const finishedList = h('ul', { class: 'thread-group-list' });
  const group = (id, label, iconName, list) => {
    const count = h('span', { class: 'tp-count' });
    const sec = h('section', { class: 'thread-group tp-group', 'aria-labelledby': id, hidden: true }, h('h3', { class: 'thread-group-head', id }, icon(iconName), label, count), list);
    sec.count = count;
    return sec;
  };
  const needsSec = group('threads-needs', 'Needs you', 'shield', needsList);
  needsSec.classList.add('tp-needs');
  const runningSec = group('threads-running', 'Running', 'thread-tree', runningList);
  const finishedSec = group('threads-finished', 'Finished', 'check', finishedList);

  const body = h('div', { class: 'threads-panel-body' }, skeleton, emptyEl, needsSec, runningSec, finishedSec);
  const inner = h('div', { id: 'threads-panel', class: 'threads-panel', role: 'region', 'aria-labelledby': 'threads-panel-title' }, head, body);

  const dock = h('div', { class: 'threads-dock', hidden: true }, inner);
  const dialog = h('dialog', { id: 'threads-sheet', class: 'sheet-dialog threads-sheet', 'aria-labelledby': 'threads-panel-title' });
  host.append(dock, dialog);

  function narrow() {
    return isNarrow();
  }

  function syncTrigger() {
    setAttr(trigger, 'aria-expanded', open ? 'true' : 'false');
    if (narrow()) {
      setAttr(trigger, 'aria-haspopup', 'dialog');
      setAttr(trigger, 'aria-controls', 'threads-sheet');
    } else {
      setAttr(trigger, 'aria-haspopup', null);
      setAttr(trigger, 'aria-controls', 'threads-panel');
    }
  }

  function place(moveFocus) {
    const phone = narrow();
    const focusedInside = inner.contains(document.activeElement);
    if (phone) {
      inner.setAttribute('role', 'presentation');
      if (inner.parentNode !== dialog) dialog.append(inner);
      dock.hidden = true;
      if (!dialog.open) dialog.showModal();
    } else {
      inner.setAttribute('role', 'region');
      if (dialog.open) dialog.close();
      if (inner.parentNode !== dock) dock.append(inner);
      dock.hidden = false;
    }
    syncTrigger();
    if (moveFocus || focusedInside) titleEl.focus({ preventScroll: true });
  }

  function hide({ restoreFocus = false } = {}) {
    open = false;
    syncTicker();
    const restore = restoreFocus && trigger?.isConnected ? trigger : null;
    // Close the sheet before focusing the button: a modal dialog makes the
    // rest of the page inert, so the button cannot take focus while it is open.
    // Focus before parking the panel in the hidden dock. Hiding a focused
    // descendant (and dialog.close()) can still move focus to the body after
    // this function returns, so the button is put back once that settles.
    if (dialog.open) dialog.close();
    if (restore) restore.focus({ preventScroll: true });
    if (inner.parentNode !== dock) dock.append(inner);
    dock.hidden = true;
    inner.setAttribute('role', 'region');
    syncTrigger();
    if (restore && document.activeElement !== restore) restore.focus({ preventScroll: true });
    if (!restore) return;
    const settle = () => {
      if (open || !restore.isConnected || document.activeElement === restore) return;
      restore.focus({ preventScroll: true });
    };
    queueMicrotask(settle);
    requestAnimationFrame(settle);
  }

  function threadRow() {
    const a = h(
      'a',
      { class: 'thread-line tp-row fam-unknown' },
      // The row's fam-<key> class colours the dot (and the running pill).
      h('span', { class: 'fam-dot tp-dot', 'aria-hidden': 'true' }),
      h(
        'span',
        { class: 'tp-main' },
        h('span', { class: 'thread-line-title' }),
        h(
          'span',
          { class: 'tp-meta' },
          h('span', { class: 'tp-plan', hidden: true }, icon('check'), h('span', { class: 'visually-hidden' }, 'Plan: '), h('span', { class: 'tp-plan-n' }), h('span', { class: 'visually-hidden tp-plan-words' })),
          h('span', { class: 'tp-device' }),
          h('span', { class: 'tp-time' }, icon('clock'), h('span', { class: 'visually-hidden tp-time-label' }), h('time', { class: 'tp-elapsed' })),
          h('span', { class: 'tp-ago', hidden: true }),
        ),
        h('span', { class: 'tp-sub', hidden: true }),
      ),
      h('span', { class: 'model-tag tp-model' }, h('span', { class: 'visually-hidden tp-fam' }), h('span', { class: 'tp-model-name' })),
      h('span', { class: 'tp-pill' }),
      icon('chevron'),
    );
    return h('li', null, a);
  }

  // Plan progress per thread: the latest update_plan call in the chat's stored
  // events. Rebuilt only when the stored log grew.
  let plans = new Map();
  let plansAt = -1;
  function planOf(threadId) {
    const c = store.convs.get(cid);
    const at = c?.loaded ? c.lastSeq : -1;
    if (at !== plansAt) {
      plansAt = at;
      plans = plansIn(c?.events || []);
    }
    return plans.get(threadId) || null;
  }

  /** Paint one row's elapsed time. Live rows keep `data-live` (their start) for the tick. */
  function paintElapsed(el, label, t) {
    const { secs, live } = threadElapsed(t);
    if (live) el.dataset.live = String(t.created);
    else delete el.dataset.live;
    setText(label, live ? 'Elapsed ' : ''); // finished rows already read "took …"
    setText(el, secs === null ? '—' : live ? elapsed(secs) : `took ${elapsed(secs)}`);
    setAttr(el, 'datetime', secs === null ? null : isoDuration(secs));
  }

  function updateThread(li, t) {
    const a = li.querySelector('a');
    const needs = approvalsIn(cid).some((x) => x.thread_id === t.id);
    setAttr(a, 'href', `#/chat/${enc(cid)}/thread/${enc(t.id)}`);
    setText(a.querySelector('.thread-line-title'), t.title || 'Thread');
    const summary = !isActive(t.status) && t.summary ? clip(t.summary) : '';
    const sub = a.querySelector('.tp-sub');
    sub.hidden = !summary;
    setText(sub, summary);
    setText(a.querySelector('.tp-device'), deviceName(t.device_id));
    const fam = familyOfId(t.model);
    if (a.dataset.fam !== fam) {
      a.dataset.fam = fam;
      setFamilyClass(a, fam);
      setText(a.querySelector('.tp-fam'), `${familyLabel(fam)}: `);
    }
    setText(a.querySelector('.tp-model-name'), t.model || 'model not recorded');
    const plan = planOf(t.id);
    const planEl = a.querySelector('.tp-plan');
    planEl.hidden = !plan;
    if (plan) {
      const { done, total } = planProgress(plan);
      setText(a.querySelector('.tp-plan-n'), `${done}/${total}`);
      setText(a.querySelector('.tp-plan-words'), ` (${done} of ${total} steps done)`);
      planEl.classList.toggle('is-complete', done === total);
    }
    paintElapsed(a.querySelector('.tp-elapsed'), a.querySelector('.tp-time-label'), t);
    const agoEl = a.querySelector('.tp-ago');
    agoEl.hidden = isActive(t.status) || !t.finished;
    setText(agoEl, agoEl.hidden ? '' : ago(t.finished));
    const pill = a.querySelector('.tp-pill');
    const key = `${t.status}|${t.checkpoint ? 1 : 0}|${needs ? 1 : 0}|${fam}`;
    if (pill.dataset.key !== key) {
      pill.dataset.key = key;
      setChildren(pill, statusPill(t.status, { checkpoint: t.checkpoint, approval: needs, family: fam }));
    }
  }

  function approvalRow() {
    return h(
      'li',
      null,
      h(
        'button',
        { class: 'thread-line tp-approval', type: 'button' },
        icon('shield'),
        h(
          'span',
          { class: 'tp-main' },
          h('span', { class: 'thread-line-title' }),
          h('span', { class: 'tp-sub' }),
          h('span', { class: 'tp-meta' }, h('span', { class: 'tp-thread' }), h('span', { class: 'tp-time' })),
        ),
        h('span', { class: 'btn btn-sm btn-primary' }, 'Review'),
      ),
    );
  }

  function updateApproval(li, a) {
    const btn = li.querySelector('button');
    // Review closes the panel and opens the chat's approval dock on this request.
    btn.onclick = () => navigate(approvalHref(a));
    setAttr(btn, 'id', `approval-row-${a.approval_id}`);
    setText(btn.querySelector('.thread-line-title'), `${a.tool} on ${deviceName(a.device_id)}`);
    const sub = btn.querySelector('.tp-sub');
    sub.hidden = !a.reason;
    setText(sub, a.reason || '');
    const t = store.threads.get(a.thread_id);
    setText(btn.querySelector('.tp-thread'), t?.title || 'Thread');
    setText(btn.querySelector('.tp-time'), ago(a.ts));
  }

  function emptyCopy() {
    const mode = store.conversations.find((c) => c.id === cid)?.mode;
    if (mode === 'standard') {
      return { title: 'Standard chat', body: 'Standard chats use one model and do not start threads.' };
    }
    return {
      title: 'No threads yet',
      body: 'Threads appear here when this chat hands work to one of your devices or the hub.',
    };
  }

  function render() {
    const loaded = store.threadsLoaded || store.threads.size > 0;
    const all = threadsIn(cid);
    const approvals = approvalsIn(cid);
    const withApproval = new Set(approvals.map((a) => a.thread_id));
    // Needs you: an approval, or waiting on a device. Those threads stay out of
    // Running / Finished so a thread is listed once. Approval rows sit above them.
    const needsThreads = all
      .filter((t) => withApproval.has(t.id) || t.status === 'waiting_device')
      .sort((a, b) => Number(withApproval.has(b.id)) - Number(withApproval.has(a.id)) || (b.created || 0) - (a.created || 0));
    const needsIds = new Set(needsThreads.map((t) => t.id));
    const needsCount = approvals.length + all.filter((t) => t.status === 'waiting_device' && !withApproval.has(t.id)).length;
    const running = all.filter((t) => t.status === 'running' && !needsIds.has(t.id)).sort((a, b) => (b.created || 0) - (a.created || 0));
    const finished = all
      .filter((t) => !isActive(t.status) && !needsIds.has(t.id))
      .sort((a, b) => (b.finished || b.created || 0) - (a.finished || a.created || 0))
      .slice(0, 60);
    const loading = !loaded;

    const parts = [];
    if (needsCount) parts.push(needsCount === 1 ? '1 needs you' : `${needsCount} need you`);
    if (running.length) parts.push(`${running.length} running`);
    setText(captionEl, loading ? 'Loading…' : parts.length ? parts.join(' · ') : 'Needs you, then running, then finished');

    skeleton.hidden = !loading;
    if (loading) {
      emptyEl.hidden = needsSec.hidden = runningSec.hidden = finishedSec.hidden = true;
      return;
    }

    const needsItems = [
      ...approvals.map((a) => ({ kind: 'approval', key: `a:${a.approval_id}`, approval: a })),
      ...needsThreads.map((t) => ({ kind: 'thread', key: `t:${t.id}`, thread: t })),
    ];
    syncList(
      needsList,
      needsItems,
      (item) => item.key,
      (item) => (item.kind === 'approval' ? approvalRow() : threadRow()),
      (li, item) => (item.kind === 'approval' ? updateApproval(li, item.approval) : updateThread(li, item.thread)),
    );
    syncList(runningList, running, (t) => t.id, threadRow, updateThread);
    syncList(finishedList, finished, (t) => t.id, threadRow, updateThread);

    needsSec.hidden = needsItems.length === 0;
    runningSec.hidden = running.length === 0;
    finishedSec.hidden = finished.length === 0;
    setText(needsSec.count, needsCount ? String(needsCount) : '');
    setText(runningSec.count, running.length ? String(running.length) : '');
    setText(finishedSec.count, finished.length ? String(finished.length) : '');
    const any = needsItems.length || running.length || finished.length;
    emptyEl.hidden = Boolean(any);
    if (!any) {
      const msg = emptyCopy();
      setText(emptyTitle, msg.title);
      setText(emptyBody, msg.body);
    }
    syncTicker();
  }

  // ---- the live elapsed times: one shared tick, only while open with a live row

  let offTick = null;

  function tick() {
    const now = Date.now() / 1000;
    for (const el of body.querySelectorAll('.tp-elapsed[data-live]')) {
      const secs = now - Number(el.dataset.live);
      setText(el, elapsed(secs));
      setAttr(el, 'datetime', isoDuration(secs));
    }
  }

  function syncTicker() {
    const want = open && Boolean(body.querySelector('.tp-elapsed[data-live]'));
    if (want && !offTick) offTick = onTick(tick);
    else if (!want && offTick) {
      offTick();
      offTick = null;
    }
  }

  // Arrow keys move between rows across the sections; Home and End jump to the ends.
  function onRowKey(e) {
    if (!['ArrowDown', 'ArrowUp', 'Home', 'End'].includes(e.key)) return;
    const row = e.target.closest?.('.thread-line');
    if (!row || !body.contains(row) || e.altKey || e.ctrlKey || e.metaKey) return;
    const rows = [...body.querySelectorAll('section:not([hidden]) .thread-line')];
    const i = rows.indexOf(row);
    if (i < 0) return;
    const next = e.key === 'Home' ? 0 : e.key === 'End' ? rows.length - 1 : e.key === 'ArrowDown' ? Math.min(rows.length - 1, i + 1) : Math.max(0, i - 1);
    e.preventDefault();
    rows[next].focus();
  }
  body.addEventListener('keydown', onRowKey);

  function onKey(e) {
    if (e.key !== 'Escape' || !open || narrow()) return;
    if (document.querySelector('dialog[open]')) return;
    if (e.defaultPrevented) return;
    e.preventDefault();
    onRequestClose();
  }

  function onCancel(e) {
    e.preventDefault();
    onRequestClose();
  }

  function onBackdrop(e) {
    if (e.target === dialog) onRequestClose();
  }

  function onMq() {
    if (!open) {
      syncTrigger();
      return;
    }
    place(false);
  }

  const mq = matchMedia(NARROW);
  mq.addEventListener('change', onMq);
  dialog.addEventListener('cancel', onCancel);
  dialog.addEventListener('click', onBackdrop);
  document.addEventListener('keydown', onKey);
  const offs = [
    on('threads', render),
    on('approvals', render),
    on('devices', render),
    on('conversations', render),
    on('models', render),
    // A thread updated its plan: its row's progress changes.
    on('stored', (e) => {
      if (open && e.conversation_id === cid && isPlanEvent(e)) render();
    }),
  ];
  syncTrigger();
  render();

  return {
    isOpen: () => open,
    focus() {
      titleEl.focus({ preventScroll: true });
    },
    /** Show or hide. `moveFocus` lands on the heading; `restoreFocus` returns to the button. */
    setOpen(next, { moveFocus = false, restoreFocus = false } = {}) {
      if (next === open) return;
      if (!next) {
        hide({ restoreFocus });
        return;
      }
      open = true;
      place(moveFocus);
      render();
    },
    destroy() {
      mq.removeEventListener('change', onMq);
      dialog.removeEventListener('cancel', onCancel);
      dialog.removeEventListener('click', onBackdrop);
      document.removeEventListener('keydown', onKey);
      body.removeEventListener('keydown', onRowKey);
      offs.forEach((off) => off());
      open = false;
      syncTicker();
      if (dialog.open) dialog.close();
      dock.remove();
      dialog.remove();
    },
  };
}

function clip(text) {
  const line = String(text).replace(/[*_`#>]/g, '').replace(/\s+/g, ' ').trim();
  return line.length > 180 ? `${line.slice(0, 180)}…` : line;
}
