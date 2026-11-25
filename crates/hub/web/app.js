// Grouper web client: shell, router and sign-in.
//
// Routes (hash):
//   #/chat                 the last conversation, or a new one
//   #/chat/new             a new conversation
//   #/chat/<id>            a conversation
//   #/chat/<id>/thread/<t> a conversation with a thread open in the inspector
//   #/chat/<id>/threads    a conversation's threads
//   #/threads              current conversation's threads, or active chats if none open
//   #/threads/<t>          resolves thread's conversation and opens it there
//   #/devices #/devices/<id> #/models #/usage #/memory #/skills #/settings #/more
//
// The shell (sidebar, tab bar) is built once per sign-in and updated in place, so
// focus and scroll survive the live updates. Everything it subscribes to is torn
// down on sign-out.

import { h, icon, setChildren, syncList, setText, setAttr } from './lib/dom.js';
import { get, enc, isAbort, onApiError } from './lib/api.js';
import { store, on, emit, threadsIn, approvalsIn, pendingApprovals, resetStore } from './lib/store.js';
import { startLive, stopLive, loadConversations, loadThreads, loadDevices, loadModels, scanApprovals } from './lib/live.js';
import { closeDrawer, navigate, syncInert, bar, skeletonBlocks, errorBanner } from './lib/ui.js';
import { ago, isActive, threadStatus } from './lib/format.js';
import { renderLogin } from './views/login.js';
import { renderChat } from './views/chat.js';
import { renderThread } from './views/thread.js';
import { renderThreads } from './views/threads.js';
import { renderDevices } from './views/devices.js';
import { renderModels } from './views/models.js';
import { renderUsage } from './views/usage.js';
import { renderMemory } from './views/memory.js';
import { renderSkills } from './views/skills.js';
import { renderSettings } from './views/settings.js';
import { renderMore } from './views/more.js';
import { openApproval } from './views/approvals.js';

const root = document.getElementById('app');
const LAST_CONV = 'grouper.lastConversation';

let els = null; // shell elements
let shellAbort = null; // listeners and subscriptions of this sign-in
let mainKey = null;
let mainCleanup = null;
let inspectorKey = null;
let inspectorCleanup = null;
let inspectorOpener = null;
let started = false;

export function applyTheme() {
  let theme = 'system';
  try {
    theme = localStorage.getItem('grouper.theme') || 'system';
  } catch {
    /* storage unavailable */
  }
  if (theme === 'system') document.documentElement.removeAttribute('data-theme');
  else document.documentElement.dataset.theme = theme;
}

async function boot() {
  applyTheme();
  try {
    const me = await get('/v1/session');
    if (!me?.signed_in) {
      showLogin();
      return;
    }
    store.me = me;
  } catch (e) {
    if (e.status === 401) {
      showLogin();
      return;
    }
    setChildren(
      root,
      h('div', { class: 'login' }, h('div', { class: 'login-card' }, h('h1', null, "Can't reach the hub"), h('p', null, e.message), h('button', { class: 'btn btn-primary', onclick: () => location.reload() }, 'Try again'))),
    );
    root.removeAttribute('aria-busy');
    return;
  }
  startApp();
}

function showLogin() {
  root.className = '';
  root.removeAttribute('aria-busy');
  setChildren(root, renderLogin((me) => {
    store.me = me;
    startApp();
  }));
}

function startApp() {
  root.className = '';
  root.removeAttribute('aria-busy');
  buildShell();
  if (!started) {
    // Once per page: these outlive sign-outs and check `store.me` themselves.
    started = true;
    window.addEventListener('hashchange', route);
    onApiError((e) => {
      if (e.status === 401 && store.me) signedOut();
    });
    on('session_ended', () => {
      if (store.me) signedOut();
    });
  }
  startLive();
  Promise.allSettled([
    loadConversations(),
    loadDevices(),
    loadModels(),
    loadThreads().then(scanApprovals),
    get('/v1/hub').then((info) => {
      store.hub = info;
    }),
    store.me.admin
      ? get('/v1/registry/proposals').then((p) => {
          store.pendingProposals = (p || []).length;
          renderNav();
        })
      : null,
  ]);
  route();
}

