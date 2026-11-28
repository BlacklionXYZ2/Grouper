// Chat: one conversation's transcript, live streaming, grouped threads cards and
// the composer. Threads for this chat open in a panel on the chat (a sheet on a
// phone). The URL `#/chat/<id>/threads` keeps that panel open across reloads;
// Back closes it.
//
// While the chat has pending approvals the approval dock (views/approvals.js)
// takes the composer's place; the draft, its selection and the model control
// are kept and come back once the last request is resolved.
//
// Stored events render in seq order. AssistantDelta fills a memory-only draft
// until the stored AssistantMessage replaces it. Consecutive ThreadStarted
// events share one grouped threads card whose rows and header update in place;
// ThreadActivity stays off the transcript.

import { h, icon, setChildren, setText, setAttr, srOnly, throttle } from '../lib/dom.js';
import { get, post, qs, enc, describe, isAbort } from '../lib/api.js';
import { renderMarkdown } from '../lib/md.js';
import { store, on, conv, deviceName, deviceOnline, threadsIn, approvalsFor, approvalsIn, isAdmin } from '../lib/store.js';
import { loadConversation, loadConversations } from '../lib/live.js';
import { bar, navigate, statusPill, skeleton, toast, announce, sentenceSpeaker, autoGrow, toastError } from '../lib/ui.js';
import { isActive, num, plural, usd, time, elapsed } from '../lib/format.js';
import { familyOfId, familyMark, familyDot, familyLabel, setFamilyClass } from '../lib/family.js';
import { mountApprovalDock } from './approvals.js';
import { mountThreadsPanel } from './threads-panel.js';
import { openModelPicker } from './model-picker.js';

const MODE_KEY = 'grouper.newMode';
const PHONE_MQ = '(max-width: 760px)';

function ownerInitial() {
  const name = String(store.me?.name || '').trim();
  const ch = name.match(/[\p{L}\p{N}]/u)?.[0] || 'Y';
  return ch.toUpperCase();
}

function timeEl(ts) {
  if (typeof ts !== 'number' || ts <= 0) return null;
  const iso = new Date(ts * 1000).toISOString();
  return h('time', { class: 'msg-time', datetime: iso }, time(ts));
}

function thinkingChip() {
  return h(
    'span',
    { class: 'msg-state', role: 'status' },
    h('span', { class: 'thinking-dots', 'aria-hidden': 'true' }, h('span'), h('span'), h('span')),
    'Thinking…',
  );
}

