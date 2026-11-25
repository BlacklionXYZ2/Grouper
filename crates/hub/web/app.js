// Grouper web client: shell, router and sign-in.
//
// Routes (hash):
//   #/chat                 the last conversation, or a new one
//   #/chat/new             a new conversation
//   #/chat/<id>            a conversation
//   #/chat/<id>/thread/<t> a conversation with a thread open in the inspector
//   #/threads #/devices #/devices/<id> #/models #/usage #/memory #/skills #/settings #/more

import { h, icon, setChildren } from './lib/dom.js';
import { get, onApiError } from './lib/api.js';
import { store, on, emit, pendingApprovals } from './lib/store.js';
import { startLive, stopLive, loadConversations, loadThreads, loadDevices, loadModels, scanApprovals } from './lib/live.js';
import { closeDrawer, navigate } from './lib/ui.js';
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
let mainKey = null;
let mainCleanup = null;
let inspectorKey = null;
let inspectorCleanup = null;
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
      h('div', { class: 'login' }, h('div', { class: 'login-card glass' }, h('h1', null, "Can't reach the hub"), h('p', null, e.message), h('button', { class: 'btn btn-primary', onclick: () => location.reload() }, 'Try again'))),
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
    started = true;
    window.addEventListener('hashchange', route);
    onApiError((e) => {
      if (e.status === 401 && store.me) signedOut();
    });
    setInterval(() => {
      if (store.online && store.me) loadDevices().catch(() => {});
    }, 30000);
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
  mainKey = null;
  inspectorKey = null;
  mainCleanup?.();
  inspectorCleanup?.();
  mainCleanup = inspectorCleanup = null;
  showLogin();
}

// ---- shell