/** The session ended on the hub (expired, logged out elsewhere, token revoked). */
export function signedOut() {
  stopLive();
  resetStore();
  mainCleanup?.();
  inspectorCleanup?.();
  mainCleanup = inspectorCleanup = null;
  mainKey = null;
  inspectorKey = null;
  teardownShell();
  showLogin();
}

// ---- shell

function teardownShell() {
  shellAbort?.abort();
  shellAbort = null;
  els = null;
}

function buildShell() {
  teardownShell();
  shellAbort = new AbortController();
  const { signal } = shellAbort;

  const skip = h(
    'a',
    {
      class: 'skip-link',
      href: '#main',
      onclick: (e) => {
        // The hash is the router's, so move focus by hand.
        e.preventDefault();
        focusMain();
      },
    },
    'Skip to main content',
  );
  const sidebar = h('aside', { class: 'sidebar glass', id: 'sidebar', 'aria-label': 'Navigation' });
  const main = h('main', { class: 'main', id: 'main', tabindex: '-1' });
  const inspector = h('aside', { class: 'inspector', 'aria-label': 'Thread', hidden: true });
  const inspectorScrim = h('div', { class: 'inspector-scrim', hidden: true, onclick: closeInspector });
  const drawerScrim = h('div', { class: 'drawer-scrim', hidden: true, onclick: () => closeDrawer({ restoreFocus: true }) });
  const tabbar = h('nav', { class: 'tabbar glass', 'aria-label': 'Tabs' });
  const offline = h('div', { class: 'offline-pill glass', role: 'status', hidden: true }, icon('offline'), h('span', null, 'Offline. Showing the last copy; reconnecting…'));
  const shell = h('div', { class: 'shell' }, skip, sidebar, drawerScrim, main, inspectorScrim, inspector, tabbar, offline);
  setChildren(root, shell);
  els = { shell, sidebar, main, inspector, inspectorScrim, tabbar, offline };
  buildNav();

  const subs = ['conversations', 'threads', 'approvals', 'online'].map((topic) => on(topic, scheduleNav));
  subs.push(
    on('online', (online) => {
      if (els) els.offline.hidden = online || !store.everOnline;
    }),
  );
  signal.addEventListener('abort', () => {
    subs.forEach((off) => off());
    cancelAnimationFrame(navFrame);
    navFrame = 0;
  });
  renderNav();
  syncInert();
  document.addEventListener(
    'keydown',
    (e) => {
      if (e.key !== 'Escape' || document.querySelector('dialog[open]') || !els) return;
      if (els.shell.classList.contains('drawer-open')) closeDrawer({ restoreFocus: true });
      else if (!els.inspector.hidden) closeInspector();
    },
    { signal },
  );
}

function focusMain() {
  const target = els?.main.querySelector('h1') || els?.main;
  target?.focus({ preventScroll: true });
}

function closeInspector() {
  const r = parseRoute();
  if (r.view === 'chat' && r.conversation && r.thread) navigate(`#/chat/${enc(r.conversation)}`);
}

function currentHash() {
  return location.hash || '#/chat';
}

function needsYouCount() {
  const approvalThreads = new Set([...store.approvals.values()].map((a) => a.thread_id));
  const waiting = [...store.threads.values()].filter((t) => t.status === 'waiting_device' && !approvalThreads.has(t.id)).length;
  return store.approvals.size + waiting;
}

// The navigation is built once (buildNav) and updated in place (renderNav).
let nav = null;
let navFrame = 0;

function scheduleNav() {
  if (navFrame) return;
  navFrame = requestAnimationFrame(() => {
    navFrame = 0;
    renderNav();
  });
}

function countBadge(label) {
  return h('span', { class: 'count', hidden: true, 'data-label': label });
}

function setBadge(badge, n) {
  badge.hidden = !n;
  setText(badge, n ? String(n) : '');
  setAttr(badge, 'aria-label', n ? `${n} ${badge.dataset.label}` : null);
}

