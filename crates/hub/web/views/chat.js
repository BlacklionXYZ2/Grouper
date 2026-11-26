// Chat: one conversation's transcript, live streaming, thread cards and the composer.
// Threads for this chat open in a panel on the chat (a sheet on a phone). The URL
// `#/chat/<id>/threads` keeps that panel open across reloads; Back closes it.
//
// While the chat has pending approvals the approval dock (views/approvals.js)
// takes the composer's place; the draft, its selection and the model/mode
// controls are kept and come back once the last request is resolved.
//
// Stored events render in seq order. AssistantDelta fills a memory-only draft
// until the stored AssistantMessage replaces it. ThreadStarted inserts a card that
// later thread events update in place; ThreadActivity stays off the transcript.

import { h, icon, setChildren, setText, setAttr, srOnly, throttle } from '../lib/dom.js';
import { get, post, qs, enc, describe, isAbort } from '../lib/api.js';
import { renderMarkdown } from '../lib/md.js';
import { store, on, conv, liveOf, deviceName, deviceOnline, threadsIn, approvalsFor, approvalsIn, isAdmin } from '../lib/store.js';
import { loadConversation, loadConversations } from '../lib/live.js';
import { bar, navigate, statusPill, skeleton, toast, announce, sentenceSpeaker, autoGrow, toastError } from '../lib/ui.js';
import { isActive, num, plural, usd } from '../lib/format.js';
import { familyOfId, familyMark, familyLabel } from '../lib/family.js';
import { mountApprovalDock } from './approvals.js';
import { mountThreadsPanel } from './threads-panel.js';
import { openModelPicker } from './model-picker.js';

const MODE_KEY = 'grouper.newMode';

