// Threads across conversations: what needs you (approvals, threads waiting for a
// device), then running, then finished.

import { h, icon, setChildren, keepFocus } from '../lib/dom.js';
import { store, on, deviceName, approvalsFor, pendingApprovals } from '../lib/store.js';
import { loadThreads, scanApprovals } from '../lib/live.js';
import { bar, statusPill, skeletonBlocks, errorBanner } from '../lib/ui.js';
import { ago } from '../lib/format.js';
import { openApproval } from './approvals.js';

export function renderThreads(container) {
  const body = h('div', { class: 'view-body narrow' });
  const view = h(
    'section',
    { class: 'view' },
    bar({
      title: 'Threads',
      caption: 'Needs you, then running, then finished',
      actions: [h('button', { class: 'icon-btn', type: 'button', 'aria-label': 'Refresh', onclick: () => refresh() }, icon('refresh'))],
    }),
    body,
  );
  container.append(view);
  let loaded = store.threads.size > 0;
  let error = null;

  function row(t) {
    const approvals = approvalsFor(t.id);
    return h(
      'li',
      null,
      h(
        'a',
        { class: 'list-row', href: `#/chat/${encodeURIComponent(t.conversation_id)}/thread/${encodeURIComponent(t.id)}` },
        h(
          'div',
          { class: 'grow' },
          h('div', { class: 'row-title' }, t.title),
          h('div', { class: 'meta' }, h('span', null, deviceName(t.device_id)), h('span', { class: 'mono' }, t.model), h('span', null, ago(t.finished || t.created))),
          t.summary && !['running', 'waiting_device'].includes(t.status) ? h('div', { class: 'row-sub' }, clip(t.summary)) : null,
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
        { class: 'list-row', type: 'button', id: `approval-row-${a.approval_id}`, onclick: () => openApproval(a.approval_id) },
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

  function section(title, items, empty) {
    return h(
      'section',
      { class: 'section' },
      h('div', { class: 'section-head' }, h('h2', null, title)),
      items.length ? h('ul', { class: 'list card' }, items) : h('p', { class: 'hint' }, empty),
    );
  }

  function render() {
    if (error) {
      setChildren(body, errorBanner(error, refresh));
      return;
    }
    if (!loaded) {
      setChildren(body, skeletonBlocks(4));
      return;
    }
    const all = [...store.threads.values()];
    const approvals = pendingApprovals();
    const withApproval = new Set(approvals.map((a) => a.thread_id));
    const waiting = all.filter((t) => t.status === 'waiting_device' && !withApproval.has(t.id));
    const running = all.filter((t) => t.status === 'running' && !withApproval.has(t.id)).sort((a, b) => b.created - a.created);
    const finished = all.filter((t) => !['running', 'waiting_device'].includes(t.status)).sort((a, b) => (b.finished || b.created) - (a.finished || a.created));
    if (!all.length) {
      setChildren(
        body,
        h('div', { class: 'empty' }, h('h2', null, 'No threads yet'), h('p', null, 'Threads appear when a project chat hands work to one of your devices or the hub.'), h('p', null, h('a', { href: '#/chat/new' }, 'Start a project chat'))),
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
      await loadThreads();
      await scanApprovals();
      loaded = true;
    } catch (e) {
      error = e;
    }
    render();
  }

  const offs = [on('threads', render), on('approvals', render), on('devices', render)];
  render();
  refresh();
  return () => offs.forEach((off) => off());
}

function clip(text) {
  const line = String(text).replace(/[*_`#>]/g, '').replace(/\s+/g, ' ').trim();
  return line.length > 180 ? `${line.slice(0, 180)}…` : line;
}