function buildNav() {
  const footLink = (href, label, iconName, view, badgeLabel) => {
    const badge = badgeLabel ? countBadge(badgeLabel) : null;
    const a = h('a', { class: 'side-item', href }, icon(iconName), h('span', { class: 'label' }, label), badge);
    return { a, view, badge };
  };
  const approvalsBtn = h(
    'button',
    {
      class: 'banner warn',
      type: 'button',
      hidden: true,
      onclick: () => {
        const currentCid = store.currentConversation;
        const here = currentCid ? approvalsIn(currentCid) : [];
        if (here.length) {
          openApproval(here[0].approval_id, { conversationId: currentCid });
          return;
        }
        const first = pendingApprovals()[0];
        if (first?.conversation_id) navigate(`#/chat/${enc(first.conversation_id)}`);
      },
    },
    icon('shield'),
    h('span', { class: 'grow' }),
    icon('chevron'),
  );
  const chatList = h('ul', { class: 'side-list' });
  const chatEmpty = h('p', { class: 'side-empty' }, 'No conversations yet.');
  const threadsAllLink = h('a', { href: '#/threads' }, 'All');
  const threadList = h('ul', { class: 'side-list' });
  const threadEmpty = h('p', { class: 'side-empty' }, 'Open a chat to see its threads.');
  const foot = [
    footLink('#/devices', 'Devices', 'devices', ['devices']),
    footLink('#/models', 'Models', 'models', ['models'], 'waiting'),
    footLink('#/usage', 'Usage & limits', 'usage', ['usage']),
    footLink('#/memory', 'Memory', 'memory', ['memory']),
    footLink('#/skills', 'Skills', 'skills', ['skills']),
    footLink('#/settings', 'Settings', 'settings', ['settings']),
  ];
  const conn = h('div', { class: 'conn', role: 'status' }, h('span', { class: 'conn-dot', 'aria-hidden': 'true' }), h('span', { class: 'conn-text' }));
  setChildren(
    els.sidebar,
    h(
      'div',
      { class: 'sidebar-head' },
      h('a', { class: 'brand', href: '#/chat' }, h('span', { class: 'brand-mark', 'aria-hidden': 'true' }), 'Grouper'),
      h('a', { class: 'icon-btn', href: '#/chat/new', 'aria-label': 'New chat', title: 'New chat' }, icon('plus')),
    ),
    h(
      'div',
      { class: 'sidebar-scroll' },
      approvalsBtn,
      h('section', { class: 'side-section', 'aria-labelledby': 'side-chats' }, h('h2', { class: 'side-title', id: 'side-chats' }, 'Chats'), chatList, chatEmpty),
      h(
        'section',
        { class: 'side-section', 'aria-labelledby': 'side-threads' },
        h('h2', { class: 'side-title', id: 'side-threads' }, 'Threads', threadsAllLink),
        threadList,
        threadEmpty,
      ),
    ),
    h('nav', { class: 'side-foot', 'aria-label': 'Sections' }, foot.map((f) => f.a), conn),
  );

  const tab = (href, label, iconName, views, badgeLabel) => {
    const badge = badgeLabel ? countBadge(badgeLabel) : null;
    const a = h('a', { class: 'tab', href }, icon(iconName), h('span', null, label), badge);
    return { a, view: views, badge };
  };
  const tabs = [
    tab('#/chat', 'Chat', 'chat', ['chat']),
    tab('#/threads', 'Threads', 'threads', ['threads'], 'need you'),
    tab('#/devices', 'Devices', 'devices', ['devices']),
    tab('#/more', 'More', 'more', ['more', 'models', 'usage', 'memory', 'skills', 'settings'], 'pending'),
  ];
  setChildren(els.tabbar, tabs.map((t) => t.a));
  nav = { approvalsBtn, chatList, chatEmpty, threadsAllLink, threadList, threadEmpty, foot, conn, tabs };
}

function sideItem() {
  const title = h('span', { class: 'title' });
  const sub = h('span', { class: 'sub' });
  const a = h('a', { class: 'side-item' }, h('span', { class: 'label' }, title, sub));
  return h('li', null, a);
}

function chatSideItem() {
  const li = sideItem();
  li.firstChild.append(countBadge('approvals waiting'));
  return li;
}

function threadRecency(t) {
  return isActive(t.status) ? t.created || 0 : t.finished || t.created || 0;
}

