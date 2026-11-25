// Chat: one conversation's transcript, live streaming, thread cards and the composer.
//
// Stored events render in seq order. AssistantDelta fills a memory-only draft
// until the stored AssistantMessage replaces it. ThreadStarted inserts a card that
// later thread events update in place; ThreadActivity stays off the transcript.

import { h, icon, setChildren } from '../lib/dom.js';
import { post, describe } from '../lib/api.js';
import { renderMarkdown } from '../lib/md.js';
import { store, on, conv, liveOf, deviceName, deviceOnline, threadsIn, approvalsFor, pendingApprovals, isAdmin } from '../lib/store.js';
import { loadConversation, loadConversations } from '../lib/live.js';
import { bar, navigate, statusPill, skeleton, toast, announce, autoGrow, toastError } from '../lib/ui.js';
import { isActive, usd } from '../lib/format.js';
import { openApproval } from './approvals.js';

const MODE_KEY = 'grouper.newMode';

export function renderChat(container, { conversation: cid, thread: openThreadId, onMissing }) {
  const isNew = !cid;
  const subs = [];
  const chipEls = new Map();
  let chipGroup = null;
  let rendered = false;
  let sending = false;
  let spokenTo = 0;
  let draftFrame = 0;
  const chipFrames = new Map();
  let openThread = openThreadId;

  const info = () => store.conversations.find((c) => c.id === cid);

  // ---- layout
  const barSlot = h('div', { class: 'bar-slot' });
  const strip = h('div', { class: 'strip', role: 'status', hidden: true });
  const approvalSlot = h('div', { class: 'approval-banner', hidden: true });
  const tail = h('div', { class: 'tail' });
  const transcript = h('div', { class: 'transcript', role: 'log', 'aria-label': 'Conversation' }, tail);
  const newMessages = h(
    'button',
    { class: 'new-messages glass', type: 'button', hidden: true, onclick: () => scrollToEnd(true) },
    'New messages',
    icon('down'),
  );
  const composer = buildComposer();
  const view = h('section', { class: 'view chat-view' }, barSlot, strip, approvalSlot, transcript, newMessages, composer.wrap);
  container.append(view);

  view.addEventListener('scroll', () => {
    if (nearBottom()) newMessages.hidden = true;
  });

  function nearBottom() {
    return view.scrollHeight - view.scrollTop - view.clientHeight < 140;
  }

  function scrollToEnd(smooth = false) {
    newMessages.hidden = true;
    view.scrollTo({ top: view.scrollHeight, behavior: smooth && !matchMedia('(prefers-reduced-motion: reduce)').matches ? 'smooth' : 'auto' });
  }

  function add(node) {
    transcript.insertBefore(node, tail);
  }

  // ---- bar, strip, approval banner

  function renderBar() {
    const c = info();
    const mode = c?.mode || composer.mode();
    const title = isNew ? 'New chat' : c?.title || 'Conversation';
    const caption = isNew ? null : `${mode === 'standard' ? 'Standard' : 'Project'} · ${c?.model || 'Default model'}`;
    setChildren(
      barSlot,
      bar({
        title,
        caption,
        actions: [h('a', { class: 'icon-btn', href: '#/chat/new', 'aria-label': 'New chat', title: 'New chat' }, icon('plus'))],
      }),
    );
    document.title = `${title} · Grouper`;
  }

  function renderStrip() {
    if (isNew || info()?.mode === 'standard') {
      strip.hidden = true;
      return;
    }
    const threads = threadsIn(cid);
    const running = threads.filter((t) => t.status === 'running').length;
    const waiting = threads.filter((t) => t.status === 'waiting_device');
    const offline = [...new Set(threads.filter((t) => isActive(t.status) && !deviceOnline(t.device_id)).map((t) => deviceName(t.device_id)))];
    const parts = [];
    if (running) parts.push(`${running} running`);
    if (waiting.length) parts.push(`${waiting.length} waiting`);
    for (const d of offline) parts.push(`${d} offline`);
    strip.hidden = !parts.length;
    setChildren(strip, icon('threads'), parts.join(' · '));
  }

  function renderApprovalBanner() {
    const all = pendingApprovals();
    if (!all.length) {
      approvalSlot.hidden = true;
      return;
    }
    const here = all.filter((a) => a.conversation_id === cid);
    const first = here[0] || all[0];
    const text =
      all.length === 1
        ? `${first.tool} on ${deviceName(first.device_id)} is waiting for your approval.`
        : `${all.length} tool calls are waiting for your approval.`;
    approvalSlot.hidden = false;
    setChildren(
      approvalSlot,
      h(
        'div',
        { class: 'banner warn', role: 'status' },
        icon('shield'),
        h('div', { class: 'grow' }, text),
        h('button', { class: 'btn btn-sm btn-primary', type: 'button', onclick: () => openApproval(first.approval_id) }, 'Review'),
      ),
    );
  }

  // ---- transcript

  function renderAll() {
    rendered = true;
    transcript.replaceChildren(tail);
    chipEls.clear();
    chipGroup = null;
    const c = conv(cid);
    if (!c.events.length && !sending) {
      add(h('div', { class: 'empty' }, h('p', null, 'No messages here yet.')));
    }
    for (const e of c.events) appendEvent(e);
    renderTail();
    requestAnimationFrame(() => scrollToEnd());
  }

  function appendEvent(e) {
    switch (e.kind) {
      case 'user_message': {
        const pending = [...transcript.querySelectorAll('.msg-user.pending')].find((n) => n.dataset.text === e.text);
        pending?.remove();
        transcript.querySelector('.empty')?.remove();
        add(h('div', { class: 'msg msg-user', dataset: { seq: e.seq } }, e.text));
        chipGroup = null;
        break;
      }
      case 'assistant_message':
        add(h('div', { class: 'msg msg-assistant md', dataset: { seq: e.seq } }, renderMarkdown(e.text)));
        chipGroup = null;
        break;
      case 'error':
        add(h('div', { class: 'msg msg-error', role: 'alert' }, icon('warning'), h('div', null, e.message)));
        chipGroup = null;
        break;
      case 'budget_warning':
        add(h('div', { class: 'msg msg-note' }, icon('warning'), `Estimated ${e.scope} spend ${usd(e.spent)} of ${usd(e.limit)} budget.`));
        break;
      case 'thread_started': {
        if (!chipGroup) {
          chipGroup = h('div', { class: 'chips', role: 'list', 'aria-label': 'Threads' });
          add(chipGroup);
        }
        const chip = h('button', { class: 'chip', type: 'button', onclick: () => openThreadSheet(e.thread_id) });
        chipEls.set(e.thread_id, { el: chip, started: e });
        chipGroup.append(h('div', { role: 'listitem', class: 'chip-item' }, chip));
        updateChip(e.thread_id);
        break;
      }
      default:
        break;
    }
  }

  function openThreadSheet(tid) {
    navigate(`#/chat/${encodeURIComponent(cid)}/thread/${encodeURIComponent(tid)}`);
  }

  function chipLine(tid) {
    const l = liveOf(tid);
    const text = (l.draft || l.lastText || '').trim();
    const lines = text.split('\n').map((s) => s.trim()).filter(Boolean);
    let last = lines[lines.length - 1] || '';
    if (last.length > 160) last = `…${last.slice(-160)}`;
    return { text: last.replace(/[#*`_>]/g, ''), tool: l.tool };
  }

  function updateChip(tid) {
    const entry = chipEls.get(tid);
    if (!entry) return;
    const s = entry.started;
    const t = store.threads.get(tid) || { status: 'running', title: s.title, device_id: s.device_id, model: s.model };
    const approval = approvalsFor(tid).length > 0;
    const { text, tool } = chipLine(tid);
    const device = deviceName(t.device_id || s.device_id);
    const pill = statusPill(t.status, { checkpoint: t.checkpoint, approval });
    const statusWord = pill.textContent;
    entry.el.setAttribute('aria-label', `${t.title || s.title}, ${statusWord}, on ${device}`);
    entry.el.setAttribute('aria-description', text || '');
    entry.el.setAttribute('aria-current', openThread === tid ? 'true' : 'false');
    setChildren(
      entry.el,
      h('div', { class: 'chip-head' }, h('span', { class: 'chip-title' }, t.title || s.title), pill),
      h('div', { class: 'chip-meta' }, `${device} · ${t.model || s.model}`),
      h(
        'div',
        { class: 'chip-line' },
        tool ? [h('span', { class: 'tool' }, tool), text ? ' · ' : ''] : null,
        text || (tool ? '' : t.status === 'running' ? 'Starting…' : t.summary ? firstLine(t.summary) : ''),
      ),
    );
  }

  function scheduleChip(tid) {
    if (chipFrames.has(tid)) return;
    chipFrames.set(
      tid,
      requestAnimationFrame(() => {
        chipFrames.delete(tid);
        updateChip(tid);
      }),
    );
  }

  // The streaming draft and the "replying" line sit after the stored events.
  function renderTail() {
    const draft = store.drafts.get(cid);
    const replying = store.replying.has(cid);
    if (draft) {
      const body = h('div', { class: 'msg msg-assistant md', 'aria-label': 'Reply so far' }, renderMarkdown(draft));
      const caret = h('span', { class: 'caret', 'aria-hidden': 'true' });
      const lastBlock = body.lastElementChild;
      (lastBlock && /^(P|LI|H\d)$/.test(lastBlock.tagName) ? lastBlock : body).append(caret);
      setChildren(tail, body);
    } else if (replying) {
      setChildren(tail, h('div', { class: 'replying' }, h('span', { class: 'pulse', 'aria-hidden': 'true' }), 'Replying'));
    } else {
      tail.replaceChildren();
    }
  }

  function scheduleTail() {
    if (draftFrame) return;
    draftFrame = requestAnimationFrame(() => {
      draftFrame = 0;
      const stick = nearBottom();
      renderTail();
      if (stick) scrollToEnd();
    });
  }

  function speakDraft() {
    const draft = store.drafts.get(cid) || '';
    if (draft.length < spokenTo) spokenTo = 0;
    const rest = draft.slice(spokenTo);
    const m = rest.match(/^[\s\S]*[.!?](\s|$)/);
    if (m) {
      spokenTo += m[0].length;
      announce(m[0].trim());
    }
  }

  // ---- composer

  function buildComposer() {
    const admin = isAdmin();
    let mode = 'project';
    try {
      mode = localStorage.getItem(MODE_KEY) === 'standard' ? 'standard' : 'project';
    } catch {
      /* storage unavailable */
    }
    const textarea = h('textarea', {
      rows: 1,
      placeholder: admin ? 'Message Grouper' : 'Reading only: sign in with an admin token to chat',
      'aria-label': 'Message',
      disabled: !admin,
      onkeydown: (e) => {
        if (e.key === 'Enter' && !e.shiftKey && !e.isComposing) {
          e.preventDefault();
          send();
        }
      },
      oninput: () => updateSend(),
    });
    autoGrow(textarea);
    const sendBtn = h('button', { class: 'send-btn', type: 'submit', 'aria-label': 'Send', disabled: true }, icon('send'));
    const error = h('div', { class: 'composer-error', role: 'alert', hidden: true });
    const seg = h('div', { class: 'seg', role: 'radiogroup', 'aria-label': 'Chat mode' });
    const modelSelect = h('select', { class: 'model-select', 'aria-label': 'Model' });
    const note = h('p', { class: 'composer-note' });

    function renderSeg() {
      const opt = (value, label, title) =>
        h(
          'label',
          { title },
          h('input', {
            type: 'radio',
            name: 'chat-mode',
            value,
            checked: mode === value,
            onchange: () => {
              mode = value;
              try {
                localStorage.setItem(MODE_KEY, mode);
              } catch {
                /* storage unavailable */
              }
              renderModels();
              renderNote();
              textarea.focus();
            },
          }),
          label,
        );
      setChildren(seg, opt('project', 'Project', 'An orchestrator that hands work to threads on your devices'), opt('standard', 'Standard', 'One model, no threads'));
      seg.hidden = !isNew;
    }

    function renderModels() {
      const c = info();
      const m = c?.mode || mode;
      const role = m === 'standard' ? 'standard' : 'orchestrator';
      const models = [...store.models].sort((a, b) => Number(b.roles.includes(role)) - Number(a.roles.includes(role)) || a.id.localeCompare(b.id));
      const current = isNew ? modelSelect.value : c?.model || '';
      setChildren(
        modelSelect,
        h('option', { value: '' }, 'Default model'),
        models.map((x) =>
          h('option', { value: x.id }, `${x.id}${x.free ? ' · free' : ''}${x.local ? ' · local' : ''}${x.roles.includes(role) ? ' · default' : ''}`),
        ),
      );
      modelSelect.value = models.some((x) => x.id === current) ? current : '';
      modelSelect.disabled = !admin || (!isNew && !store.online);
      modelSelect.hidden = !store.models.length;
    }

    function renderNote() {
      const m = info()?.mode || mode;
      note.textContent =
        m === 'standard'
          ? 'Standard chat: one model answers. Memory is shared.'
          : 'Project chat: the orchestrator can start threads on your devices or the hub.';
    }

    modelSelect.addEventListener('change', async () => {
      if (isNew) return;
      const value = modelSelect.value || null;
      try {
        await post(`/v1/conversations/${encodeURIComponent(cid)}/model`, { model: value });
        await loadConversations();
        toast(value ? `This conversation now uses ${value}.` : 'This conversation uses the default model again.');
      } catch (err) {
        toastError(err);
        renderModels();
      }
    });

    function updateSend() {
      sendBtn.disabled = !admin || !textarea.value.trim() || sending || (!store.online && store.everOnline);
    }

    const form = h(
      'form',
      {
        class: 'composer glass',
        onsubmit: (e) => {
          e.preventDefault();
          send();
        },
      },
      textarea,
      error,
      h('div', { class: 'composer-row' }, seg, modelSelect, h('span', { class: 'spacer' }), sendBtn),
    );
    const wrap = h('div', { class: 'composer-wrap' }, form, note);
    renderSeg();
    renderModels();
    renderNote();

    return {
      wrap,
      textarea,
      error,
      mode: () => mode,
      model: () => modelSelect.value,
      updateSend,
      renderModels,
      setOffline(offline) {
        textarea.disabled = !admin || offline;
        updateSend();
        renderModels();
      },
    };
  }

  async function send() {
    const text = composer.textarea.value.trim();
    if (!text || sending || !isAdmin()) return;
    sending = true;
    composer.error.hidden = true;
    composer.textarea.value = '';
    composer.updateSend();
    const pending = h('div', { class: 'msg msg-user pending', dataset: { text } }, text);
    if (isNew) {
      transcript.querySelector('.welcome')?.remove();
      composer.wrap.querySelector('.seg').hidden = true;
    }
    transcript.querySelector('.empty')?.remove();
    add(pending);
    scrollToEnd();
    try {
      if (isNew) {
        const body = { mode: composer.mode(), text };
        if (composer.model()) body.model = composer.model();
        const r = await post('/v1/chat', body);
        await loadConversations().catch(() => {});
        navigate(`#/chat/${encodeURIComponent(r.conversation_id)}`);
      } else {
        await post('/v1/chat', { conversation_id: cid, text });
        store.replying.add(cid);
        renderTail();
      }
    } catch (err) {
      pending.remove();
      composer.textarea.value = text;
      composer.error.textContent = describe(err);
      composer.error.hidden = false;
      if (isNew) {
        composer.wrap.querySelector('.seg').hidden = false;
        renderWelcome();
      }
    } finally {
      sending = false;
      composer.updateSend();
      composer.textarea.focus();
    }
  }

  function renderWelcome() {
    if (transcript.querySelector('.welcome')) return;
    add(
      h(
        'div',
        { class: 'welcome' },
        h('div', { class: 'boot-mark', 'aria-hidden': 'true' }),
        h('h2', null, 'What should we work on?'),
        h(
          'p',
          null,
          'A project chat can hand work to your Mac, your PC or the hub, and you can watch and steer each thread. Standard chat is one model, no threads.',
        ),
      ),
    );
  }

  // ---- wiring

  renderBar();
  renderStrip();
  renderApprovalBanner();

  if (isNew) {
    renderWelcome();
    requestAnimationFrame(() => composer.textarea.focus());
  } else {
    const c = conv(cid);
    if (c.loaded) {
      renderAll();
    } else {
      transcript.insertBefore(h('div', { 'aria-busy': 'true', 'aria-label': 'Loading conversation' }, skeleton(3), skeleton(2)), tail);
    }
    loadConversation(cid).catch((err) => {
      if (!rendered) setChildren(transcript, h('div', { class: 'banner error', role: 'alert' }, icon('warning'), describe(err)), tail);
    });
  }

  subs.push(
    on('loaded', (id) => {
      if (id === cid) {
        renderAll();
        if (store.conversations.length && !info()) missing();
      }
    }),
    on('stored', (e) => {
      if (e.conversation_id !== cid || !rendered) return;
      const stick = nearBottom();
      appendEvent(e);
      if (e.kind === 'assistant_message') {
        spokenTo = 0;
        announce('Reply finished', { force: true });
      }
      renderTail();
      if (stick || e.kind === 'user_message') scrollToEnd();
      else if (['assistant_message', 'thread_started', 'error'].includes(e.kind)) newMessages.hidden = false;
    }),
    on('event', (e) => {
      if (e.conversation_id !== cid) return;
      if (e.kind === 'assistant_delta') {
        scheduleTail();
        speakDraft();
      } else if (e.kind === 'turn_finished' || e.kind === 'assistant_message' || e.kind === 'error') {
        scheduleTail();
      }
      if (e.thread_id) scheduleChip(e.thread_id);
      if (e.kind.startsWith('thread_') || e.kind.startsWith('approval_')) renderStrip();
    }),
    on('replying', (id) => {
      if (id === cid) scheduleTail();
    }),
    on('threads', () => {
      for (const tid of chipEls.keys()) scheduleChip(tid);
      renderStrip();
    }),
    on('devices', () => {
      for (const tid of chipEls.keys()) scheduleChip(tid);
      renderStrip();
    }),
    on('approvals', () => {
      renderApprovalBanner();
      for (const tid of chipEls.keys()) scheduleChip(tid);
    }),
    on('conversations', () => {
      renderBar();
      composer.renderModels();
      if (!isNew && rendered && !info()) missing();
    }),
    on('models', () => composer.renderModels()),
    on('online', (online) => composer.setOffline(!online)),
    on('route', (r) => {
      if (r.view === 'chat' && r.conversation === cid) {
        openThread = r.thread;
        for (const tid of chipEls.keys()) updateChip(tid);
      }
    }),
  );
  composer.setOffline(!store.online && store.everOnline);

  function missing() {
    onMissing?.();
    setChildren(
      transcript,
      h('div', { class: 'empty' }, h('h2', null, 'Conversation not found'), h('p', null, "It isn't on this hub. Start a new chat instead."), h('p', null, h('a', { href: '#/chat/new' }, 'New chat'))),
      tail,
    );
  }

  return () => {
    for (const off of subs) off();
    cancelAnimationFrame(draftFrame);
    for (const f of chipFrames.values()) cancelAnimationFrame(f);
  };
}

function firstLine(text) {
  const line = String(text).split('\n').find((l) => l.trim()) || '';
  return line.length > 140 ? `${line.slice(0, 140)}…` : line;
}
