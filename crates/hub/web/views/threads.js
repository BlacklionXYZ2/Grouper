// Threads for the open conversation (needs you, then running, then finished).
// When no conversation is open, shows only the chats that have active threads
// or pending approvals (chat title + counts only, no thread titles or contents).

import { h, icon, setChildren, setText, setAttr, keepFocus } from '../lib/dom.js';
import { enc } from '../lib/api.js';
import { store, on, deviceName, threadsIn, approvalsFor, approvalsIn, pendingApprovals } from '../lib/store.js';
import { loadThreads, loadApprovals } from '../lib/live.js';
import { bar, statusPill, skeletonBlocks, errorBanner } from '../lib/ui.js';
import { ago, isActive, plural } from '../lib/format.js';
import { openApproval } from './approvals.js';

export function renderThreads(container, { conversation: cid = null } = {}) {
  const chat = () => (cid ? store.conversations.find((c) => c.id === cid) : null);

  const chatLink = h('a', { class: 'btn btn-sm', href: cid ? `#/chat/${enc(cid)}` : '#/chat', hidden: !cid }, icon('chat'), 'Chat');
  const refreshBtn = h('button', { class: 'icon-btn', type: 'button', 'aria-label': 'Refresh', onclick: () => refresh() }, icon('refresh'));
  const barEl = bar({
    title: cid ? chat()?.title || 'Conversation' : 'Threads',
    caption: ' ',
    actions: [chatLink, refreshBtn],
  });
  const titleEl = barEl.querySelector('h1');
  const captionEl = barEl.querySelector('.caption');

  const body = h('div', { class: 'view-body narrow' });
  const view = h('section', { class: 'view' }, barEl, body);
  container.append(view);
  let loaded = store.threadsLoaded || store.threads.size > 0;
  let error = null;

  function renderHeader() {
    if (cid) {
      const c = chat();
      const name = c?.title || 'Conversation';
      const ts = threadsIn(cid);
      const running = ts.filter((t) => t.status === 'running').length;
      const waiting = ts.filter((t) => t.status === 'waiting_device').length;
      const apprs = approvalsIn(cid).length;
      const parts = [];
      if (apprs) parts.push(plural(apprs, 'approval'));
      if (running) parts.push(`${running} running`);
      if (waiting) parts.push(`${waiting} waiting`);
      const caption = parts.length ? `Threads · ${parts.join(' · ')}` : 'Threads · Needs you, then running, then finished';
      setText(titleEl, name);
      setText(captionEl, caption);
      chatLink.hidden = false;
      setAttr(chatLink, 'href', `#/chat/${enc(cid)}`);
      document.title = `${name} · Threads · Grouper`;
    } else {
      setText(titleEl, 'Threads');
      setText(captionEl, 'Chats with active threads or pending approvals');
      chatLink.hidden = true;
      document.title = 'Threads · Grouper';
    }
  }

  function row(t) {
    const approvals = approvalsFor(t.id);
    return h(
      'li',
      null,
      h(
        'a',
        { class: 'list-row', href: `#/chat/${enc(cid)}/thread/${enc(t.id)}` },
        h(
          'div',
          { class: 'grow' },
          h('div', { class: 'row-title' }, t.title),
          h('div', { class: 'meta' }, h('span', null, deviceName(t.device_id)), h('span', { class: 'mono' }, t.model), h('span', null, ago(t.finished || t.created))),
          t.summary && !isActive(t.status) ? h('div', { class: 'row-sub' }, clip(t.summary)) : null,
        ),
        statusPill(t.status, { checkpoint: t.checkpoint, approval: approvals.length > 0 }),
      ),
    );
  }

  function approvalRow(a) {
    const t = store.threads.get(a.thread_id);
    return h(
      'li',
      null,
      h(
        'button',
        { class: 'list-row', type: 'button', id: `approval-row-${a.approval_id}`, onclick: () => openApproval(a.approval_id, { conversationId: cid }) },
        icon('shield'),
        h(
          'div',
          { class: 'grow' },
          h('div', { class: 'row-title' }, `${a.tool} on ${deviceName(a.device_id)}`),
          h('div', { class: 'row-sub' }, a.reason),
          h('div', { class: 'meta' }, t ? h('span', null, t.title) : null, h('span', null, ago(a.ts))),
        ),
        h('span', { class: 'btn btn-sm btn-primary' }, 'Review'),
      ),
    );
  }

  function chatSummaryRow(item) {
    const counts = [];
    if (item.running) counts.push(h('span', null, `${item.running} running`));
    if (item.waiting) counts.push(h('span', null, `${item.waiting} waiting`));
    if (item.approvals) counts.push(h('span', null, plural(item.approvals, 'approval')));
    return h(
      'li',
      null,
      h(
        'a',
        { class: 'list-row', href: `#/chat/${enc(item.id)}` },
        icon('chat'),
        h('div', { class: 'grow' }, h('div', { class: 'row-title' }, item.title), h('div', { class: 'meta' }, counts)),
        item.approvals ? h('span', { class: 'count', 'aria-label': `${plural(item.approvals, 'approval')} waiting` }, String(item.approvals)) : null,
        icon('chevron'),
      ),
    );
  }

  function section(title, items, empty) {
    return h(
      'section',
      { class: 'section' },
      h('div', { class: 'section-head' }, h('h2', null, title)),
      items.length ? h('ul', { class: 'list card' }, items) : h('p', { class: 'hint' }, empty),
    );
  }

  function renderChatsOverview() {
    const convIds = new Set();
    for (const t of store.threads.values()) {
      if (isActive(t.status) && t.conversation_id) convIds.add(t.conversation_id);
    }
    for (const a of pendingApprovals()) {
      if (a.conversation_id) convIds.add(a.conversation_id);
    }
    const chats = [...convIds]
      .map((id) => {
        const c = store.conversations.find((x) => x.id === id);
        const ts = threadsIn(id);
        const apprs = approvalsIn(id);
        const running = ts.filter((t) => t.status === 'running').length;
        const waiting = ts.filter((t) => t.status === 'waiting_device').length;
        const latest = Math.max(0, ...ts.filter((t) => isActive(t.status)).map((t) => t.created || 0), ...apprs.map((a) => a.ts || 0));
        return {
          id,
          title: c?.title || 'Untitled conversation',
          running,
          waiting,
          approvals: apprs.length,
          latest,
        };
      })
      .sort((a, b) => Number(b.approvals > 0) - Number(a.approvals > 0) || Number(b.waiting > 0) - Number(a.waiting > 0) || b.running - a.running || b.latest - a.latest);

    if (!chats.length) {
      setChildren(
        body,
        h(
          'div',
          { class: 'empty' },
          h('h2', null, 'No active threads'),
          h('p', null, 'Open a chat to see its threads, or start a new project chat.'),
          h('p', null, h('a', { href: '#/chat/new' }, 'Start a project chat')),
        ),
      );
      return;
    }

    keepFocus(body, () =>
      setChildren(
        body,
        h('p', { class: 'section-note' }, 'Threads belong to their chat. Select a chat to see its threads and approvals.'),
        h('ul', { class: 'list card' }, chats.map(chatSummaryRow)),
      ),
    );
  }

  function render() {
    renderHeader();
    if (error) {
      setChildren(body, errorBanner(error, refresh));
      return;
    }
    if (!loaded) {
      setChildren(body, skeletonBlocks(4));
      return;
    }
    if (!cid) {
      renderChatsOverview();
      return;
    }

    const all = threadsIn(cid);
    const approvals = approvalsIn(cid);
    const withApproval = new Set(approvals.map((a) => a.thread_id));
    const waiting = all.filter((t) => t.status === 'waiting_device' && !withApproval.has(t.id));
    const running = all.filter((t) => t.status === 'running' && !withApproval.has(t.id)).sort((a, b) => b.created - a.created);
    const finished = all.filter((t) => !isActive(t.status)).sort((a, b) => (b.finished || b.created) - (a.finished || a.created));
    if (!all.length && !approvals.length) {
      const standard = chat()?.mode === 'standard';
      setChildren(
        body,
        h(
          'div',
          { class: 'empty' },
          h('h2', null, standard ? 'Standard chat' : 'No threads in this chat yet'),
          h(
            'p',
            null,
            standard
              ? 'Standard chats use one model and do not start threads.'
              : 'Threads appear here when this chat hands work to one of your devices or the hub.',
          ),
          h('p', null, h('a', { href: `#/chat/${enc(cid)}` }, 'Back to chat')),
        ),
      );
      return;
    }
    // Live updates redraw the lists; the person keeps their place.
    keepFocus(body, () =>
      setChildren(
        body,
        section('Needs you', [...approvals.map(approvalRow), ...waiting.map(row)], 'Nothing is waiting for you.'),
        section('Running', running.map(row), 'Nothing running.'),
        section('Finished', finished.slice(0, 60).map(row), 'Nothing finished yet.'),
      ),
    );
  }

  async function refresh() {
    error = null;
    try {
      await Promise.all([loadThreads(), loadApprovals({ strict: true })]);
      loaded = true;
    } catch (e) {
      error = e;
    }
    render();
  }

  const offs = [on('threads', render), on('approvals', render), on('devices', render), on('conversations', render)];
  render();
  refresh();
  return () => offs.forEach((off) => off());
}

function clip(text) {
  const line = String(text).replace(/[*_`#>]/g, '').replace(/\s+/g, ' ').trim();
  return line.length > 180 ? `${line.slice(0, 180)}…` : line;
}
