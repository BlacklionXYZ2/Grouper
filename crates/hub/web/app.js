// Grouper web client: shell, router and sign-in.
//
// Routes (hash):
//   #/chat                 the last conversation, or a new one
//   #/chat/new             a new conversation
//   #/chat/<id>            a conversation
//   #/chat/<id>/thread/<t> a conversation with a thread open in the inspector
//   #/threads #/devices #/devices/<id> #/models #/usage #/memory #/skills #/settings #/more
//
// The shell (sidebar, tab bar) is built once per sign-in and updated in place, so
// focus and scroll survive the live updates. Everything it subscribes to is torn
// down on sign-out.

import { h, icon, setChildren, syncList, setText, setAttr } from './lib/dom.js';
import { get, onApiError } from './lib/api.js';
import { store, on, emit, pendingApprovals } from './lib/store.js';
import { startLive, stopLive, loadConversations, loadThreads, loadDevices, loadModels, scanApprovals } from './lib/live.js';
import { closeDrawer, navigate, syncInert } from './lib/ui.js';
import { ago, isActive } from './lib/format.js';
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
  store.me = null;
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

  const subs = ['conversations', 'threads', 'approvals', 'online', 'route'].map((topic) => on(topic, scheduleNav));
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
  if (r.view === 'chat' && r.thread) navigate(`#/chat/${encodeURIComponent(r.conversation)}`);
}

function currentHash() {
  return location.hash || '#/chat';
}

function needsYouCount() {
  const waiting = [...store.threads.values()].filter((t) => t.status === 'waiting_device').length;
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
    { class: 'banner warn', type: 'button', hidden: true, onclick: () => openApproval(pendingApprovals()[0]?.approval_id) },
    icon('shield'),
    h('span', { class: 'grow' }),
    icon('chevron'),
  );
  const chatList = h('ul', { class: 'side-list' });
  const chatEmpty = h('p', { class: 'side-empty' }, 'No conversations yet.');
  const threadList = h('ul', { class: 'side-list' });
  const threadEmpty = h('p', { class: 'side-empty' }, 'Nothing running.');
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
        h('h2', { class: 'side-title', id: 'side-threads' }, 'Threads', h('a', { href: '#/threads' }, 'All')),
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
  nav = { approvalsBtn, chatList, chatEmpty, threadList, threadEmpty, foot, conn, tabs };
}

function sideItem() {
  const title = h('span', { class: 'title' });
  const sub = h('span', { class: 'sub' });
  const a = h('a', { class: 'side-item' }, h('span', { class: 'label' }, title, sub));
  return h('li', null, a);
}