function renderNav() {
  if (!els || !nav) return;
  const r = parseRoute();
  const currentCid = store.currentConversation;

  // Chats, newest first, with per-chat running/waiting lines and approval badges.
  syncList(
    nav.chatList,
    store.conversations.slice(0, 50),
    (c) => c.id,
    chatSideItem,
    (li, c) => {
      const a = li.firstChild;
      setAttr(a, 'href', `#/chat/${enc(c.id)}`);
      setAttr(a, 'aria-current', r.view === 'chat' && r.conversation === c.id ? 'page' : null);
      setText(a.querySelector('.title'), c.title || 'Untitled');
      const chatThreads = threadsIn(c.id);
      const running = chatThreads.filter((t) => t.status === 'running').length;
      const waiting = chatThreads.filter((t) => t.status === 'waiting_device').length;
      const subParts = [c.mode === 'standard' ? 'Standard' : 'Project'];
      if (running) subParts.push(`${running} running`);
      if (waiting) subParts.push(`${waiting} waiting`);
      if (!running && !waiting) subParts.push(ago(c.created));
      setText(a.querySelector('.sub'), subParts.join(' · '));
      setBadge(a.querySelector('.count'), approvalsIn(c.id).length);
    },
  );
  nav.chatEmpty.hidden = store.conversations.length > 0;

  // Current conversation's threads only: needs approval / waiting / running first,
  // then recent finished. Empty with a short hint when no conversation is open.
  const currentApprovals = currentCid ? approvalsIn(currentCid) : [];
  const approvalsByThread = new Set(currentApprovals.map((a) => a.thread_id));
  const scopedThreads = currentCid
    ? threadsIn(currentCid)
        .sort((a, b) => rank(a, approvalsByThread) - rank(b, approvalsByThread) || threadRecency(b) - threadRecency(a))
        .slice(0, 8)
    : [];
  syncList(
    nav.threadList,
    scopedThreads,
    (t) => t.id,
    () => {
      const li = sideItem();
      li.firstChild.prepend(h('span', { class: 'side-icon' }));
      return li;
    },
    (li, t) => {
      const a = li.firstChild;
      const needs = approvalsByThread.has(t.id);
      const st = threadStatus(t.status, { checkpoint: t.checkpoint, approval: needs });
      const kind = needs ? 'shield' : t.status === 'running' ? 'threads' : st.icon;
      const slot = a.querySelector('.side-icon');
      slot.className = `side-icon ${st.tone}`;
      if (slot.dataset.icon !== kind) {
        slot.dataset.icon = kind;
        slot.replaceChildren(icon(kind));
      }
      setAttr(a, 'href', `#/chat/${enc(currentCid)}/thread/${enc(t.id)}`);
      setAttr(a, 'aria-current', r.view === 'chat' && r.conversation === currentCid && r.thread === t.id ? 'page' : null);
      setText(a.querySelector('.title'), t.title);
      const subText = needs
        ? 'Needs approval'
        : t.status === 'waiting_device'
          ? t.checkpoint
            ? 'Checkpoint'
            : 'Waiting for device'
          : t.status === 'running'
            ? 'Running'
            : `${st.word} · ${ago(t.finished || t.created)}`;
      setText(a.querySelector('.sub'), subText);
    },
  );
  const currentChat = currentCid ? store.conversations.find((c) => c.id === currentCid) : null;
  setAttr(nav.threadsAllLink, 'href', currentCid ? `#/chat/${enc(currentCid)}/threads` : '#/threads');
  setAttr(nav.threadsAllLink, 'aria-current', r.view === 'threads' ? 'page' : null);
  nav.threadEmpty.hidden = scopedThreads.length > 0;
  setText(
    nav.threadEmpty,
    !currentCid
      ? 'Open a chat to see its threads.'
      : currentChat?.mode === 'standard'
        ? 'Standard chats have no threads.'
        : 'No threads in this chat yet.',
  );

  // Global sidebar approval indicator so nothing in other chats is missed.
  const approvals = pendingApprovals();
  nav.approvalsBtn.hidden = !approvals.length;
  const approvalText =
    currentCid && !currentApprovals.length && approvals.length
      ? approvals.length === 1
        ? '1 approval in another chat'
        : `${approvals.length} approvals in other chats`
      : approvals.length === 1
        ? '1 approval waiting'
        : `${approvals.length} approvals waiting`;
  setText(nav.approvalsBtn.querySelector('.grow'), approvalText);

  for (const f of nav.foot) {
    setAttr(f.a, 'aria-current', f.view.includes(r.view) ? 'page' : null);
    if (f.badge) setBadge(f.badge, store.pendingProposals);
  }
  nav.conn.classList.toggle('online', store.online);
  setText(nav.conn.querySelector('.conn-text'), store.online ? `Connected · ${store.me?.name || 'signed in'}` : 'Offline · reconnecting');

  const needs = needsYouCount();
  for (const t of nav.tabs) {
    setAttr(t.a, 'aria-current', t.view.includes(r.view) ? 'page' : null);
    if (t.view[0] === 'threads') setAttr(t.a, 'href', currentCid ? `#/chat/${enc(currentCid)}/threads` : '#/threads');
    if (t.badge) setBadge(t.badge, t.view[0] === 'threads' ? needs : store.pendingProposals);
  }
}