export function renderChat(container, { conversation: cid, thread: openThreadId, onMissing }) {
  const isNew = !cid;
  const subs = [];
  const threadRows = new Map(); // thread_id -> { group, row, dotSlot, titleSpan, planEl, deviceSr, modelSr, modelText, pillSlot, started }
  const threadPlans = new Map(); // thread_id -> { done, total } from latest update_plan tool_call
  const threadGroups = new Set(); // { el, headText, list, ids }
  let activeGroup = null;
  let activeRun = null; // the run of consecutive activity lines (design system 3.2)
  let lastSpeaker = null; // 'user' | 'assistant' | null
  let rendered = false;
  let sending = false;
  const speaker = sentenceSpeaker();
  const threadFrames = new Map();
  let openThread = openThreadId;
  const phoneMq = typeof matchMedia === 'function' ? matchMedia(PHONE_MQ) : null;

  const info = () => store.conversations.find((c) => c.id === cid);

  // ---- layout
  // The panel and menu are created once the view exists. Callbacks close over
  // them; declare them here so buildComposer() never hits a TDZ reference.
  let panel = null;
  let chatMenu = null;
  let titleBtnEl = null;
  let titleTextEl = null;
  const barSlot = h('div', { class: 'bar-slot' });
  const stripText = h('span', { class: 'strip-text' });
  const stripStatus = h('span', { class: 'visually-hidden', role: 'status' });
  const strip = h(
    'button',
    {
      class: 'strip',
      type: 'button',
      hidden: true,
      onclick: () => openThreadsList(),
    },
    h('span', { class: 'visually-hidden' }, 'Open threads, '),
    icon('thread-tree'),
    stripText,
    icon('chevron'),
  );

  function openThreadsList() {
    if (!cid) return;
    if (panel?.isOpen()) panel.focus();
    else navigate(`#/chat/${enc(cid)}/threads`);
  }

  // Not a live region: the draft changes many times a second. Screen readers hear
  // the reply through the throttled #live-region instead (sentences, then "Reply
  // finished").
  const tail = h('div', { class: 'tail' });
  const transcript = h('div', { class: 'transcript chat-transcript', role: 'region', 'aria-label': 'Conversation' }, tail);

  // Live streaming / thinking row: one persistent node, refilled at most every 100 ms.
  const tailAvatar = h('span', { class: 'msg-avatar msg-avatar-assistant', 'aria-hidden': 'true' });
  const tailAuthor = h('span', { class: 'msg-author' }, 'Grouper');
  const tailState = thinkingChip();
  const tailHead = h('div', { class: 'msg-head' }, tailAuthor, tailState);
  const tailSr = srOnly('Grouper: ');
  const tailBody = h('div', { class: 'msg-body md', 'aria-label': 'Reply so far' });
  const tailRow = h('div', { class: 'msg msg-row msg-assistant msg-live' }, tailAvatar, h('div', { class: 'msg-content' }, tailHead, tailSr, tailBody));

  const newMessages = h(
    'button',
    { class: 'new-messages', type: 'button', hidden: true, onclick: () => scrollToEnd(true) },
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
            composer.form.hidden = true;
            // Focus in the composer would be lost: hand it to the dock's heading (never Approve).
            if (typing) dock.focusHeading();
          } else {
            composer.wrap.classList.remove('docked');
            composer.form.hidden = false;
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

  // Icon with count (the word stays for screen readers so the accessible name is "Threads N").
  const threadsCount = h('span', { class: 'threads-count' }, '0');
  const threadsLabel = h('span', { class: 'threads-label' }, h('span', { class: 'threads-word visually-hidden' }, 'Threads '), threadsCount);
  const threadsNeed = h('span', { class: 'visually-hidden' });
  const threadsDot = h('span', { class: 'threads-dot', 'aria-hidden': 'true', hidden: true });
  const threadsBtn = isNew
    ? null
    : h(
        'button',
        {
          class: 'btn btn-sm btn-ghost threads-btn',
          type: 'button',
          'aria-expanded': 'false',
          title: 'Threads',
          onclick: () => {
            if (panel?.isOpen()) navigate(`#/chat/${enc(cid)}`);
            else navigate(`#/chat/${enc(cid)}/threads`);
          },
        },
        icon('thread-tree'),
        threadsLabel,
        threadsNeed,
        threadsDot,
      );

  const menuCleanup = [];
  chatMenu = isNew ? null : buildChatMenu();

  function buildChatMenu() {
    let open = false;
    let activeTrigger = null;
    let usageAbort = null;
    const usageLine = h('div', { class: 'menu-usage-val' }, 'Loading…');
    const usageSub = h('div', { class: 'hint' }, '');
    // Outside the menu: a usage total is not a menuitem, and role=none would still
    // expose these divs as menu children (axe aria-required-children).
    const summaryBlock = isAdmin()
      ? h('div', { class: 'menu-summary', tabindex: '0' }, h('div', { class: 'field-label' }, 'Estimated chat usage'), usageLine, usageSub)
      : null;
    const modelItemValue = h('span', { class: 'menu-item-meta' });
    const modelItem = h(
      'button',
      {
        class: 'menu-item',
        type: 'button',
        role: 'menuitem',
        onclick: () => {
          const opener = activeTrigger || trigger;
          setOpen(false);
          composer.pickModel(opener);
        },
      },
      icon('models'),
      h('span', { class: 'grow' }, 'Model'),
      modelItemValue,
    );
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
      { class: 'menu-item chat-menu-new', role: 'menuitem', href: '#/chat/new', onclick: () => setOpen(false) },
      icon('plus'),
      'New chat',
    );
    const menuList = h('div', { class: 'menu-list', role: 'menu', 'aria-label': 'Chat menu' }, modelItem, usageItem, newChatItem);
    const menuPanel = h(
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
          toggleFrom(trigger, false);
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
          if (open && e.relatedTarget && !wrap.contains(e.relatedTarget) && !activeTrigger?.contains(e.relatedTarget)) setOpen(false);
        },
      },
      trigger,
      menuPanel,
    );

    function syncModelItem() {
      const m = composer.model();
      setText(modelItemValue, m || 'Default');
      modelItem.hidden = !store.models.length || !isAdmin() || !store.online;
    }

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

    function syncExpanded() {
      trigger.setAttribute('aria-expanded', open && activeTrigger === trigger ? 'true' : 'false');
      if (titleBtnEl) titleBtnEl.setAttribute('aria-expanded', open && activeTrigger === titleBtnEl ? 'true' : 'false');
    }

    function setOpen(next, { restoreFocus = false, opener = trigger, alignLeft = false } = {}) {
      if (open === next && (!next || activeTrigger === opener)) return;
      const prevTrigger = activeTrigger;
      open = next;
      if (open) {
        activeTrigger = opener;
        menuPanel.classList.toggle('from-title', alignLeft);
        syncModelItem();
      }
      menuPanel.hidden = !open;
      syncExpanded();
      if (open) {
        refreshMenuUsage();
        // Usage total first, then the actions. The total is not a menuitem.
        menuStops()[0]?.focus();
      } else {
        usageAbort?.abort();
        const target = prevTrigger || trigger;
        activeTrigger = null;
        syncExpanded();
        if (restoreFocus && target?.isConnected) target.focus();
      }
    }

    function toggleFrom(opener, alignLeft = false) {
      if (open && activeTrigger === opener) setOpen(false, { restoreFocus: true });
      else setOpen(true, { opener, alignLeft });
    }

    const onDocClick = (e) => {
      if (open && !wrap.contains(e.target) && !activeTrigger?.contains(e.target)) setOpen(false);
    };
    // Only while focus is in the menu (or on its trigger): keys typed elsewhere aren't ours.
    const onKeyDown = (e) => {
      if (!open) return;
      const inside = wrap.contains(document.activeElement) || activeTrigger?.contains(document.activeElement);
      if (!inside) return;
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
        // From the trigger (not an item): Down goes to the first item, Up to the last.
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
    return { wrap, toggleFrom, syncModelItem, isOpen: () => open };
  }

  // Build the bar once and update the title text in place so focus, menu state
  // and the router's h1 tabindex survive background conversation updates.

  function buildBar() {
    let titleNode;
    if (isNew) {
      titleTextEl = null;
      titleNode = h(
        'span',
        { class: 'chat-title-static' },
        h('span', { class: 'only-desktop' }, 'New chat'),
        h('span', { class: 'only-mobile' }, 'Grouper'),
      );
    } else {
      titleTextEl = h('span', { class: 'chat-title-text' }, 'Conversation');
      titleBtnEl = h(
        'button',
        {
          class: 'chat-title-btn',
          type: 'button',
          'aria-haspopup': 'menu',
          'aria-expanded': 'false',
          onclick: (e) => {
            e.stopPropagation();
            chatMenu?.toggleFrom(titleBtnEl, true);
          },
        },
        titleTextEl,
        icon('down'),
      );
      titleNode = titleBtnEl;
    }
    setChildren(
      barSlot,
      bar({
        title: titleNode,
        actions: [
          threadsBtn,
          chatMenu?.wrap || null,
        ],
      }),
    );
  }

  function renderBar() {
    const c = info();
    const title = isNew ? 'New chat' : c?.title || 'Conversation';
    if (titleTextEl) {
      setText(titleTextEl, title);
      const modeLabel = (c?.mode || composer.mode()) === 'standard' ? 'Standard' : 'Project';
      setAttr(titleBtnEl, 'title', `${title} · ${modeLabel} · ${c?.model || 'Default model'}`);
    }
    chatMenu?.syncModelItem();
    document.title = `${title} · Grouper`;
  }

  function renderStrip() {
    if (isNew || info()?.mode === 'standard') {
      strip.hidden = true;
      setText(stripStatus, '');
      return;
    }
    const threads = threadsIn(cid);
    const waiting = threads.filter((t) => t.status === 'waiting_device');
    const offline = [...new Set(threads.filter((t) => isActive(t.status) && !deviceOnline(t.device_id)).map((t) => deviceName(t.device_id)))];
    const parts = [];
    if (waiting.length) parts.push(`${waiting.length} waiting`);
    for (const d of offline) parts.push(`${d} offline`);
    strip.hidden = !parts.length;
    const text = parts.join(' · ');
    setText(stripText, text);
    setText(stripStatus, strip.hidden ? '' : text);
  }

  // Visible text is one node ("Threads N" with "Threads" visually hidden) so the
  // accessible name contains it. The "needs you" count is a visually hidden suffix.
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

  function renderUserMessage(text, { seq = null, ts = 0, pending = false } = {}) {
    const initial = ownerInitial();
    const attrs = {
      class: ['msg', 'msg-row', 'msg-user', pending && 'pending'],
      dataset: pending ? { text } : seq !== null ? { seq } : undefined,
    };
    return h(
      'div',
      attrs,
      h('span', { class: 'msg-avatar msg-avatar-user', 'aria-hidden': 'true' }, initial),
      h('div', { class: 'msg-user-body' }, h('div', { class: 'msg-bubble' }, srOnly('You: '), text), timeEl(ts)),
    );
  }

  function renderAssistantMessage(text, { seq = null, ts = 0, continuation = false } = {}) {
    const body = h('div', { class: 'msg-body md' }, renderMarkdown(text));
    const timeNode = timeEl(ts);
    if (continuation) {
      return h(
        'div',
        { class: 'msg msg-row msg-assistant msg-cont', dataset: seq !== null ? { seq } : undefined },
        h('div', { class: 'msg-content' }, srOnly('Grouper: '), h('div', { class: 'msg-assistant-body' }, body, timeNode)),
      );
    }
    return h(
      'div',
      { class: 'msg msg-row msg-assistant', dataset: seq !== null ? { seq } : undefined },
      h('span', { class: 'msg-avatar msg-avatar-assistant', 'aria-hidden': 'true' }),
      h(
        'div',
        { class: 'msg-content' },
        h('div', { class: 'msg-head' }, h('span', { class: 'msg-author' }, 'Grouper')),
        h('div', { class: 'msg-assistant-body' }, body, timeNode),
      ),
    );
  }

  function renderAll() {
    rendered = true;
    transcript.replaceChildren(tail);
    threadRows.clear();
    threadPlans.clear();
    threadGroups.clear();
    activeGroup = null;
    activeRun = null;
    lastSpeaker = null;
    const c = conv(cid);
    if (!c.events.length && !sending) {
      add(h('div', { class: 'empty' }, h('p', null, 'No messages here yet.')));
    }
    for (const e of c.events) {
      recordPlanEvent(e);
      appendEvent(e, false);
    }
    for (const tid of threadPlans.keys()) updateThreadRow(tid);
    renderTail();
    requestAnimationFrame(() => scrollToEnd());
  }

  function recordPlanEvent(e) {
    if (e.kind !== 'thread_activity' || !e.thread_id) return;
    const a = e.event;
    if (a?.type !== 'tool_call' || a.name !== 'update_plan') return;
    const items = a.input?.items;
    if (!Array.isArray(items) || !items.length) return;
    const done = items.filter((it) => it && (it.status === 'completed' || it.status === 'done')).length;
    threadPlans.set(e.thread_id, { done, total: items.length });
  }

  /** `live`: it just happened (only then does an error interrupt a screen reader). */
  function appendEvent(e, live = true) {
    switch (e.kind) {
      case 'user_message': {
        const pending = [...transcript.querySelectorAll('.msg-user.pending')].find((n) => n.dataset.text === e.text);
        pending?.remove();
        transcript.querySelector('.empty')?.remove();
        add(renderUserMessage(e.text, { seq: e.seq, ts: e.ts }));
        activeGroup = null;
        activeRun = null;
        lastSpeaker = 'user';
        break;
      }
      case 'assistant_message':
        transcript.querySelector('.empty')?.remove();
        add(renderAssistantMessage(e.text, { seq: e.seq, ts: e.ts, continuation: lastSpeaker === 'assistant' }));
        activeGroup = null;
        activeRun = null;
        lastSpeaker = 'assistant';
        break;
      case 'error':
        add(h('div', { class: 'msg msg-error', role: live ? 'alert' : null }, icon('warning'), h('div', null, e.message)));
        activeGroup = null;
        activeRun = null;
        lastSpeaker = null;
        break;
      case 'budget_warning':
        add(h('div', { class: 'msg msg-note' }, icon('warning'), `Estimated ${e.scope} spend ${usd(e.spent)} of ${usd(e.limit)} budget.`));
        break;
      case 'thread_started': {
        transcript.querySelector('.empty')?.remove();
        activeRun = null;
        if (!activeGroup) {
          activeGroup = createThreadGroup();
          threadGroups.add(activeGroup);
          add(activeGroup.el);
        }
        addThreadRow(activeGroup, e);
        lastSpeaker = 'assistant';
        break;
      }
      case 'thread_finished':
      case 'approval_resolved':
        addActivity(e);
        break;
      default:
        break;
    }
  }

  // ---- activity lines (3.2): outcomes that need no decision, one line each

  function threadLink(tid, title) {
    return h(
      'a',
      {
        class: 'activity-link',
        href: `#/chat/${enc(store.threads.get(tid)?.conversation_id || cid)}/thread/${enc(tid)}`,
        onclick: (ev) => {
          if (ev.metaKey || ev.ctrlKey || ev.shiftKey || ev.altKey || ev.button !== 0) return;
          ev.preventDefault();
          openThreadSheet(tid);
        },
      },
      title,
    );
  }

  /** The line for one stored event, or null when it isn't worth one. */
  function activityLine(e) {
    const t = e.thread_id ? store.threads.get(e.thread_id) : null;
    const title = t?.title || 'Thread';
    let iconName;
    let parts;
    if (e.kind === 'thread_finished') {
      const tid = e.thread_id;
      const secs = t?.created && e.ts ? Math.max(0, e.ts - t.created) : null;
      const how = e.status === 'done' ? 'finished' : e.status === 'failed' ? 'failed' : 'stopped';
      iconName = e.status === 'done' ? 'check' : e.status === 'failed' ? 'warning' : 'stop';
      parts = [threadLink(tid, title), ` ${how}${secs !== null && e.status === 'done' ? ` in ${elapsed(secs)}` : ''}`];
    } else {
      if (e.reason === 'dropped') return null;
      const asked = conv(cid).events.find((x) => x.kind === 'approval_requested' && x.approval_id === e.approval_id);
      const tool = asked?.tool || 'a tool call';
      iconName = e.approved ? 'check' : 'x';
      parts = [`${e.approved ? 'Approved' : 'Denied'} ${tool}`];
      if (e.thread_id && t?.title) parts.push(' in ', threadLink(e.thread_id, title));
    }
    const stamp = typeof e.ts === 'number' && e.ts > 0 ? h('time', { class: 'activity-time', datetime: new Date(e.ts * 1000).toISOString() }, time(e.ts)) : null;
    return h('li', { class: 'activity-line' }, icon(iconName), h('span', { class: 'activity-text' }, ...parts), stamp);
  }

  function addActivity(e) {
    const line = activityLine(e);
    if (!line) return;
    transcript.querySelector('.empty')?.remove();
    activeGroup = null;
    if (!activeRun) {
      const list = h('ul', { class: 'activity-list', 'aria-label': 'Activity' });
      const el = h('div', { class: 'activity-run' }, list);
      activeRun = { el, list, count: 0, summary: null, details: null };
      add(el);
    }
    const run = activeRun;
    run.list.append(line);
    run.count += 1;
    if (run.count > 4) {
      if (!run.details) {
        run.summary = h('summary', { class: 'activity-fold' }, icon('chevron'), h('span', null));
        run.details = h('details', { class: 'activity-details' }, run.summary);
        run.el.replaceChildren(run.details);
        run.details.append(run.list);
      }
      setText(run.summary.querySelector('span'), `${run.count} events`);
    }
  }

  // ---- grouped threads card (V5, V6)

  function createThreadGroup() {
    const headText = h('span', { class: 'thread-group-title' }, '1 thread running');
    const list = h('ul', { class: 'thread-group-list' });
    const foot = h(
      'button',
      {
        class: 'thread-group-foot',
        type: 'button',
        onclick: () => openThreadsList(),
      },
      icon('down'),
      h('span', null, 'View details'),
      h('span', { class: 'spacer' }),
      icon('chevron'),
    );
    const el = h(
      'section',
      { class: 'thread-group', 'aria-label': 'Threads' },
      h('div', { class: 'thread-group-head' }, icon('thread-tree'), headText),
      list,
      foot,
    );
    return { el, headText, list, ids: [] };
  }

  function addThreadRow(group, started) {
    const tid = started.thread_id;
    const dotSlot = h('span', { class: 'fam-dot fam-unknown', 'aria-hidden': 'true' });
    const titleSpan = h('span', { class: 'thread-line-title-text' });
    const planEl = h('span', { class: 'thread-plan', hidden: true });
    const deviceSr = srOnly('');
    const titleWrap = h('span', { class: 'thread-line-title' }, titleSpan, planEl, deviceSr);
    const modelSr = srOnly('');
    const modelText = h('span', { class: 'model-tag-text' });
    const modelTag = h('span', { class: 'model-tag' }, modelSr, modelText);
    const pillSlot = h('span', { class: 'thread-line-pill' });
    const row = h(
      'a',
      {
        class: 'thread-line fam-unknown',
        href: `#/chat/${enc(cid)}/thread/${enc(tid)}`,
        onclick: (ev) => {
          if (ev.metaKey || ev.ctrlKey || ev.shiftKey || ev.altKey || ev.button !== 0) return;
          ev.preventDefault();
          openThreadSheet(tid);
        },
      },
      dotSlot,
      titleWrap,
      modelTag,
      pillSlot,
      icon('chevron'),
    );
    const entry = { group, row, dotSlot, titleSpan, planEl, deviceSr, modelSr, modelText, pillSlot, started, fam: 'unknown', pillKey: '' };
    threadRows.set(tid, entry);
    group.ids.push(tid);
    group.list.append(h('li', null, row));
    updateThreadRow(tid);
  }

  function openThreadSheet(tid) {
    const targetCid = store.threads.get(tid)?.conversation_id || cid;
    navigate(`#/chat/${enc(targetCid)}/thread/${enc(tid)}`);
  }

  function threadStateOf(tid, started) {
    const t = store.threads.get(tid) || { status: 'running', title: started.title, device_id: started.device_id, model: started.model };
    const approval = approvalsFor(tid).length > 0;
    return { t, approval };
  }

  function updateThreadGroupHead(group) {
    let approvalCount = 0;
    let running = 0;
    let waiting = 0;
    let done = 0;
    let failed = 0;
    let stopped = 0;
    for (const tid of group.ids) {
      const entry = threadRows.get(tid);
      if (!entry) continue;
      const { t, approval } = threadStateOf(tid, entry.started);
      if (approval) approvalCount++;
      else if (t.status === 'running') running++;
      else if (t.status === 'waiting_device') waiting++;
      else if (t.status === 'done') done++;
      else if (t.status === 'failed') failed++;
      else if (t.status === 'stopped') stopped++;
    }
    const n = group.ids.length;
    let label;
    if (running === n && n > 0) label = `${plural(n, 'thread')} running`;
    else if (done === n && n > 0) label = `${plural(n, 'thread')} done`;
    else if (failed === n && n > 0) label = `${plural(n, 'thread')} failed`;
    else if (stopped === n && n > 0) label = `${plural(n, 'thread')} stopped`;
    else {
      const parts = [];
      if (approvalCount) parts.push(`${approvalCount} ${approvalCount === 1 ? 'needs' : 'need'} approval`);
        if (waiting) parts.push(`${waiting} waiting`);
      if (done) parts.push(`${done} done`);
      if (failed) parts.push(`${failed} failed`);
      if (stopped) parts.push(`${stopped} stopped`);
      label = parts.join(' · ') || plural(n, 'thread');
    }
    setText(group.headText, label);
    group.el.dataset.count = String(n);
  }

  function updateThreadRow(tid) {
    const entry = threadRows.get(tid);
    if (!entry) return;
    const s = entry.started;
    const { t, approval } = threadStateOf(tid, s);
    const targetCid = t.conversation_id || cid;
    const modelId = t.model || s.model || '';
    const fam = familyOfId(modelId);
    const device = deviceName(t.device_id || s.device_id);
    const title = t.title || s.title || 'Thread';

    setAttr(entry.row, 'href', `#/chat/${enc(targetCid)}/thread/${enc(tid)}`);
    setAttr(entry.row, 'aria-current', openThread === tid ? 'true' : null);
    setText(entry.titleSpan, title);
    const plan = threadPlans.get(tid);
    entry.planEl.hidden = !plan;
    if (plan) {
      setText(entry.planEl, `${plan.done}/${plan.total}`);
      setAttr(entry.planEl, 'title', `${plan.done} of ${plan.total} plan steps done`);
      entry.planEl.classList.toggle('is-done', plan.total > 0 && plan.done >= plan.total);
    }
    setText(entry.deviceSr, plan ? `, ${plan.done} of ${plan.total} steps, on ${device}` : `, on ${device}`);
    setText(entry.modelSr, `${familyLabel(fam)}: `);
    setText(entry.modelText, modelId || 'Default model');

    if (entry.fam !== fam) {
      entry.fam = fam;
      setFamilyClass(entry.row, fam);
      const nextDot = familyDot(fam);
      entry.dotSlot.replaceWith(nextDot);
      entry.dotSlot = nextDot;
    }

    const pillKey = `${t.status}|${t.checkpoint ? 1 : 0}|${approval ? 1 : 0}|${fam}`;
    if (entry.pillKey !== pillKey) {
      entry.pillKey = pillKey;
      setChildren(entry.pillSlot, statusPill(t.status, { checkpoint: t.checkpoint, approval, family: fam }));
    }

    updateThreadGroupHead(entry.group);
  }

  function scheduleThreadRow(tid) {
    if (threadFrames.has(tid)) return;
    threadFrames.set(
      tid,
      requestAnimationFrame(() => {
        threadFrames.delete(tid);
        updateThreadRow(tid);
      }),
    );
  }

  // The streaming draft and the "Thinking…" chip sit after the stored events.
  function renderTail() {
    scheduleTail.cancel();
    const draft = store.drafts.get(cid);
    const replying = store.replying.has(cid);
    if (draft || replying) {
      const cont = lastSpeaker === 'assistant';
      tailRow.classList.toggle('msg-cont', cont);
      tailAvatar.hidden = cont;
      tailAuthor.hidden = cont;
      tailSr.hidden = !cont;
      if (draft) {
        tailBody.hidden = false;
        tailBody.replaceChildren(renderMarkdown(draft));
        const caret = h('span', { class: 'caret', 'aria-hidden': 'true' });
        const lastBlock = tailBody.lastElementChild;
        (lastBlock && /^(P|LI|H\d)$/.test(lastBlock.tagName) ? lastBlock : tailBody).append(caret);
      } else {
        tailBody.hidden = true;
        tailBody.replaceChildren();
      }
      if (tail.firstChild !== tailRow) tail.replaceChildren(tailRow);
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

  // ---- composer (V8: one rounded card; model chip top-left, tools slot + round teal send top-right, textarea below)

  function buildComposer() {
    const admin = isAdmin();
    let mode = 'project';
    try {
      mode = localStorage.getItem(MODE_KEY) === 'standard' ? 'standard' : 'project';
    } catch {
      /* storage unavailable */
    }
    const inputId = `chat-input-${cid || 'new'}`;
    const placeholderText = () =>
      !admin ? 'Reading only: sign in with an admin token to chat' : phoneMq?.matches ? 'Message Grouper…' : 'Ask Grouper anything…';

    const textarea = h('textarea', {
      id: inputId,
      rows: 1,
      placeholder: placeholderText(),
      disabled: !admin,
      onkeydown: (e) => {
        if (e.key === 'Enter' && !e.shiftKey && !e.isComposing && e.keyCode !== 229) {
          e.preventDefault();
          send();
        }
      },
      oninput: () => updateSend(),
    });
    autoGrow(textarea);

    const onPhoneChange = () => {
      textarea.placeholder = placeholderText();
    };
    phoneMq?.addEventListener?.('change', onPhoneChange);

    const sendBtn = h(
      'button',
      { class: 'send-btn', type: 'submit', 'aria-label': 'Send', disabled: true },
      h('span', { class: 'send-btn-disc', 'aria-hidden': 'true' }, icon('send')),
    );
    const error = h('div', { class: 'composer-error', role: 'alert', hidden: true });

    // The model: a chip button that opens the covering picker. A new chat keeps its
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
        onclick: () => pickModel(modelBtn),
      },
      modelMark,
      h('span', { class: 'visually-hidden' }, 'Model: '),
      modelLabel,
      icon('down'),
    );

    // Reserved empty slot for future attachment/image/code controls (Q6: no dead buttons).
    const toolsSlot = h('div', { class: 'composer-tools' });

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
      chatMenu?.syncModelItem();
    }

    async function pickModel(opener = modelBtn) {
      if (modelBtn.disabled) return;
      const before = currentModel();
      const value = await openModelPicker({ role: role(), current: before, opener });
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

    function setMode(next) {
      mode = next === 'standard' ? 'standard' : 'project';
      try {
        localStorage.setItem(MODE_KEY, mode);
      } catch {
        /* storage unavailable */
      }
      renderModels();
    }

    function updateSend() {
      sendBtn.disabled = !admin || !textarea.value.trim() || sending || (!store.online && store.everOnline);
    }

    const form = h(
      'form',
      {
        class: 'composer chat-composer glass',
        onsubmit: (e) => {
          e.preventDefault();
          send();
        },
      },
      h('label', { for: inputId, class: 'visually-hidden' }, 'Message'),
      h('div', { class: 'composer-top' }, modelBtn, h('span', { class: 'spacer' }), toolsSlot, sendBtn),
      textarea,
      error,
    );
    const wrap = h('div', { class: 'composer-wrap' }, form);
    renderModels();

    return {
      wrap,
      form,
      textarea,
      error,
      mode: () => mode,
      setMode,
      model: () => currentModel(),
      pickModel,
      updateSend,
      renderModels,
      setOffline(offline) {
        textarea.disabled = !admin || offline;
        updateSend();
        renderModels();
      },
      destroy() {
        phoneMq?.removeEventListener?.('change', onPhoneChange);
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
    const pending = renderUserMessage(text, { pending: true });
    if (isNew) transcript.querySelector('.welcome')?.remove();
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
      if (isNew) renderWelcome();
    } finally {
      sending = false;
      composer.updateSend();
      composer.textarea.focus();
    }
  }

  // New-chat empty state with Project / Standard mode cards (Q12).
  function renderWelcome() {
    if (transcript.querySelector('.welcome')) return;
    const current = composer.mode();
    const card = (value, iconName, title, desc) =>
      h(
        'label',
        { class: 'mode-card' },
        h('input', {
          type: 'radio',
          name: 'chat-mode',
          value,
          checked: current === value,
          onchange: () => {
            composer.setMode(value);
          },
        }),
        h(
          'span',
          { class: 'mode-card-head' },
          h('span', { class: 'mode-card-icon', 'aria-hidden': 'true' }, icon(iconName)),
          h('span', { class: 'mode-card-title' }, title),
          h('span', { class: 'mode-card-check', 'aria-hidden': 'true' }, icon('check')),
        ),
        h('span', { class: 'mode-card-desc' }, desc),
      );
    add(
      h(
        'div',
        { class: 'welcome' },
        h('div', { class: 'boot-mark', 'aria-hidden': 'true' }),
        h('h2', null, 'What should we work on?'),
        h('p', { class: 'welcome-sub' }, 'Choose how this conversation runs before your first message.'),
        h(
          'div',
          { class: 'mode-cards', role: 'radiogroup', 'aria-label': 'Chat mode' },
          card('project', 'thread-tree', 'Project', 'Orchestrator hands work to parallel threads on your devices or the hub.'),
          card('standard', 'chat', 'Standard', 'One model answers directly, with shared memory and no threads.'),
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

  buildBar();
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
      recordPlanEvent(e);
      if (e.kind === 'assistant_delta') {
        scheduleTail();
        speakDraft();
      } else if (e.kind === 'turn_finished' || e.kind === 'assistant_message' || e.kind === 'error') {
        scheduleTail();
      }
      if (e.thread_id) scheduleThreadRow(e.thread_id);
      if (e.kind.startsWith('thread_') || e.kind.startsWith('approval_')) renderStrip();
    }),
    on('replying', (id) => {
      if (id === cid) scheduleTail();
    }),
    on('threads', () => {
      for (const tid of threadRows.keys()) scheduleThreadRow(tid);
      renderStrip();
      renderThreadsButton();
    }),
    on('devices', () => {
      for (const tid of threadRows.keys()) scheduleThreadRow(tid);
      renderStrip();
    }),
    on('approvals', () => {
      renderThreadsButton();
      for (const tid of threadRows.keys()) scheduleThreadRow(tid);
    }),
    on('conversations', () => {
      renderBar();
      renderStrip();
      composer.renderModels();
      if (!isNew && rendered && !info()) missing();
    }),
    on('models', () => {
      composer.renderModels();
      for (const tid of threadRows.keys()) scheduleThreadRow(tid);
    }),
    on('online', (online) => composer.setOffline(!online)),
    on('route', (r) => {
      if (r.view !== 'chat' || r.conversation !== cid) return;
      openThread = r.thread;
      for (const tid of threadRows.keys()) updateThreadRow(tid);
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
    composer.destroy();
    for (const off of subs) off();
    for (const fn of menuCleanup) fn();
    scheduleTail.cancel();
    for (const f of threadFrames.values()) cancelAnimationFrame(f);
  };
}