function renderNav() {
  if (!els || !nav) return;
  const r = parseRoute();

  // Chats, newest first.
  syncList(
    nav.chatList,
    store.conversations.slice(0, 50),
    (c) => c.id,
    sideItem,
    (li, c) => {
      const a = li.firstChild;
      setAttr(a, 'href', `#/chat/${encodeURIComponent(c.id)}`);
      setAttr(a, 'aria-current', r.view === 'chat' && r.conversation === c.id ? 'page' : null);
      setText(a.querySelector('.title'), c.title || 'Untitled');
      setText(a.querySelector('.sub'), `${c.mode === 'standard' ? 'Standard' : 'Project'} · ${ago(c.created)}`);
    },
  );
  nav.chatEmpty.hidden = store.conversations.length > 0;

  // Threads that need you, then running.
  const approvalsByThread = new Set(pendingApprovals().map((a) => a.thread_id));
  const active = [...store.threads.values()]
    .filter((t) => isActive(t.status) || approvalsByThread.has(t.id))
    .sort((a, b) => rank(a, approvalsByThread) - rank(b, approvalsByThread) || b.created - a.created)
    .slice(0, 6);
  syncList(
    nav.threadList,
    active,
    (t) => t.id,
    () => {
      const li = sideItem();
      li.firstChild.prepend(h('span', { class: 'side-icon' }));
      return li;
    },
    (li, t) => {
      const a = li.firstChild;
      const needs = approvalsByThread.has(t.id);
      const kind = needs ? 'shield' : t.status === 'waiting_device' ? 'offline' : 'threads';
      const slot = a.querySelector('.side-icon');
      if (slot.dataset.icon !== kind) {
        slot.dataset.icon = kind;
        slot.replaceChildren(icon(kind));
      }
      setAttr(a, 'href', `#/chat/${encodeURIComponent(t.conversation_id)}/thread/${encodeURIComponent(t.id)}`);
      setAttr(a, 'aria-current', r.thread === t.id ? 'page' : null);
      setText(a.querySelector('.title'), t.title);
      setText(a.querySelector('.sub'), needs ? 'Needs approval' : t.status === 'waiting_device' ? 'Waiting for device' : 'Running');
    },
  );
  nav.threadEmpty.hidden = active.length > 0;

  const approvals = pendingApprovals();
  nav.approvalsBtn.hidden = !approvals.length;
  setText(nav.approvalsBtn.querySelector('.grow'), approvals.length === 1 ? '1 approval waiting' : `${approvals.length} approvals waiting`);

  for (const f of nav.foot) {
    setAttr(f.a, 'aria-current', f.view.includes(r.view) ? 'page' : null);
    if (f.badge) setBadge(f.badge, store.pendingProposals);
  }
  nav.conn.classList.toggle('online', store.online);
  setText(nav.conn.querySelector('.conn-text'), store.online ? `Connected · ${store.me?.name || 'signed in'}` : 'Offline · reconnecting');

  const needs = needsYouCount();
  for (const t of nav.tabs) {
    setAttr(t.a, 'aria-current', t.view.includes(r.view) ? 'page' : null);
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
    return { view, conversation: a && a !== 'new' ? a : a === 'new' ? null : undefined, thread: b === 'thread' ? c : undefined };
  }
  return { view, id: a };
}

function route() {
  if (!els || !store.me) return;
  closeDrawer();
  const r = parseRoute();
  if (r.view === 'chat' && r.conversation === undefined) {
    let last = null;
    try {
      last = localStorage.getItem(LAST_CONV);
    } catch {
      /* storage unavailable */
    }
    if (last) {
      location.replace(`#/chat/${encodeURIComponent(last)}`);
      return;
    }
    r.conversation = null;
  }
  if (r.view === 'chat' && r.conversation) {
    try {
      localStorage.setItem(LAST_CONV, r.conversation);
    } catch {
      /* storage unavailable */
    }
  }

  let mainChanged = false;
  const key = r.view === 'chat' ? `chat:${r.conversation || 'new'}` : `${r.view}:${r.id || ''}`;
  if (key !== mainKey) {
    mainChanged = true;
    mainKey = key;
    mainCleanup?.();
    mainCleanup = null;
    els.main.replaceChildren();
    const params = { ...r, onMissing: () => forgetConversation(r.conversation) };
    const renderers = {
      chat: renderChat,
      threads: renderThreads,
      devices: renderDevices,
      models: renderModels,
      usage: renderUsage,
      memory: renderMemory,
      skills: renderSkills,
      settings: renderSettings,
      more: renderMore,
    };
    const render = renderers[r.view] || renderChat;
    mainCleanup = render(els.main, params) || null;
    els.main.querySelector('h1')?.setAttribute('tabindex', '-1');
    document.title = `${els.main.querySelector('h1')?.textContent || 'Grouper'} · Grouper`;
  }

  const tKey = r.view === 'chat' && r.thread ? r.thread : null;
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
      inspectorCleanup = renderThread(els.inspector, { threadId: tKey, conversationId: r.conversation, onClose: closeInspector }) || null;
    }
  }
  syncInert();
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
  try {
    if (localStorage.getItem(LAST_CONV) === id) localStorage.removeItem(LAST_CONV);
  } catch {
    /* storage unavailable */
  }
}

boot();