export function renderChat(container, { conversation: cid, thread: openThreadId, onMissing }) {
  const isNew = !cid;
  const subs = [];
  const chipEls = new Map();
  let chipGroup = null;
  let rendered = false;
  let sending = false;
  const speaker = sentenceSpeaker();
  const chipFrames = new Map();
  let openThread = openThreadId;

  const info = () => store.conversations.find((c) => c.id === cid);

  // ---- layout
  // The panel is created once the view exists. The strip and the Threads button
  // close over it; both are only clicked after this function has assigned it.
  let panel = null;
  const barSlot = h('div', { class: 'bar-slot' });
  const stripText = h('span', { class: 'strip-text' });
  const stripStatus = h('span', { class: 'visually-hidden', role: 'status' });
  const strip = h(
    'button',
    {
      class: 'strip',
      type: 'button',
      hidden: true,
      onclick: () => {
        if (!cid) return;
        if (panel?.isOpen()) panel.focus();
        else navigate(`#/chat/${enc(cid)}/threads`);
      },
    },
    h('span', { class: 'visually-hidden' }, 'Open threads, '),
    icon('threads'),
    stripText,
  );
  // Not a live region: the draft changes many times a second. Screen readers hear
  // the reply through the throttled #live-region instead (sentences, then "Reply
  // finished").
  const tail = h('div', { class: 'tail' });
  const transcript = h('div', { class: 'transcript', role: 'region', 'aria-label': 'Conversation' }, tail);
  // The streaming draft: one node, refilled at most every 100 ms.
  const draftEl = h('div', { class: 'msg msg-assistant md', 'aria-label': 'Reply so far' });
  const replyingEl = h('div', { class: 'replying' }, h('span', { class: 'pulse', 'aria-hidden': 'true' }), 'Replying');
  const newMessages = h(
    'button',
    { class: 'new-messages glass', type: 'button', hidden: true, onclick: () => scrollToEnd(true) },
    'New messages',
    icon('down'),
  );
  const composer = buildComposer();
  // "New messages" floats just above the composer, whatever the composer's height.
  composer.wrap.prepend(newMessages);
  // The approval dock sits where the composer is and replaces it while requests wait.
  let composerSel = null; // the draft's selection when the dock took over
  const dock = isNew
    ? null
    : mountApprovalDock({
        conversationId: cid,
        onActiveChange: (docked, { hadFocus }) => {
          if (docked) {
            const typing = composer.form.contains(document.activeElement);
            composerSel = [composer.textarea.selectionStart, composer.textarea.selectionEnd];
            composer.wrap.classList.add('docked');
            composer.form.hidden = composer.note.hidden = true;
            // Focus in the composer would be lost: hand it to the dock's heading (never Approve).
            if (typing) dock.focusHeading();
          } else {
            composer.wrap.classList.remove('docked');
            composer.form.hidden = composer.note.hidden = false;
            if (hadFocus && !composer.textarea.disabled) {
              composer.textarea.focus({ preventScroll: true });
              if (composerSel) composer.textarea.setSelectionRange(...composerSel);
            }
            composerSel = null;
          }
        },
      });
  if (dock) composer.wrap.insertBefore(dock.el, composer.form);
  // The column scrolls. The threads dock sits beside it, so the view itself does not.
  const column = h('div', { class: 'chat-column' }, barSlot, strip, stripStatus, transcript, composer.wrap);
  const view = h('section', { class: 'view chat-view' }, column);
  container.append(view);

  column.addEventListener('scroll', () => {
    if (nearBottom()) newMessages.hidden = true;
  });

  function nearBottom() {
    return column.scrollHeight - column.scrollTop - column.clientHeight < 140;
  }

  function scrollToEnd(smooth = false) {
    newMessages.hidden = true;
    column.scrollTo({ top: column.scrollHeight, behavior: smooth && !matchMedia('(prefers-reduced-motion: reduce)').matches ? 'smooth' : 'auto' });
  }

  function add(node) {
    transcript.insertBefore(node, tail);
  }

  // ---- bar, chat menu, strip

  // On a phone the word hides and the count stays; the button keeps its full name.
  const threadsCount = h('span', { class: 'threads-count' }, '0');
  const threadsLabel = h('span', { class: 'threads-label' }, h('span', { class: 'threads-word' }, 'Threads '), threadsCount);
  const threadsNeed = h('span', { class: 'visually-hidden' });
  const threadsDot = h('span', { class: 'threads-dot', 'aria-hidden': 'true', hidden: true });
  const threadsBtn = isNew
    ? null
    : h(
        'button',
        {
          class: 'btn btn-sm threads-btn',
          type: 'button',
          'aria-expanded': 'false',
          onclick: () => {
            if (panel?.isOpen()) navigate(`#/chat/${enc(cid)}`);
            else navigate(`#/chat/${enc(cid)}/threads`);
          },
        },
        icon('threads'),
        threadsLabel,
        threadsNeed,
        threadsDot,
      );

  const menuCleanup = [];
  const chatMenu = isNew ? null : buildChatMenu();

  function buildChatMenu() {
    let open = false;
    let usageAbort = null;
    const usageLine = h('div', { class: 'menu-usage-val' }, 'Loading…');
    const usageSub = h('div', { class: 'hint' }, '');
    // Outside the menu: a usage total is not a menuitem, and role=none would still
    // expose these divs as menu children (axe aria-required-children).
    const summaryBlock = isAdmin()
      ? h('div', { class: 'menu-summary', tabindex: '0' }, h('div', { class: 'field-label' }, 'Estimated chat usage'), usageLine, usageSub)
      : null;
    const usageItem = isAdmin()
      ? h(
          'a',
          { class: 'menu-item', role: 'menuitem', href: `#/usage${qs({ conversation: cid })}`, onclick: () => setOpen(false) },
          icon('usage'),
          'View chat usage',
        )
      : null;
    const newChatItem = h(
      'a',
      { class: 'menu-item', role: 'menuitem', href: '#/chat/new', onclick: () => setOpen(false) },
      icon('plus'),
      'New chat',
    );
    const menuList = h('div', { class: 'menu-list', role: 'menu', 'aria-label': 'Chat menu' }, usageItem, newChatItem);
    const panel = h(
      'div',
      { class: 'menu-pop', hidden: true },
      summaryBlock,
      summaryBlock ? h('div', { class: 'menu-sep', 'aria-hidden': 'true' }) : null,
      menuList,
    );
    const trigger = h(
      'button',
      {
        class: 'icon-btn',
        type: 'button',
        'aria-label': 'Chat menu',
        title: 'Chat menu',
        'aria-haspopup': 'menu',
        'aria-expanded': 'false',
        onclick: (e) => {
          e.stopPropagation();
          setOpen(!open);
        },
      },
      icon('more'),
    );
    const wrap = h(
      'div',
      {
        class: 'menu-wrap',
        // Tabbing (or otherwise moving focus) out of the menu closes it. A click on
        // nothing focusable has no relatedTarget; the document click handles clicks.
        onfocusout: (e) => {
          if (open && e.relatedTarget && !wrap.contains(e.relatedTarget)) setOpen(false);
        },
      },
      trigger,
      panel,
    );

    async function refreshMenuUsage() {
      if (!isAdmin()) return;
      usageAbort?.abort();
      const ctrl = new AbortController();
      usageAbort = ctrl;
      usageLine.textContent = 'Loading…';
      usageSub.textContent = '';
      try {
        const rows = (await get(`/v1/usage${qs({ conversation: cid, group: 'model' })}`, { signal: ctrl.signal })) || [];
        if (ctrl.signal.aborted) return;
        const total = rows.reduce(
          (acc, r) => ({
            calls: acc.calls + r.calls,
            tokens: acc.tokens + r.input_tokens + r.output_tokens + r.cache_read_tokens + r.cache_write_tokens,
            cost: acc.cost + r.cost,
          }),
          { calls: 0, tokens: 0, cost: 0 },
        );
        if (!total.calls) {
          usageLine.textContent = 'No metered calls yet';
          usageSub.textContent = '';
        } else {
          usageLine.textContent = usd(total.cost);
          usageSub.textContent = `${plural(total.calls, 'call')} · ${num(total.tokens)} tokens`;
        }
      } catch (err) {
        if (isAbort(err)) return;
        usageLine.textContent = 'Unavailable';
        usageSub.textContent = describe(err);
      }
    }

    function menuStops() {
      return [...(summaryBlock ? [summaryBlock] : []), ...menuList.querySelectorAll('[role="menuitem"]:not([hidden])')];
    }

    function setOpen(next, { restoreFocus = false } = {}) {
      if (open === next) return;
      open = next;
      panel.hidden = !open;
      trigger.setAttribute('aria-expanded', open ? 'true' : 'false');
      if (open) {
        refreshMenuUsage();
        // Usage total first, then the actions. The total is not a menuitem.
        menuStops()[0]?.focus();
      } else {
        usageAbort?.abort();
        if (restoreFocus) trigger.focus();
      }
    }

    const onDocClick = (e) => {
      if (open && !wrap.contains(e.target)) setOpen(false);
    };
    // Only while focus is in the menu (or on its button): keys typed elsewhere aren't ours.
    const onKeyDown = (e) => {
      if (!open || !wrap.contains(document.activeElement)) return;
      if (e.key === 'Escape') {
        e.preventDefault();
        e.stopPropagation();
        setOpen(false, { restoreFocus: true });
        return;
      }
      if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
        const items = menuStops();
        if (!items.length) return;
        e.preventDefault();
        const n = items.length;
        const i = items.indexOf(document.activeElement);
        const down = e.key === 'ArrowDown';
        // From the button (not an item): Down goes to the first item, Up to the last.
        const next = i < 0 ? (down ? 0 : n - 1) : (i + (down ? 1 : n - 1)) % n;
        items[next].focus();
      }
    };
    document.addEventListener('click', onDocClick);
    document.addEventListener('keydown', onKeyDown, true);
    menuCleanup.push(() => {
      usageAbort?.abort();
      document.removeEventListener('click', onDocClick);
      document.removeEventListener('keydown', onKeyDown, true);
    });
    return { wrap };
  }

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
        actions: [
          // The sidebar has New chat on wide screens; the bar has it where the sidebar is a drawer.
          h('a', { class: 'icon-btn only-mobile', href: '#/chat/new', 'aria-label': 'New chat', title: 'New chat' }, icon('plus')),
          threadsBtn,
          chatMenu?.wrap || null,
        ],
      }),
    );
    document.title = `${title} · Grouper`;
  }

  function renderStrip() {
    if (isNew || info()?.mode === 'standard') {
      strip.hidden = true;
      setText(stripStatus, '');
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
    const text = parts.join(' · ');
    setText(stripText, text);
    setText(stripStatus, strip.hidden ? '' : text);
  }

  // Visible text is one node ("Threads N") so the accessible name contains it.
  // The "needs you" count is a visually hidden suffix; the dot is decorative.
  function renderThreadsButton() {
    if (!threadsBtn) return;
    const threads = threadsIn(cid);
    const approvals = approvalsIn(cid);
    const claimed = new Set(approvals.map((a) => a.thread_id));
    const need = approvals.length + threads.filter((t) => t.status === 'waiting_device' && !claimed.has(t.id)).length;
    setText(threadsCount, String(threads.length));
    setText(threadsNeed, need === 0 ? '' : need === 1 ? ', 1 needs you' : `, ${need} need you`);
    threadsDot.hidden = need === 0;
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
    for (const e of c.events) appendEvent(e, false);
    renderTail();
    requestAnimationFrame(() => scrollToEnd());
  }

  /** `live`: it just happened (only then does an error interrupt a screen reader). */
  function appendEvent(e, live = true) {
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
        add(h('div', { class: 'msg msg-error', role: live ? 'alert' : null }, icon('warning'), h('div', null, e.message)));
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
    const targetCid = store.threads.get(tid)?.conversation_id || cid;
    navigate(`#/chat/${encodeURIComponent(targetCid)}/thread/${encodeURIComponent(tid)}`);
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
    // No aria-label: it replaced the visible title, status, device and line, so the
    // accessible name no longer contained that text (label-content-name-mismatch).
    entry.el.removeAttribute('aria-label');
    entry.el.removeAttribute('aria-description');
    entry.el.setAttribute('aria-current', openThread === tid ? 'true' : 'false');
    setChildren(
      entry.el,
      h('div', { class: 'chip-head' }, h('span', { class: 'chip-title' }, t.title || s.title), pill),
      h('div', { class: 'chip-meta' }, `${device} · `, familyMark(familyOfId(t.model || s.model)), srOnly(`${familyLabel(familyOfId(t.model || s.model))}: `), t.model || s.model),
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
    scheduleTail.cancel();
    const draft = store.drafts.get(cid);
    const replying = store.replying.has(cid);
    if (draft) {
      draftEl.replaceChildren(renderMarkdown(draft));
      const caret = h('span', { class: 'caret', 'aria-hidden': 'true' });
      const lastBlock = draftEl.lastElementChild;
      (lastBlock && /^(P|LI|H\d)$/.test(lastBlock.tagName) ? lastBlock : draftEl).append(caret);
      if (tail.firstChild !== draftEl) tail.replaceChildren(draftEl);
    } else if (replying) {
      if (tail.firstChild !== replyingEl) tail.replaceChildren(replyingEl);
    } else if (tail.firstChild) {
      tail.replaceChildren();
    }
  }

  // The draft's Markdown is re-rendered at most every 100 ms while it streams.
  const scheduleTail = throttle(() => {
    const stick = nearBottom();
    renderTail();
    if (stick) scrollToEnd();
  }, 100);

  function speakDraft() {
    speaker.speak(store.drafts.get(cid) || '');
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
    // The model: a button that opens the covering picker. A new chat keeps its
    // choice here until the first send; an existing chat changes it on the hub.
    let selected = '';
    const modelMark = h('span', { class: 'model-btn-mark' });
    const modelLabel = h('span', { class: 'model-btn-label' });
    const modelBtn = h(
      'button',
      {
        class: 'model-btn',
        type: 'button',
        'aria-haspopup': 'dialog',
        onclick: () => pickModel(),
      },
      modelMark,
      h('span', { class: 'visually-hidden' }, 'Model: '),
      modelLabel,
      icon('down'),
    );
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

    function role() {
      return (info()?.mode || mode) === 'standard' ? 'standard' : 'orchestrator';
    }

    function currentModel() {
      const value = isNew ? selected : info()?.model || '';
      return store.models.some((x) => x.id === value) ? value : '';
    }

    function renderModels() {
      selected = isNew ? currentModel() : selected;
      const value = currentModel();
      const fam = value ? familyOfId(value) : null;
      const key = `${value}|${fam}`;
      if (modelBtn.dataset.key !== key) {
        modelBtn.dataset.key = key;
        setChildren(modelMark, fam ? familyMark(fam) : null);
        modelMark.hidden = !fam;
        setText(modelLabel, value || 'Default model');
        setAttr(modelBtn, 'title', value ? `${value} (${familyLabel(fam)})` : 'Default model for this chat’s role');
      }
      modelBtn.disabled = !admin || (!isNew && !store.online);
      modelBtn.hidden = !store.models.length;
    }

    async function pickModel() {
      if (modelBtn.disabled) return;
      const before = currentModel();
      const value = await openModelPicker({ role: role(), current: before, opener: modelBtn });
      if (value === undefined || value === before) return;
      if (isNew) {
        selected = value;
        renderModels();
        return;
      }
      try {
        await post(`/v1/conversations/${encodeURIComponent(cid)}/model`, { model: value || null });
        await loadConversations();
        toast(value ? `This conversation now uses ${value}.` : 'This conversation uses the default model again.');
      } catch (err) {
        toastError(err);
      }
      renderModels();
    }

    function renderNote() {
      const m = info()?.mode || mode;
      note.textContent =
        m === 'standard'
          ? 'Standard chat: one model answers. Memory is shared.'
          : 'Project chat: the orchestrator can start threads on your devices or the hub.';
    }

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
      h('div', { class: 'composer-row' }, seg, modelBtn, h('span', { class: 'spacer' }), sendBtn),
    );
    const wrap = h('div', { class: 'composer-wrap' }, form, note);
    renderSeg();
    renderModels();
    renderNote();

    return {
      wrap,
      form,
      note,
      textarea,
      error,
      mode: () => mode,
      model: () => currentModel(),
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
    // Nothing is sent past a pending approval: the dock has to be answered first.
    if (!text || sending || !isAdmin() || dock?.isActive()) return;
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

  if (!isNew) {
    panel = mountThreadsPanel(view, {
      cid,
      trigger: threadsBtn,
      onRequestClose: () => navigate(`#/chat/${enc(cid)}`),
    });
  }

  renderBar();
  renderStrip();
  renderThreadsButton();
  dock?.start();

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
        speaker.reset();
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
      renderThreadsButton();
    }),
    on('devices', () => {
      for (const tid of chipEls.keys()) scheduleChip(tid);
      renderStrip();
    }),
    on('approvals', () => {
      renderThreadsButton();
      for (const tid of chipEls.keys()) scheduleChip(tid);
    }),
    on('conversations', () => {
      renderBar();
      renderStrip();
      composer.renderModels();
      if (!isNew && rendered && !info()) missing();
    }),
    on('models', () => {
      composer.renderModels();
      for (const tid of chipEls.keys()) scheduleChip(tid);
    }),
    on('online', (online) => composer.setOffline(!online)),
    on('route', (r) => {
      if (r.view !== 'chat' || r.conversation !== cid) return;
      openThread = r.thread;
      for (const tid of chipEls.keys()) updateChip(tid);
      const wasOpen = panel?.isOpen() ?? false;
      const want = Boolean(r.threadsPanel) && !r.thread;
      panel?.setOpen(want, { moveFocus: want && !wasOpen, restoreFocus: wasOpen && !r.thread });
      // dialog.close() can hand focus back to the Threads button after the inspector
      // has focused its title. Put the title last when a thread takes over the panel,
      // unless a carried-over step-in draft already has focus in the thread's composer.
      if (wasOpen && r.thread) {
        requestAnimationFrame(() => {
          if (document.activeElement?.closest?.('.thread-foot')) return;
          document.getElementById('thread-title')?.focus({ preventScroll: true });
        });
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
    dock?.destroy();
    panel?.destroy();
    for (const off of subs) off();
    for (const fn of menuCleanup) fn();
    scheduleTail.cancel();
    for (const f of chipFrames.values()) cancelAnimationFrame(f);
  };
}

function firstLine(text) {
  const line = String(text).split('\n').find((l) => l.trim()) || '';
  return line.length > 140 ? `${line.slice(0, 140)}…` : line;
}
