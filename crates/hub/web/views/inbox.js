// Needs you (#/inbox): everything waiting on the owner, across every chat.
//
//   1. Pending approvals, oldest first (the dock's order on every client),
//      grouped by chat. Tool, device, the plain reason, age, thread and chat.
//      Review only navigates: it opens the request's chat with the approval dock
//      on that request (#/chat/<id>?approval=<id>); a row never answers. Tool
//      arguments are never shown here (they can hold secrets; spec 4.3).
//   2. Threads waiting for a device, grouped by chat: device, last seen,
//      checkpoint or not, age, and the thread's model with its family accent.
//      Each opens its chat with the thread in the inspector.
//   3. With "All active" on (off by default), running threads too.
//
// This is an attention filter, not a thread catalog: every row leads back to the
// thread's own chat. Live updates go through syncList so focus and scroll stay
// put. An approval answered elsewhere (or dropped because its thread ended) stays
// a while as a settled row with no actions, then goes. New approvals are
// announced once each. Offline, the cached rows stay; Review still opens the
// dock, which shows the request read-only until the hub reconnects.

import { h, icon, setChildren, setText, setAttr, syncList } from '../lib/dom.js';
import { enc } from '../lib/api.js';
import { store, on, deviceName, waitingThreads, byRequestOrder } from '../lib/store.js';
import { navigate } from '../lib/ui.js';
import { bar, statusPill, skeletonBlocks, announce, isOffline } from '../lib/ui.js';
import { ago } from '../lib/format.js';
import { familyOfId, familyMark, familyLabel, setFamilyClass } from '../lib/family.js';
import { isManualReason, approvalHref } from './approvals.js';

const ALL_ACTIVE = 'grouper.inbox.allActive';
const LAST_CONV = 'grouper.lastConversation';
const SETTLED_MS = 20000; // how long an answered request stays as a settled row
let uid = 0;

function readFlag(key) {
  try {
    return localStorage.getItem(key) === '1';
  } catch {
    return false;
  }
}

function writeFlag(key, on) {
  try {
    if (on) localStorage.setItem(key, '1');
    else localStorage.removeItem(key);
  } catch {
    /* storage unavailable */
  }
}

function lastConversation() {
  try {
    return localStorage.getItem(LAST_CONV);
  } catch {
    return null;
  }
}

function chatTitle(cid) {
  if (!cid) return 'Unknown chat';
  return store.conversations.find((c) => c.id === cid)?.title || 'Untitled chat';
}

/** Group items by chat. Groups keep the order of their first item. */
function byChat(items, cidOf) {
  const groups = new Map();
  for (const item of items) {
    const cid = cidOf(item) || '';
    if (!groups.has(cid)) groups.set(cid, []);
    groups.get(cid).push(item);
  }
  return [...groups].map(([cid, list]) => ({ cid, items: list }));
}

function deviceLine(id) {
  const name = deviceName(id);
  if (id === 'hub') return name;
  const d = store.devices.find((x) => x.id === id);
  if (!d) return name;
  if (d.online) return `${name} · online`;
  return `${name} · offline, ${d.last_seen ? `last seen ${ago(d.last_seen)}` : 'never seen'}`;
}