function rank(t, approvals) {
  if (approvals.has(t.id)) return 0;
  if (t.status === 'waiting_device') return 1;
  if (t.status === 'running') return 2;
  return 3;
}

// ---- router

export function parseRoute() {
  const parts = currentHash()
    .replace(/^#\/?/, '')
    .split('/')
    .filter(Boolean)
    .map((p) => {
      try {
        return decodeURIComponent(p);
      } catch {
        return p;
      }
    });
  const [view = 'chat', a, b, c] = parts;
  if (view === 'chat') {
    if (a && a !== 'new' && b === 'threads') {
      return { view: 'threads', conversation: a };
    }
    if (a === 'new' && b === 'thread' && c) {
      return { view: 'thread-redirect', thread: c };
    }
    return { view, conversation: a && a !== 'new' ? a : a === 'new' ? null : undefined, thread: b === 'thread' ? c : undefined };
  }
  if ((view === 'threads' || view === 'thread') && a) {
    return { view: 'thread-redirect', thread: a };
  }
  return { view, id: a };
}

function renderThreadRedirect(container, { thread: threadId }) {
  const body = h('div', { class: 'view-body narrow' }, skeletonBlocks(2));
  container.append(h('section', { class: 'view' }, bar({ title: 'Opening thread…' }), body));
  const aborter = new AbortController();

  async function resolve() {
    setChildren(body, skeletonBlocks(2));
    try {
      const thread = await get(`/v1/threads/${enc(threadId)}`, { signal: aborter.signal });
      if (aborter.signal.aborted) return;
      if (thread?.conversation_id) {
        const known = store.threads.get(threadId);
        store.threads.set(threadId, { ...known, ...thread, checkpoint: known?.checkpoint });
        emit('threads');
        location.replace(`#/chat/${enc(thread.conversation_id)}/thread/${enc(threadId)}`);
        return;
      }
      showMissing();
    } catch (err) {
      if (isAbort(err) || aborter.signal.aborted) return;
      if (err.status === 404) showMissing();
      else setChildren(body, errorBanner(err, resolve));
    }
  }

  function showMissing() {
    setChildren(
      body,
      h(
        'div',
        { class: 'empty' },
        h('h2', null, 'Thread not found'),
        h('p', null, "It isn't on this hub."),
        h('p', null, h('a', { href: '#/threads' }, 'View threads'), ' · ', h('a', { href: '#/chat' }, 'Back to chat')),
      ),
    );
  }

  resolve();
  return () => aborter.abort();
}

function route() {
  if (!els || !store.me) return;
  closeDrawer();
  const r = parseRoute();

  // Deep links like #/threads/<id>: resolve the thread's conversation and open it there.
  if (r.view === 'thread-redirect' && r.thread) {
    const known = store.threads.get(r.thread);
    if (known?.conversation_id) {
      location.replace(`#/chat/${enc(known.conversation_id)}/thread/${enc(r.thread)}`);
      return;
    }
  }

  if (r.view === 'chat' && r.conversation === undefined) {
    let last = null;
    try {
      last = localStorage.getItem(LAST_CONV);
    } catch {
      /* storage unavailable */
    }
    if (last) {
      location.replace(`#/chat/${enc(last)}`);
      return;
    }
    r.conversation = null;
  }

  // Never open a thread inside the wrong conversation: switch to its conversation first.
  if (r.view === 'chat' && r.conversation && r.thread) {
    const known = store.threads.get(r.thread);
    if (known?.conversation_id && known.conversation_id !== r.conversation) {
      location.replace(`#/chat/${enc(known.conversation_id)}/thread/${enc(r.thread)}`);
      return;
    }
  }

  // Track which conversation is currently open (null when none is open).
  if (r.view === 'chat') {
    store.currentConversation = r.conversation || null;
  } else if (r.view === 'threads') {
    if (r.conversation) store.currentConversation = r.conversation;
    // Plain #/threads keeps the open conversation if one was open; otherwise null.
  } else {
    store.currentConversation = null;
  }

  if (store.currentConversation && (r.view === 'chat' || (r.view === 'threads' && r.conversation))) {
    try {
      localStorage.setItem(LAST_CONV, store.currentConversation);
    } catch {
      /* storage unavailable */
    }
  }

  const scopedThreadsCid = r.view === 'threads' ? r.conversation || store.currentConversation || null : null;

  let mainChanged = false;
  const key =
    r.view === 'chat'
      ? `chat:${r.conversation || 'new'}`
      : r.view === 'threads'
        ? `threads:${scopedThreadsCid || 'all'}`
        : r.view === 'thread-redirect'
          ? `thread-redirect:${r.thread || ''}`
          : `${r.view}:${r.id || ''}`;
  if (key !== mainKey) {
    mainChanged = true;
    mainKey = key;
    mainCleanup?.();
    mainCleanup = null;
    els.main.replaceChildren();
    const params = {
      ...r,
      ...(r.view === 'threads' ? { conversation: scopedThreadsCid } : null),
      onMissing: () => forgetConversation(r.conversation),
    };
    const renderers = {
      chat: renderChat,
      threads: renderThreads,
      'thread-redirect': renderThreadRedirect,
      devices: renderDevices,
      models: renderModels,
      usage: renderUsage,
      memory: renderMemory,
      skills: renderSkills,
      settings: renderSettings,
      more: renderMore,
    };
    const render = renderers[r.view] || renderChat;
    document.title = 'Grouper';
    mainCleanup = render(els.main, params) || null;
    els.main.querySelector('h1')?.setAttribute('tabindex', '-1');
    if (document.title === 'Grouper') {
      document.title = `${els.main.querySelector('h1')?.textContent || 'Grouper'} · Grouper`;
    }
  }

  const tKey = r.view === 'chat' && r.conversation && r.thread ? `${r.conversation}:${r.thread}` : null;
  let inspectorClosed = false;
  if (tKey !== inspectorKey) {
    const active = document.activeElement;
    if (tKey && !inspectorKey && active && !els.inspector.contains(active) && active !== document.body) inspectorOpener = active;
    inspectorClosed = Boolean(inspectorKey && !tKey);
    inspectorKey = tKey;
    inspectorCleanup?.();
    inspectorCleanup = null;
    els.inspector.replaceChildren();
    els.inspector.hidden = !tKey;
    els.inspectorScrim.hidden = !tKey;
    if (tKey) {
      inspectorCleanup =
        renderThread(els.inspector, {
          threadId: r.thread,
          conversationId: r.conversation,
          onClose: closeInspector,
          onWrongConversation: (targetCid) => {
            location.replace(`#/chat/${enc(targetCid)}/thread/${enc(r.thread)}`);
          },
        }) || null;
    }
  }
  syncInert();
  if (navFrame) {
    cancelAnimationFrame(navFrame);
    navFrame = 0;
  }
  renderNav();
  emit('route', r);

  // Move focus where the person's attention goes: back to what opened a closed
  // inspector, else to the new view's heading unless the view focused something
  // itself (the composer, the inspector's title). Views focus in their own frame
  // callbacks, which run before this one.
  requestAnimationFrame(() => {
    if (!els) return;
    const active = document.activeElement;
    if (inspectorClosed && !mainChanged) {
      const back = inspectorOpener?.isConnected && !inspectorOpener.closest('[inert]') ? inspectorOpener : null;
      inspectorOpener = null;
      if (back) back.focus({ preventScroll: true });
      else focusMain();
      return;
    }
    if (!mainChanged) return;
    const viewFocused = active && active !== document.body && (els.main.contains(active) || els.inspector.contains(active));
    if (!viewFocused) focusMain();
  });
}

function forgetConversation(id) {
  if (store.currentConversation === id) {
    store.currentConversation = null;
    scheduleNav();
  }
  try {
    if (localStorage.getItem(LAST_CONV) === id) localStorage.removeItem(LAST_CONV);
  } catch {
    /* storage unavailable */
  }
}

boot();