function buildShell() {
  const sidebar = h('aside', { class: 'sidebar glass', 'aria-label': 'Navigation' });
  const main = h('main', { class: 'main', id: 'main' });
  const inspector = h('aside', { class: 'inspector', 'aria-label': 'Thread', hidden: true });
  const inspectorScrim = h('div', { class: 'inspector-scrim', hidden: true, onclick: closeInspector });
  const drawerScrim = h('div', { class: 'drawer-scrim', hidden: true, onclick: closeDrawer });
  const tabbar = h('nav', { class: 'tabbar glass', 'aria-label': 'Tabs' });
  const offline = h('div', { class: 'offline-pill glass', role: 'status', hidden: true }, icon('offline'), h('span', null, 'Offline. Showing the last copy; reconnecting…'));
  const shell = h('div', { class: 'shell' }, sidebar, drawerScrim, main, inspectorScrim, inspector, tabbar, offline);
  setChildren(root, shell);
  els = { shell, sidebar, main, inspector, inspectorScrim, tabbar, offline };

  for (const topic of ['conversations', 'threads', 'approvals', 'online', 'devices', 'route']) on(topic, renderNav);
  on('online', (online) => {
    els.offline.hidden = online || !store.everOnline;
  });
  renderNav();
  document.addEventListener('keydown', (e) => {
    if (e.key === 'Escape' && !document.querySelector('dialog[open]')) {
      if (els.shell.classList.contains('drawer-open')) closeDrawer();
      else if (!els.inspector.hidden) closeInspector();
    }
  });
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

function renderNav() {
  if (!els) return;
  const r = parseRoute();
  const navLink = (href, label, iconName, current, badge) =>
    h(
      'a',
      { class: 'side-item', href, 'aria-current': current ? 'page' : null },
      icon(iconName),
      h('span', { class: 'label' }, label),
      badge ? h('span', { class: 'count', 'aria-label': `${badge} waiting` }, String(badge)) : null,
    );

  // Chats, newest first.
  const chats = store.conversations.length
    ? h(
        'ul',
        { class: 'side-list' },
        store.conversations.slice(0, 50).map((c) =>
          h(
            'li',
            null,
            h(
              'a',
              { class: 'side-item', href: `#/chat/${encodeURIComponent(c.id)}`, 'aria-current': r.view === 'chat' && r.conversation === c.id ? 'page' : null },
              h('span', { class: 'label' }, h('span', { class: 'title' }, c.title || 'Untitled'), h('span', { class: 'sub' }, `${c.mode === 'standard' ? 'Standard' : 'Project'} · ${ago(c.created)}`)),
            ),
          ),
        ),
      )
    : h('p', { class: 'side-empty' }, 'No conversations yet.');

  // Threads that need you, then running.
  const approvalsByThread = new Set(pendingApprovals().map((a) => a.thread_id));
  const active = [...store.threads.values()]
    .filter((t) => isActive(t.status) || approvalsByThread.has(t.id))
    .sort((a, b) => rank(a, approvalsByThread) - rank(b, approvalsByThread) || b.created - a.created)
    .slice(0, 6);
  const threads = active.length
    ? h(
        'ul',
        { class: 'side-list' },
        active.map((t) => {
          const needs = approvalsByThread.has(t.id);
          return h(
            'li',
            null,
            h(
              'a',
              { class: 'side-item', href: `#/chat/${encodeURIComponent(t.conversation_id)}/thread/${encodeURIComponent(t.id)}`, 'aria-current': r.thread === t.id ? 'page' : null },
              icon(needs ? 'shield' : t.status === 'waiting_device' ? 'offline' : 'threads'),
              h('span', { class: 'label' }, h('span', { class: 'title' }, t.title), h('span', { class: 'sub' }, needs ? 'Needs approval' : t.status === 'waiting_device' ? 'Waiting for device' : 'Running')),
            ),
          );
        }),
      )
    : h('p', { class: 'side-empty' }, 'Nothing running.');

  const approvals = pendingApprovals();
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
      approvals.length
        ? h(
            'button',
            { class: 'banner warn', type: 'button', onclick: () => openApproval(approvals[0].approval_id) },
            icon('shield'),
            h('span', { class: 'grow' }, approvals.length === 1 ? '1 approval waiting' : `${approvals.length} approvals waiting`),
            icon('chevron'),
          )
        : null,
      h('section', { class: 'side-section', 'aria-labelledby': 'side-chats' }, h('h2', { class: 'side-title', id: 'side-chats' }, 'Chats'), chats),
      h(
        'section',
        { class: 'side-section', 'aria-labelledby': 'side-threads' },
        h('h2', { class: 'side-title', id: 'side-threads' }, 'Threads', h('a', { href: '#/threads' }, 'All')),
        threads,
      ),
    ),
    h(
      'nav',
      { class: 'side-foot', 'aria-label': 'Sections' },
      navLink('#/devices', 'Devices', 'devices', r.view === 'devices'),
      navLink('#/models', 'Models', 'models', r.view === 'models', store.pendingProposals),
      navLink('#/usage', 'Usage & limits', 'usage', r.view === 'usage'),
      navLink('#/memory', 'Memory', 'memory', r.view === 'memory'),
      navLink('#/skills', 'Skills', 'skills', r.view === 'skills'),
      navLink('#/settings', 'Settings', 'settings', r.view === 'settings'),
      h('div', { class: ['conn', store.online && 'online'], role: 'status' }, h('span', { class: 'conn-dot', 'aria-hidden': 'true' }), store.online ? `Connected · ${store.me?.name || 'signed in'}` : 'Offline · reconnecting'),
    ),
  );

  const needs = needsYouCount();
  const tab = (href, label, iconName, current, badge) =>
    h('a', { class: 'tab', href, 'aria-current': current ? 'page' : null }, icon(iconName), h('span', null, label), badge ? h('span', { class: 'count', 'aria-label': `${badge} need you` }, String(badge)) : null);
  setChildren(
    els.tabbar,
    tab('#/chat', 'Chat', 'chat', r.view === 'chat'),
    tab('#/threads', 'Threads', 'threads', r.view === 'threads', needs),
    tab('#/devices', 'Devices', 'devices', r.view === 'devices'),
    tab('#/more', 'More', 'more', ['more', 'models', 'usage', 'memory', 'skills', 'settings'].includes(r.view), store.pendingProposals),
  );
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

  const key = r.view === 'chat' ? `chat:${r.conversation || 'new'}` : `${r.view}:${r.id || ''}`;
  if (key !== mainKey) {
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
  if (tKey !== inspectorKey) {
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
  emit('route', r);
}

function forgetConversation(id) {
  try {
    if (localStorage.getItem(LAST_CONV) === id) localStorage.removeItem(LAST_CONV);
  } catch {
    /* storage unavailable */
  }
}

boot();