export function renderInbox(container) {
  const mountedAt = Date.now();
  let allActive = readFlag(ALL_ACTIVE);
  let offlineSince = null; // when the connection dropped, if this page saw it

  // ---- skeleton of the page

  const toggle = h('input', {
    type: 'checkbox',
    role: 'switch',
    checked: allActive,
    'aria-describedby': 'inbox-toggle-hint',
    onchange: () => {
      allActive = toggle.checked;
      writeFlag(ALL_ACTIVE, allActive);
      render();
    },
  });
  const toggleHint = h('span', { id: 'inbox-toggle-hint', class: 'visually-hidden' }, 'Also list running threads');
  const toggleLabel = h('label', { class: 'switch inbox-toggle' }, toggle, h('span', null, 'All active'), toggleHint);

  const header = bar({ title: 'Needs you', caption: 'Loading…', actions: [toggleLabel] });
  const captionEl = header.querySelector('.caption');

  const offlineNote = h('div', { class: 'banner warn', role: 'status', hidden: true }, icon('offline'), h('span', { class: 'grow' }));
  const skeleton = skeletonBlocks(3);

  const emptyLink = h('a', { href: '#/chat' });
  const empty = h(
    'div',
    { class: 'empty inbox-empty', hidden: true },
    h('h2', null, 'Nothing needs you right now'),
    h('p', null, 'Approvals and threads waiting for a device show up here.'),
    h('p', null, emptyLink),
  );

  const section = (key, title) => {
    const id = `inbox-${key}`;
    const heading = h('h2', { class: 'inbox-h2', id }, h('span', { class: 'inbox-h2-text' }, title), h('span', { class: 'inbox-h2-count' }));
    const groups = h('div', { class: 'inbox-groups' });
    const el = h('section', { class: 'inbox-section', 'aria-labelledby': id, hidden: true }, heading, groups);
    return { el, heading, groups };
  };
  const approvalsSec = section('approvals', 'Approvals');
  const waitingSec = section('waiting', 'Waiting for a device');
  const runningSec = section('running', 'Running');

  const body = h('div', { class: 'view-body narrow inbox' }, offlineNote, skeleton, empty, approvalsSec.el, waitingSec.el, runningSec.el);
  container.append(h('section', { class: 'view' }, header, body));

  // ---- groups (one per chat) and rows

  function groupEl(kind) {
    const id = `inbox-${kind}-g${++uid}`;
    const link = h('a', { class: 'inbox-chat-link' });
    const heading = h('h3', { class: 'inbox-chat', id }, h('span', { class: 'visually-hidden' }, 'Chat: '), link);
    const list = h('ul', { class: 'list card inbox-list', 'aria-labelledby': id });
    return h('section', { class: 'inbox-group', 'aria-labelledby': id }, heading, list);
  }

  function updateGroup(sec, group, create, update) {
    const link = sec.querySelector('.inbox-chat-link');
    setText(link, chatTitle(group.cid));
    setAttr(link, 'href', group.cid ? `#/chat/${enc(group.cid)}` : '#/chat');
    syncList(sec.querySelector('ul'), group.items, (it) => it.key, create, update);
  }

  function approvalRow() {
    const review = h('button', { class: 'btn btn-sm btn-primary inbox-primary inbox-review', type: 'button' }, 'Review');
    const threadLink = h('a', { class: 'inbox-thread-link' });
    return h(
      'li',
      null,
      h(
        'div',
        { class: 'list-row inbox-row inbox-approval' },
        h('span', { class: 'inbox-icon' }, icon('shield')),
        h(
          'div',
          { class: 'grow' },
          h('div', { class: 'row-title' }, h('span', { class: 'mono inbox-tool' }), h('span', { class: 'inbox-on' })),
          h('div', { class: 'row-sub inbox-reason' }),
          h(
            'div',
            { class: 'meta' },
            h('span', { class: 'inbox-kind' }),
            h('span', null, 'Thread: ', threadLink),
            h('span', { class: 'inbox-in' }),
            h('span', { class: 'inbox-age' }),
          ),
        ),
        h('div', { class: 'inbox-action' }, review),
      ),
    );
  }

  function updateApproval(li, item) {
    const a = item.approval;
    const device = deviceName(a.device_id);
    const thread = store.threads.get(a.thread_id);
    const cid = a.conversation_id || thread?.conversation_id;
    const row = li.firstChild;
    setText(row.querySelector('.inbox-tool'), a.tool);
    setText(row.querySelector('.inbox-on'), ` on ${device}`);
    setText(row.querySelector('.inbox-reason'), a.reason || 'This tool is set to Manual, so every call waits for you.');
    setText(row.querySelector('.inbox-kind'), isManualReason(a.reason) ? 'Manual tool' : 'A safety rule asked');
    row.querySelector('.inbox-kind').classList.toggle('inbox-risk', !isManualReason(a.reason));
    const threadLink = row.querySelector('.inbox-thread-link');
    setText(threadLink, thread?.title || 'Open thread');
    setAttr(threadLink, 'href', cid ? `#/chat/${enc(cid)}/thread/${enc(a.thread_id)}` : `#/threads/${enc(a.thread_id)}`);
    setText(row.querySelector('.inbox-in'), `in ${chatTitle(cid)}`);
    setText(row.querySelector('.inbox-age'), `Requested ${ago(a.ts)}`);

    const btn = row.querySelector('.inbox-review');
    const settled = item.settled;
    row.classList.toggle('settled', Boolean(settled));
    if (settled) {
      // Stays focusable (aria-disabled, not disabled) so focus on it isn't
      // dropped on the page when the request settles.
      btn.onclick = null;
      btn.disabled = false;
      setAttr(btn, 'aria-disabled', 'true');
      setAttr(btn, 'aria-label', `${a.tool} on ${device}${thread?.title ? `, thread ${thread.title}` : ''}: ${settled}`);
      btn.className = 'btn btn-sm inbox-primary inbox-review inbox-settled';
      if (btn.dataset.state !== 'settled') {
        btn.dataset.state = 'settled';
        setChildren(btn, icon('check'), settled);
      }
      return;
    }
    if (btn.dataset.state === 'settled') {
      delete btn.dataset.state;
      btn.className = 'btn btn-sm btn-primary inbox-primary inbox-review';
      setChildren(btn, 'Review');
    }
    // Review opens the dock in the request's chat; Back returns here.
    btn.onclick = () => navigate(approvalHref(a));
    setAttr(btn, 'aria-disabled', null);
    setAttr(btn, 'aria-label', `Review ${a.tool} on ${device}${thread?.title ? `, thread ${thread.title}` : ''}`);
    btn.disabled = false;
    setAttr(btn, 'title', isOffline() ? 'Offline: the request opens read-only until the hub reconnects' : null);
  }

  function threadRow() {
    return h(
      'li',
      null,
      h(
        'a',
        { class: 'list-row thread-row inbox-row inbox-primary fam-unknown' },
        h(
          'div',
          { class: 'grow' },
          h('div', { class: 'row-title' }),
          h(
            'div',
            { class: 'meta tp-meta' },
            h('span', { class: 'tp-model' }, h('span', { class: 'tp-mark' }), h('span', { class: 'visually-hidden tp-fam' }), h('span', { class: 'mono tp-model-name' })),
            h('span', { class: 'inbox-device' }),
            h('span', { class: 'inbox-checkpoint' }),
            h('span', { class: 'inbox-age' }),
          ),
        ),
        h('span', { class: 'tp-pill' }),
      ),
    );
  }

  function updateThread(li, item) {
    const t = item.thread;
    const a = li.firstChild;
    setAttr(a, 'href', t.conversation_id ? `#/chat/${enc(t.conversation_id)}/thread/${enc(t.id)}` : `#/threads/${enc(t.id)}`);
    setText(a.querySelector('.row-title'), t.title || 'Thread');
    const fam = familyOfId(t.model);
    if (a.dataset.fam !== fam) {
      a.dataset.fam = fam;
      setFamilyClass(a, fam);
      setChildren(a.querySelector('.tp-mark'), familyMark(fam));
      setText(a.querySelector('.tp-fam'), `${familyLabel(fam)}: `);
    }
    setText(a.querySelector('.tp-model-name'), t.model || 'model not recorded');
    const waiting = t.status === 'waiting_device';
    setText(a.querySelector('.inbox-device'), waiting ? deviceLine(t.device_id) : deviceName(t.device_id));
    const cp = a.querySelector('.inbox-checkpoint');
    cp.hidden = !waiting;
    setText(cp, waiting ? (t.checkpoint ? 'Checkpoint saved' : 'No checkpoint') : '');
    setText(a.querySelector('.inbox-age'), `Started ${ago(t.created)}`);
    const pill = a.querySelector('.tp-pill');
    const key = `${t.status}|${t.checkpoint ? 1 : 0}`;
    if (pill.dataset.key !== key) {
      pill.dataset.key = key;
      setChildren(pill, statusPill(t.status, { checkpoint: t.checkpoint }));
    }
  }

  // ---- settled approvals and announcements

  const known = new Map(store.approvals); // approvals seen on the last render
  const settled = new Map(); // id -> {approval, text, at}
  const announced = new Set(store.approvals.keys());
  let settleTimer = 0;

  function trackApprovals() {
    for (const [id, a] of known) {
      if (store.approvals.has(id) || settled.has(id)) continue;
      const res = store.resolutions.get(id);
      const text =
        res?.reason === 'timeout' || res?.reason === 'timeout_stop'
          ? 'Timed out'
          : res?.reason === 'dropped'
            ? 'No longer waiting'
            : store.answered.has(id)
              ? 'Answered'
              : 'No longer waiting';
      settled.set(id, { approval: a, text, at: Date.now() });
    }
    known.clear();
    for (const [id, a] of store.approvals) {
      known.set(id, a);
      settled.delete(id); // came back (a reload raced an answer)
    }
    const now = Date.now();
    for (const [id, s] of settled) if (now - s.at >= SETTLED_MS) settled.delete(id);
    clearTimeout(settleTimer);
    if (settled.size) {
      const next = Math.min(...[...settled.values()].map((s) => s.at + SETTLED_MS)) - now;
      settleTimer = setTimeout(render, Math.max(250, next));
    }

    // One announcement per new approval: those that arrived live after this page opened.
    const fresh = [...store.approvals.values()].filter((a) => !announced.has(a.approval_id) && (a.arrivedAt || 0) >= mountedAt);
    for (const a of store.approvals.values()) announced.add(a.approval_id);
    if (fresh.length) {
      const names = fresh.map((a) => `${a.tool} on ${deviceName(a.device_id)}`);
      announce(fresh.length === 1 ? `${names[0]} needs review` : `${fresh.length} new approvals need review: ${names.join(', ')}`, { force: true });
    }
  }

  // ---- render

  function render() {
    trackApprovals();
    const offline = isOffline();
    const loaded = store.threadsLoaded || store.threads.size > 0 || store.approvals.size > 0 || settled.size > 0 || offline;

    // Approvals, oldest first (ties by id); settled ones keep their place until they go.
    const approvalItems = [
      ...[...store.approvals.values()].map((a) => ({ key: `a:${a.approval_id}`, approval: a, settled: null })),
      ...[...settled.values()].map((s) => ({ key: `a:${s.approval.approval_id}`, approval: s.approval, settled: s.text })),
    ].sort((x, y) => byRequestOrder(x.approval, y.approval));
    const approvalGroups = byChat(approvalItems, (it) => it.approval.conversation_id || store.threads.get(it.approval.thread_id)?.conversation_id);

    const newestFirst = (x, y) => (y.created || 0) - (x.created || 0);
    const waiting = waitingThreads().sort(newestFirst);
    const waitingGroups = byChat(
      waiting.map((t) => ({ key: `t:${t.id}`, thread: t })),
      (it) => it.thread.conversation_id,
    );
    const withApproval = new Set([...store.approvals.values()].map((a) => a.thread_id));
    const running = allActive ? [...store.threads.values()].filter((t) => t.status === 'running' && !withApproval.has(t.id)).sort(newestFirst) : [];
    const runningGroups = byChat(
      running.map((t) => ({ key: `t:${t.id}`, thread: t })),
      (it) => it.thread.conversation_id,
    );

    // Caption: the counts in words.
    const parts = [];
    const pending = store.approvals.size;
    if (pending) parts.push(pending === 1 ? '1 approval' : `${pending} approvals`);
    if (waiting.length) parts.push(`${waiting.length} waiting for a device`);
    if (allActive && running.length) parts.push(`${running.length} running`);
    setText(captionEl, !loaded ? 'Loading…' : parts.length ? parts.join(' · ') : 'Nothing waiting on you');

    // Offline: the cached rows stay, answering is paused.
    offlineNote.hidden = !offline;
    if (offline) {
      const since = offlineSince ? ` from ${ago(offlineSince / 1000)}` : '';
      setText(offlineNote.querySelector('.grow'), `Offline · showing the last synced copy${since}. Answering is paused until the hub reconnects.`);
    }

    skeleton.hidden = loaded;
    if (!loaded) {
      empty.hidden = approvalsSec.el.hidden = waitingSec.el.hidden = runningSec.el.hidden = true;
      return;
    }

    // Keep the person's place if the row they were on goes away.
    const active = document.activeElement;
    const rowsBefore = [...body.querySelectorAll('.inbox-primary')];
    const focusIndex = body.contains(active) ? rowsBefore.findIndex((el) => el === active || el.contains(active) || el.closest('li')?.contains(active)) : -1;

    syncList(approvalsSec.groups, approvalGroups, (g) => g.cid, () => groupEl('a'), (sec, g) => updateGroup(sec, g, approvalRow, updateApproval));
    syncList(waitingSec.groups, waitingGroups, (g) => g.cid, () => groupEl('w'), (sec, g) => updateGroup(sec, g, threadRow, updateThread));
    syncList(runningSec.groups, runningGroups, (g) => g.cid, () => groupEl('r'), (sec, g) => updateGroup(sec, g, threadRow, updateThread));

    approvalsSec.el.hidden = !approvalItems.length;
    waitingSec.el.hidden = !waiting.length;
    runningSec.el.hidden = !running.length;
    setText(approvalsSec.heading.querySelector('.inbox-h2-count'), pending ? ` · ${pending}` : '');
    setText(waitingSec.heading.querySelector('.inbox-h2-count'), waiting.length ? ` · ${waiting.length}` : '');
    setText(runningSec.heading.querySelector('.inbox-h2-count'), running.length ? ` · ${running.length}` : '');

    const nothing = !approvalItems.length && !waiting.length;
    empty.hidden = !nothing;
    if (nothing) {
      const last = lastConversation();
      const title = last ? store.conversations.find((c) => c.id === last)?.title : null;
      setAttr(emptyLink, 'href', last ? `#/chat/${enc(last)}` : '#/chat');
      setText(emptyLink, title ? `Back to ${title}` : last ? 'Back to your chat' : 'Start a chat');
    }

    if (focusIndex >= 0 && (!document.activeElement || document.activeElement === document.body)) {
      const rows = [...body.querySelectorAll('section:not([hidden]) .inbox-primary')];
      const target = rows[Math.min(focusIndex, rows.length - 1)] || (nothing ? emptyLink : null) || container.querySelector('h1');
      target?.focus({ preventScroll: true });
    }
  }

  // Arrow keys move between rows' main controls across sections; Home and End jump to the ends.
  function onRowKey(e) {
    if (!['ArrowDown', 'ArrowUp', 'Home', 'End'].includes(e.key) || e.altKey || e.ctrlKey || e.metaKey) return;
    const li = e.target.closest?.('.inbox-list > li');
    if (!li || !body.contains(li)) return;
    const rows = [...body.querySelectorAll('section:not([hidden]) .inbox-primary')];
    const current = rows.findIndex((el) => li.contains(el));
    if (current < 0) return;
    const next = e.key === 'Home' ? 0 : e.key === 'End' ? rows.length - 1 : e.key === 'ArrowDown' ? Math.min(rows.length - 1, current + 1) : Math.max(0, current - 1);
    e.preventDefault();
    rows[next].focus();
  }
  body.addEventListener('keydown', onRowKey);

  // Ages ("2m ago") drift; refresh the text every half minute.
  const ageTimer = setInterval(() => {
    if (!document.hidden) render();
  }, 30000);

  const offs = [
    on('approvals', render),
    on('threads', render),
    on('devices', render),
    on('conversations', render),
    on('models', render),
    on('online', (online) => {
      offlineSince = online ? null : offlineSince || Date.now();
      render();
    }),
  ];
  render();

  return () => {
    offs.forEach((off) => off());
    clearInterval(ageTimer);
    clearTimeout(settleTimer);
    body.removeEventListener('keydown', onRowKey);
  };
}
