// The thread sheet (inspector on wide screens, a sheet on narrow ones): live
// activity, waiting and checkpoint state, step-in, Stop, files and rating.

import { h, icon, setChildren } from '../lib/dom.js';
import { get, post, qs, enc, describe } from '../lib/api.js';
import { renderMarkdown } from '../lib/md.js';
import { store, on, liveOf, deviceName, approvalsFor, isAdmin } from '../lib/store.js';
import { statusPill, skeleton, confirmAction, toast, autoGrow, errorBanner } from '../lib/ui.js';
import { ago, dateTime, num, pretty, isActive } from '../lib/format.js';
import { openApproval } from './approvals.js';

const drafts = new Map();

/** Put text in a thread's step-in field (used by the approval sheet's "Change"). */
export function setStepInDraft(threadId, text) {
  drafts.set(threadId, text);
}

export function renderThread(container, { threadId, conversationId, onClose }) {
  const subs = [];
  let info = store.threads.get(threadId) || null;
  let cid = conversationId || info?.conversation_id;
  let events = [];
  let lastSeq = 0;
  let loaded = false;
  let loadError = null;
  let frame = 0;
  let rating = null;
  let files = null; // null: not loaded; {error} or listing
  let filesLoading = false;
  const openCalls = new Set();
  let showRaw = false;

  const head = h('header', { class: 'sheet-head' });
  const body = h('div', { class: 'sheet-body' });
  const foot = h('div', { class: 'sheet-foot' });
  container.append(head, body, foot);

  // Step-in field, built once so typing survives re-renders.
  const stepText = h('textarea', {
    class: 'textarea',
    rows: 1,
    placeholder: 'Message this thread',
    'aria-label': 'Message this thread',
    onkeydown: (e) => {
      if (e.key === 'Enter' && !e.shiftKey && !e.isComposing) {
        e.preventDefault();
        sendStepIn();
      }
    },
  });
  autoGrow(stepText, 140);
  if (drafts.has(threadId)) {
    stepText.value = drafts.get(threadId);
    drafts.delete(threadId);
    requestAnimationFrame(() => {
      stepText.focus();
      stepText.setSelectionRange(stepText.value.length, stepText.value.length);
    });
  }
  const stepBtn = h('button', { class: 'btn btn-primary', type: 'button', onclick: () => sendStepIn() }, 'Send');
  const stepError = h('div', { class: 'composer-error', role: 'alert', hidden: true });
  const stopBtn = h('button', { class: 'btn', type: 'button', onclick: () => stop() }, icon('stop'), 'Stop thread');

  function t() {
    return store.threads.get(threadId) || info || {};
  }

  async function load() {
    try {
      const [thread, evs] = await Promise.all([
        get(`/v1/threads/${enc(threadId)}`),
        (async () => {
          if (!cid) cid = (await get(`/v1/threads/${enc(threadId)}`)).conversation_id;
          return get(`/v1/conversations/${enc(cid)}/events${qs({ thread: threadId })}`);
        })(),
      ]);
      info = thread;
      const known = store.threads.get(threadId);
      store.threads.set(threadId, { ...known, ...thread, checkpoint: known?.checkpoint });
      cid = thread.conversation_id;
      // Keep events that arrived on the socket while loading.
      const extra = events.filter((e) => e.seq > ((evs || []).at(-1)?.seq || 0));
      events = [...(evs || []), ...extra];
      lastSeq = events.at(-1)?.seq || 0;
      loaded = true;
    } catch (err) {
      loadError = err;
    }
    render();
  }

  function schedule() {
    if (frame) return;
    frame = requestAnimationFrame(() => {
      frame = 0;
      render();
    });
  }

  function render() {
    const th = t();
    renderHead(th);
    if (loadError) {
      setChildren(body, errorBanner(loadError, () => {
        loadError = null;
        render();
        load();
      }));
      foot.hidden = true;
      return;
    }
    if (!loaded) {
      setChildren(body, h('div', { 'aria-busy': 'true', 'aria-label': 'Loading thread' }, skeleton(3), skeleton(3)));
      foot.hidden = true;
      return;
    }
    const keepScroll = body.scrollTop;
    const atEnd = body.scrollHeight - body.scrollTop - body.clientHeight < 60;
    setChildren(body, approvalsSection(), waitingSection(th), activitySection(th), resultSection(th), filesSection(), rawSection());
    body.scrollTop = atEnd ? body.scrollHeight : keepScroll;
    renderFoot(th);
  }

  function renderHead(th) {
    const approval = approvalsFor(threadId).length > 0;
    const started = events.find((e) => e.kind === 'thread_started');
    const reason = th.reason || started?.reason;
    setChildren(
      head,
      h(
        'div',
        { class: 'sheet-head-row' },
        h('h2', { id: 'thread-title', tabindex: '-1' }, th.title || started?.title || 'Thread'),
        h('button', { class: 'icon-btn', type: 'button', 'aria-label': 'Close thread', onclick: onClose }, icon('close')),
      ),
      h(
        'div',
        { class: 'meta' },
        statusPill(th.status, { checkpoint: th.checkpoint, approval }),
        h('span', null, deviceName(th.device_id)),
        h('span', { class: 'mono' }, th.model || ''),
        th.created ? h('span', null, `started ${ago(th.created)}`) : null,
      ),
      reason ? h('p', { class: 'hint' }, reason) : null,
    );
    container.setAttribute('aria-labelledby', 'thread-title');
  }

  function approvalsSection() {
    const list = approvalsFor(threadId);
    if (!list.length) return null;
    return h(
      'div',
      null,
      list.map((a) =>
        h(
          'div',
          { class: 'banner warn' },
          icon('shield'),
          h('div', { class: 'grow' }, h('strong', null, a.tool), ` on ${deviceName(a.device_id)} is waiting for your approval.`),
          h('button', { class: 'btn btn-sm btn-primary', type: 'button', onclick: () => openApproval(a.approval_id) }, 'Review'),
        ),
      ),
    );
  }

  function waitingSection(th) {
    const waiting = events.filter((e) => e.kind === 'thread_waiting').at(-1);
    const checkpoint = events.filter((e) => e.kind === 'thread_checkpoint').at(-1);
    const handed = events.filter((e) => e.kind === 'thread_handed_over').at(-1);
    if (th.status !== 'waiting_device' && !checkpoint && !handed) return null;
    const device = store.devices.find((d) => d.id === th.device_id);
    const parts = [];
    if (th.status === 'waiting_device') {
      parts.push(
        h(
          'div',
          { class: 'callout warn' },
          h('h3', null, icon('offline'), `${deviceName(th.device_id)} is offline`),
          h('div', null, device?.last_seen ? `Last seen ${ago(device.last_seen)}.` : 'Not seen yet.'),
          waiting?.reason ? h('div', { class: 'hint' }, waiting.reason) : null,
          !checkpoint ? h('div', { class: 'hint' }, 'Meanwhile the hub works towards a checkpoint, then waits for the device.') : null,
        ),
      );
    }
    if (checkpoint) {
      parts.push(
        h(
          'div',
          { class: 'callout' },
          h('h3', null, icon('flag'), 'Checkpoint on the hub'),
          h('div', { class: 'md' }, renderMarkdown(checkpoint.summary)),
        ),
      );
    }
    if (handed) {
      parts.push(
        h(
          'div',
          { class: 'callout' },
          h('h3', null, icon('check'), `Handed over to ${deviceName(handed.device_id)}`),
          handed.files.length ? h('div', null, `Copied: ${handed.files.join(', ')}`) : h('div', null, 'No files to copy.'),
          handed.skipped.length ? h('div', { class: 'hint' }, `Left out: ${handed.skipped.join('; ')}`) : null,
        ),
      );
    }
    return h('section', { 'aria-label': 'Waiting for device' }, parts);
  }

  function activitySection(th) {
    const items = [];
    const calls = new Map();
    let input = 0;
    let output = 0;
    for (const e of events) {
      if (e.kind === 'thread_activity') {
        const a = e.event;
        if (a.type === 'text') items.push(h('div', { class: 'act-text md' }, renderMarkdown(a.text)));
        else if (a.type === 'tool_call') {
          const row = { call: a, result: null, el: null };
          calls.set(a.call_id, row);
          row.el = h('div');
          items.push(row.el);
        } else if (a.type === 'tool_result') {
          const row = calls.get(a.call_id);
          if (row) row.result = a;
          else items.push(toolRow({ call_id: a.call_id, name: a.name, input: null }, a, th));
        } else if (a.type === 'usage') {
          input += a.input_tokens + a.cache_read_tokens + a.cache_write_tokens;
          output += a.output_tokens;
        } else if (a.type === 'notice') items.push(h('div', { class: 'act-notice' }, a.text));
      } else if (e.kind === 'thread_input') {
        items.push(h('div', { class: 'act-input' }, h('span', { class: 'who' }, e.from === 'user' ? 'You' : `From the ${e.from}`), e.text));
      } else if (e.kind === 'approval_resolved') {
        items.push(h('div', { class: 'act-notice' }, e.approved ? 'You approved a tool call.' : 'A tool call was denied.'));
      } else if (e.kind === 'error') {
        items.push(h('div', { class: 'msg-error' }, icon('warning'), e.message));
      }
    }
    // Tool rows were placeholders until their results were known.
    for (const row of calls.values()) {
      const at = items.indexOf(row.el);
      if (at >= 0) items[at] = toolRow(row.call, row.result, th);
    }
    const draft = liveOf(threadId).draft;
    if (draft) {
      const d = h('div', { class: 'act-text md', 'aria-label': 'Thread output so far' }, renderMarkdown(draft));
      (d.lastElementChild || d).append(h('span', { class: 'caret', 'aria-hidden': 'true' }));
      items.push(d);
    }
    if (!items.length) {
      items.push(h('p', { class: 'hint' }, isActive(th.status) ? 'Working… output appears here as it streams.' : 'No output.'));
    }
    return h(
      'section',
      { 'aria-labelledby': 'act-title' },
      h('h3', { class: 'sub-title', id: 'act-title' }, 'Activity', input || output ? h('span', null, `${num(input)} in · ${num(output)} out tokens`) : null),
      h('div', { class: 'activity' }, items),
    );
  }

  function toolRow(call, result, th) {
    const state = result ? (result.ok ? h('span', { class: 'ok-word' }, 'ok') : h('span', { class: 'fail-word' }, 'failed')) : isActive(th.status) ? h('span', { class: 'run-word' }, 'running') : h('span', { class: 'hint' }, 'no result');
    const details = h(
      'details',
      {
        class: 'tool-row',
        open: openCalls.has(call.call_id),
        ontoggle: () => {
          if (details.open) openCalls.add(call.call_id);
          else openCalls.delete(call.call_id);
        },
      },
      h('summary', null, icon('chevron', null), h('span', { class: 'tool-name' }, call.name), state),
      h(
        'div',
        { class: 'tool-body' },
        call.input !== null && call.input !== undefined ? [h('span', { class: 'label' }, 'Input'), h('pre', { class: 'code' }, pretty(call.input))] : null,
        result ? [h('span', { class: 'label' }, 'Output'), h('pre', { class: 'code' }, result.output || '(empty)')] : null,
      ),
    );
    details.querySelector('summary .icon').classList.add('chev');
    return details;
  }

  function resultSection(th) {
    if (isActive(th.status)) return null;
    const finished = events.filter((e) => e.kind === 'thread_finished').at(-1);
    const summary = th.summary || finished?.summary;
    const thumb = (value, iconName, label) =>
      h(
        'button',
        {
          class: 'icon-btn',
          type: 'button',
          'aria-label': label,
          'aria-pressed': rating === value ? 'true' : 'false',
          disabled: !isAdmin(),
          onclick: () => rate(rating === value ? 'clear' : value),
        },
        icon(iconName),
      );
    return h(
      'section',
      { 'aria-labelledby': 'res-title' },
      h('h3', { class: 'sub-title', id: 'res-title' }, th.status === 'done' ? 'Result' : th.status === 'failed' ? 'Failed' : 'Stopped', th.finished ? h('span', null, dateTime(th.finished)) : null),
      summary ? h('div', { class: 'callout md' }, renderMarkdown(summary)) : h('p', { class: 'hint' }, 'No summary.'),
      h('div', { class: 'rating', role: 'group', 'aria-label': 'Rate this result' }, h('span', { class: 'hint' }, 'Rate the result'), thumb('up', 'up', 'Good result'), thumb('down', 'downvote', 'Poor result')),
    );
  }

  async function rate(value) {
    try {
      await post(`/v1/threads/${enc(threadId)}/rating`, { rating: value });
      rating = value === 'clear' ? null : value;
      toast(value === 'clear' ? 'Rating cleared.' : 'Thanks. Ratings feed the model registry.');
      render();
    } catch (err) {
      toast(describe(err), 'error');
    }
  }

  function filesSection() {
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
            icon('download'),
            h('span', { class: 'name' }, f.path),
            h('span', { class: 'size' }, size(f.size)),
          ),
        ),
        files.truncated ? h('p', { class: 'hint' }, 'More files exist than are listed.') : null,
        h('p', { class: 'hint' }, `On ${deviceName(files.device_id)}.`),
      );
    }
    return h('section', { 'aria-labelledby': 'files-title' }, h('h3', { class: 'sub-title', id: 'files-title' }, 'Files'), content);
  }

  async function loadFiles() {
    filesLoading = true;
    render();
    try {
      files = await get(`/v1/threads/${enc(threadId)}/files`);
    } catch (err) {
      files = { error: describe(err) };
    }
    filesLoading = false;
    render();
  }

  function rawSection() {
    const details = h(
      'details',
      {
        class: 'tool-row',
        open: showRaw,
        ontoggle: () => {
          showRaw = details.open;
          if (showRaw) fill();
        },
      },
      h('summary', null, icon('chevron'), h('span', { class: 'tool-name' }, `Show events (${events.length})`)),
    );
    details.querySelector('summary .icon').classList.add('chev');
    const fill = () => {
      if (details.querySelector('.tool-body')) return;
      details.append(h('div', { class: 'tool-body' }, h('pre', { class: 'code' }, events.map((e) => JSON.stringify(e)).join('\n'))));
    };
    if (showRaw) fill();
    return details;
  }

  function renderFoot(th) {
    foot.hidden = false;
    const active = isActive(th.status);
    const canAct = isAdmin() && store.online;
    stepText.disabled = !active || !canAct;
    stepBtn.disabled = !active || !canAct;
    stopBtn.disabled = !active || !canAct;
    stepText.placeholder = !isAdmin() ? 'Sign in with an admin token to message threads' : active ? 'Message this thread' : 'This thread has finished';
    if (!foot.childElementCount) {
      foot.append(stepError, h('div', { class: 'stepin' }, stepText, stepBtn), h('div', null, stopBtn));
    }
    stopBtn.className = active && th.status === 'running' ? 'btn' : 'btn btn-ghost';
    stopBtn.hidden = !active;
  }

  async function sendStepIn() {
    const text = stepText.value.trim();
    if (!text || stepBtn.disabled) return;
    stepBtn.disabled = true;
    stepError.hidden = true;
    try {
      await post(`/v1/threads/${enc(threadId)}/message`, { text });
      stepText.value = '';
      toast('Sent to the thread. The orchestrator is told too.');
    } catch (err) {
      stepError.textContent = describe(err);
      stepError.hidden = false;
    } finally {
      stepBtn.disabled = false;
      render();
    }
  }

  async function stop() {
    const th = t();
    if (th.status === 'running') {
      const ok = await confirmAction({
        title: 'Stop this thread?',
        body: [`“${th.title}” stops on ${deviceName(th.device_id)}. Work it hasn’t saved is lost, and the orchestrator is told you stopped it.`],
        confirmLabel: 'Stop thread',
        danger: true,
      });
      if (!ok) return;
    }
    stopBtn.disabled = true;
    try {
      await post(`/v1/threads/${enc(threadId)}/stop`);
      toast('Thread stopped.');
    } catch (err) {
      stepError.textContent = describe(err);
      stepError.hidden = false;
    } finally {
      render();
    }
  }

  subs.push(
    on('event', (e) => {
      if (e.thread_id !== threadId) return;
      if (e.seq > 0) {
        if (e.seq <= lastSeq) return;
        events.push(e);
        lastSeq = e.seq;
      }
      schedule();
    }),
    on('threads', schedule),
    on('approvals', schedule),
    on('devices', schedule),
    on('online', schedule),
  );

  render();
  load();
  requestAnimationFrame(() => head.querySelector('h2')?.focus?.());

  return () => {
    for (const off of subs) off();
    cancelAnimationFrame(frame);
  };
}

function size(bytes) {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}
