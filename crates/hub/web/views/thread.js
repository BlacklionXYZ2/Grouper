// The thread sheet (inspector on wide screens, a sheet on narrow ones): live
// activity, waiting and checkpoint state, step-in, Stop, files and rating.
//
// The sheet is built once. Stored activity is appended as it arrives (a tool row is
// filled in place when its result lands); deltas only refill the streaming-draft
// node, at most every 100 ms. The small sections (approvals, waiting, result,
// files) are redrawn only when their own inputs change.

import { h, icon, setChildren, throttle, setText } from '../lib/dom.js';
import { get, post, qs, enc, describe } from '../lib/api.js';
import { renderMarkdown } from '../lib/md.js';
import { store, on, liveOf, deviceName, approvalsFor, isAdmin } from '../lib/store.js';
import { statusPill, skeleton, confirmAction, toast, autoGrow, errorBanner, announce, sentenceSpeaker } from '../lib/ui.js';
import { ago, dateTime, num, pretty, isActive } from '../lib/format.js';
import { openApproval } from './approvals.js';

const drafts = new Map();

/** Put text in a thread's step-in field (used by the approval sheet's "Change"). */
export function setStepInDraft(threadId, text) {
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

export function renderThread(container, { threadId, conversationId, onClose }) {
  const subs = [];
  const aborter = new AbortController();
  let info = store.threads.get(threadId) || null;
  let cid = conversationId || info?.conversation_id;
  let events = [];
  let lastSeq = 0;
  let loaded = false;
  let loadError = null;
  let rating = null;
  let files = null; // null: not loaded; {error} or listing
  let filesLoading = false;
  let tokensIn = 0;
  let tokensOut = 0;
  let lastStatus = null;
  const calls = new Map(); // call_id -> {details, state, body, call, result}
  const openCalls = new Set();
  const speaker = sentenceSpeaker();

  // ---- skeleton of the sheet (built once)
  const titleEl = h('h2', { id: 'thread-title', tabindex: '-1' });
  const metaEl = h('div', { class: 'meta' });
  const reasonEl = h('p', { class: 'hint', hidden: true });
  const head = h(
    'header',
    { class: 'sheet-head' },
    h('div', { class: 'sheet-head-row' }, titleEl, h('button', { class: 'icon-btn', type: 'button', 'aria-label': 'Close thread', onclick: onClose }, icon('close'))),
    metaEl,
    reasonEl,
  );
  const statusSlot = h('div', { class: 'sheet-status' }); // loading and errors
  const approvalsSlot = h('div', { hidden: true });
  const waitingSlot = h('section', { 'aria-label': 'Waiting for device', hidden: true });
  const tokensEl = h('span');
  const actList = h('div', { class: 'activity' });
  const draftEl = h('div', { class: 'act-text md', 'aria-label': 'Thread output so far', hidden: true });
  const emptyEl = h('p', { class: 'hint' });
  actList.append(draftEl, emptyEl);
  const activity = h('section', { 'aria-labelledby': 'act-title', hidden: true }, h('h3', { class: 'sub-title', id: 'act-title' }, 'Activity', tokensEl), actList);
  const resultSlot = h('section', { 'aria-labelledby': 'res-title', hidden: true });
  const filesSlot = h('section', { 'aria-labelledby': 'files-title', hidden: true });
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
  const body = h('div', { class: 'sheet-body' }, statusSlot, approvalsSlot, waitingSlot, activity, resultSlot, filesSlot, raw);
  const foot = h('div', { class: 'sheet-foot', hidden: true });
  container.append(head, body, foot);
  container.setAttribute('aria-labelledby', 'thread-title');

  // Step-in field, built once so typing survives updates.
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
  const stepRow = h('div', { class: 'stepin' }, stepText, stepBtn);
  const stopRow = h('div', null, stopBtn);
  const footNote = h('p', { class: 'hint foot-note' });
  foot.append(stepError, stepRow, stopRow, footNote);

  function t() {
    return store.threads.get(threadId) || info || {};
  }

  async function load() {
    const { signal } = aborter;
    try {
      // The thread's own record says which conversation holds its events; read it
      // first only when the route didn't say.
      let thread;
      let evs;
      if (cid) {
        [thread, evs] = await Promise.all([get(`/v1/threads/${enc(threadId)}`, { signal }), get(`/v1/conversations/${enc(cid)}/events${qs({ thread: threadId })}`, { signal })]);
      } else {
        thread = await get(`/v1/threads/${enc(threadId)}`, { signal });
        evs = await get(`/v1/conversations/${enc(thread.conversation_id)}/events${qs({ thread: threadId })}`, { signal });
      }
      info = thread;
      const known = store.threads.get(threadId);
      store.threads.set(threadId, { ...known, ...thread, checkpoint: known?.checkpoint });
      cid = thread.conversation_id;
      // Keep events that arrived on the socket while loading.
      const fetched = evs || [];
      const extra = events.filter((e) => e.seq > (fetched.at(-1)?.seq || 0));
      events = [...fetched, ...extra];
      lastSeq = events.at(-1)?.seq || 0;
      loaded = true;
      loadError = null;
      rebuildActivity();
    } catch (err) {
      if (signal.aborted) return;
      loadError = err;
    }
    renderAll();
  }

  // ---- whole-sheet refresh (cheap parts only; never the activity list)

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
    for (const el of [activity, filesSlot, raw]) el.hidden = !ready;
    foot.hidden = !ready;
    if (!ready) {
      approvalsSlot.hidden = waitingSlot.hidden = resultSlot.hidden = true;
      return;
    }
    renderApprovals();
    renderWaiting(th);
    renderResult(th);
    renderFiles();
    renderRawCount();
    renderFoot(th);
    refreshToolStates(th);
    renderEmpty(th);
    lastStatus = th.status;
  }

  function renderHead(th) {
    const approval = approvalsFor(threadId).length > 0;
    const started = events.find((e) => e.kind === 'thread_started');
    const reason = th.reason || started?.reason;
    setText(titleEl, th.title || started?.title || 'Thread');
    setChildren(
      metaEl,
      statusPill(th.status, { checkpoint: th.checkpoint, approval }),
      h('span', null, deviceName(th.device_id)),
      h('span', { class: 'mono' }, th.model || ''),
      th.created ? h('span', null, `started ${ago(th.created)}`) : null,
    );
    setText(reasonEl, reason || '');
    reasonEl.hidden = !reason;
  }

  function renderApprovals() {
    const list = approvalsFor(threadId);
    approvalsSlot.hidden = !list.length;
    setChildren(
      approvalsSlot,
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
          { class: 'callout warn' },
          h('h3', null, icon('offline'), `${deviceName(th.device_id)} is offline`),
          h('div', null, device?.last_seen ? `Last seen ${ago(device.last_seen)}.` : 'Not seen yet.'),
          waiting?.reason ? h('div', { class: 'hint' }, waiting.reason) : null,
          !checkpoint ? h('div', { class: 'hint' }, 'Meanwhile the hub works towards a checkpoint, then waits for the device.') : null,
        ),
      );
    }
    if (checkpoint) {
      parts.push(h('div', { class: 'callout' }, h('h3', null, icon('flag'), 'Checkpoint on the hub'), h('div', { class: 'md' }, renderMarkdown(checkpoint.summary))));
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
    waitingSlot.hidden = false;
    setChildren(waitingSlot, parts);
  }

  // ---- activity (appended once, never rebuilt while streaming)

  function rebuildActivity() {
    for (const node of [...actList.children]) if (node !== draftEl && node !== emptyEl) node.remove();
    calls.clear();
    tokensIn = tokensOut = 0;
    for (const e of events) appendActivity(e);
    renderTokens();
    renderDraft();
  }

  function addItem(node) {
    actList.insertBefore(node, draftEl);
  }

  /** Fold one stored event into the activity list. */
  function appendActivity(e) {
    if (e.kind === 'thread_activity') {
      const a = e.event;
      if (a.type === 'text') addItem(h('div', { class: 'act-text md' }, renderMarkdown(a.text)));
      else if (a.type === 'tool_call') {
        const row = toolRow(a);
        calls.set(a.call_id, row);
        addItem(row.details);
        updateToolRow(row);
      } else if (a.type === 'tool_result') {
        let row = calls.get(a.call_id);
        if (!row) {
          row = toolRow({ call_id: a.call_id, name: a.name, input: null });
          calls.set(a.call_id, row);
          addItem(row.details);
        }
        row.result = a;
        updateToolRow(row);
      } else if (a.type === 'usage') {
        tokensIn += a.input_tokens + a.cache_read_tokens + a.cache_write_tokens;
        tokensOut += a.output_tokens;
      } else if (a.type === 'notice') addItem(h('div', { class: 'act-notice' }, a.text));
    } else if (e.kind === 'thread_input') {
      addItem(h('div', { class: 'act-input' }, h('span', { class: 'who' }, e.from === 'user' ? 'You' : `From the ${e.from}`), e.text));
    } else if (e.kind === 'approval_resolved') {
      addItem(h('div', { class: 'act-notice' }, e.approved ? 'You approved a tool call.' : 'A tool call was denied.'));
    } else if (e.kind === 'error') {
      addItem(h('div', { class: 'msg-error' }, icon('warning'), e.message));
    }
  }

  function toolRow(call) {
    const state = h('span');
    const bodyEl = h('div', { class: 'tool-body' });
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
      h('summary', null, chev(), h('span', { class: 'tool-name' }, call.name), state),
      bodyEl,
    );
    return { details, state, body: bodyEl, call, result: null };
  }

  function updateToolRow(row) {
    const { call, result } = row;
    setChildren(
      row.body,
      call.input !== null && call.input !== undefined ? [h('span', { class: 'label' }, 'Input'), h('pre', { class: 'code' }, pretty(call.input))] : null,
      result ? [h('span', { class: 'label' }, 'Output'), h('pre', { class: 'code' }, result.output || '(empty)')] : null,
    );
    setToolState(row, t());
  }

  function setToolState(row, th) {
    const [cls, word] = row.result
      ? row.result.ok
        ? ['ok-word', 'ok']
        : ['fail-word', 'failed']
      : isActive(th.status)
        ? ['run-word', 'running']
        : ['hint', 'no result'];
    row.state.className = cls;
    setText(row.state, word);
  }

  function refreshToolStates(th) {
    if (th.status === lastStatus) return;
    for (const row of calls.values()) if (!row.result) setToolState(row, th);
  }

  function renderTokens() {
    setText(tokensEl, tokensIn || tokensOut ? `${num(tokensIn)} in · ${num(tokensOut)} out tokens` : '');
  }

  function renderEmpty(th) {
    const has = actList.childElementCount > 2 || !draftEl.hidden;
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
      (draftEl.lastElementChild || draftEl).append(h('span', { class: 'caret', 'aria-hidden': 'true' }));
      draftEl.hidden = false;
      if (atEnd) body.scrollTop = body.scrollHeight;
    }
    renderEmpty(t());
  }

  const scheduleDraft = throttle(renderDraft, 100);

  function nearEnd() {
    return body.scrollHeight - body.scrollTop - body.clientHeight < 60;
  }

  // ---- result, rating, files, raw events

  function renderResult(th) {
    if (isActive(th.status)) {
      resultSlot.hidden = true;
      resultSlot.replaceChildren();
      return;
    }
    const finished = events.filter((e) => e.kind === 'thread_finished').at(-1);
    const summary = th.summary || finished?.summary;
    const key = `${th.status}|${th.finished}|${summary}|${isAdmin()}`;
    if (resultSlot.dataset.key === key) return; // keep focus on the thumbs
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
    resultSlot.hidden = false;
    setChildren(
      resultSlot,
      h('h3', { class: 'sub-title', id: 'res-title' }, th.status === 'done' ? 'Result' : th.status === 'failed' ? 'Failed' : 'Stopped', th.finished ? h('span', null, dateTime(th.finished)) : null),
      summary ? h('div', { class: 'callout md' }, renderMarkdown(summary)) : h('p', { class: 'hint' }, 'No summary.'),
      h('div', { class: 'rating', role: 'group', 'aria-label': 'Rate this result' }, h('span', { class: 'hint' }, 'Rate the result'), thumb('up', 'up', 'Good result'), thumb('down', 'downvote', 'Poor result')),
    );
  }

  async function rate(value) {
    try {
      await post(`/v1/threads/${enc(threadId)}/rating`, { rating: value });
      rating = value === 'clear' ? null : value;
      for (const b of resultSlot.querySelectorAll('.thumb')) b.setAttribute('aria-pressed', b.dataset.value === rating ? 'true' : 'false');
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
            icon('download'),
            h('span', { class: 'name' }, f.path),
            h('span', { class: 'size' }, size(f.size)),
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
    setChildren(filesSlot, h('h3', { class: 'sub-title', id: 'files-title' }, 'Files'), content);
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

  function renderFoot(th) {
    const active = isActive(th.status);
    const canAct = isAdmin() && store.online;
    stepRow.hidden = !active;
    stopRow.hidden = !active;
    stepText.disabled = !canAct;
    stepBtn.disabled = !canAct;
    stopBtn.disabled = !canAct;
    stepText.placeholder = !isAdmin() ? 'Sign in with an admin token to message threads' : 'Message this thread';
    stopBtn.className = th.status === 'running' ? 'btn' : 'btn btn-ghost';
    let note = '';
    if (!active) note = th.status === 'done' ? 'This thread has finished. Ask in the chat to follow up.' : `This thread has ${th.status === 'failed' ? 'failed' : 'stopped'}. Ask in the chat to try again.`;
    else if (!isAdmin()) note = 'Reading only: sign in with an admin token to message or stop threads.';
    else if (!store.online) note = 'Offline. You can message or stop the thread once the hub reconnects.';
    setText(footNote, note);
    footNote.hidden = !note;
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
      if (e.seq > 0) {
        if (e.seq <= lastSeq) return;
        events.push(e);
        lastSeq = e.seq;
        if (!loaded) return; // the load keeps it and draws everything
        const atEnd = nearEnd();
        appendActivity(e);
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
        scheduleDraft();
        speaker.speak(liveOf(threadId).draft);
      }
    }),
    on('threads', scheduleAll),
    on('approvals', scheduleAll),
    on('devices', scheduleAll),
    on('online', scheduleAll),
  );

  renderAll();
  load();
  requestAnimationFrame(() => titleEl.focus());

  return () => {
    for (const off of subs) off();
    aborter.abort();
    scheduleAll.cancel();
    scheduleDraft.cancel();
  };
}

function chev() {
  const c = icon('chevron');
  c.classList.add('chev');
  return c;
}

function size(bytes) {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}
