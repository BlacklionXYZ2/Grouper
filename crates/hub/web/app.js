// Grouper web client: shell, router and sign-in.
//
// Routes (hash):
//   #/chat                 the last conversation, or a new one
//   #/chat/new             a new conversation
//   #/chat/<id>            a conversation
//   #/chat/<id>/thread/<t> a conversation with a thread open in the inspector
//   #/chat/<id>/threads    this conversation with the threads panel open
//   #/chat/<id>?approval=<a>           the conversation with its approval dock on request <a>
//   #/chat/<id>/thread/<t>?approval=<a> the same, with thread <t> open and its dock on <a>
//                          (notifications and inbox Review use the first; the dock checks
//                          <a> against the hub's pending list and says when it's gone)
//   #/threads              current or last conversation with the panel open, or #/chat
//   #/threads/<t>          resolves the thread's conversation and opens the inspector
//                          (keeps `?approval=<a>`)
//   #/inbox                Needs you: approvals and waiting threads across all chats
//   #/models               the registry
//   #/models/<id>          the registry with that model's provider group open, its row shown and focused
//   #/usage                usage and limits; `?provider=<id>` filters the breakdown
//   #/usage?conversation=<id>  one chat's usage (also takes `&provider=<id>`)
//   #/devices #/devices/<id> #/memory #/skills #/settings #/more
//
// The shell (sidebar, tab bar) is built once per sign-in and updated in place, so
// focus and scroll survive the live updates. Everything it subscribes to is torn
// down on sign-out.

import { h, icon, setChildren, syncList, setText, setAttr } from './lib/dom.js';
import { get, enc, isAbort, onApiError } from './lib/api.js';
import { store, on, emit, threadsIn, approvalsIn, needsYouCount, waitingThreads, resetStore } from './lib/store.js';
import { startLive, stopLive, loadConversations, loadThreads, loadDevices, loadModels, loadApprovals } from './lib/live.js';
import { closeDrawer, navigate, syncInert, bar, skeletonBlocks, errorBanner, toast } from './lib/ui.js';
import { ago } from './lib/format.js';
import { renderLogin } from './views/login.js';
import { renderChat } from './views/chat.js';
import { renderThread } from './views/thread.js';
import { renderDevices } from './views/devices.js';
import { renderModels } from './views/models.js';
import { renderUsage } from './views/usage.js';
import { renderMemory } from './views/memory.js';
import { renderSkills } from './views/skills.js';
import { renderSettings } from './views/settings.js';
import { renderMore } from './views/more.js';
import { renderInbox } from './views/inbox.js';
import { startNotifications, stopNotifications, forgetNotifications } from './lib/push.js';
import { mountPalette, openPalette, paletteShortcut } from './views/palette.js';

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

function onHashChange() {
  if (store.me && els) route();
  else if (!store.me && !els) showLogin();
}

async function boot() {
  applyTheme();
  window.addEventListener('hashchange', onHashChange);
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
  const r = parseRoute();
  setChildren(
    root,
    renderLogin(
      (me) => {
        store.me = me;
        startApp();
      },
      r.view === 'claim' ? { claim: true, code: r.code, error: r.error } : undefined,
    ),
  );
}

function startApp() {
  root.className = '';
  root.removeAttribute('aria-busy');
  buildShell();
  if (!started) {
    // Once per page: these outlive sign-outs and check `store.me` themselves.
    started = true;
    onApiError((e) => {
      if (e.status === 401 && store.me) signedOut();
    });
    on('session_ended', () => {
      if (store.me) signedOut();
    });
  }
  startLive();
  startNotifications();
  Promise.allSettled([
    loadConversations(),
    loadDevices(),
    loadModels(),
    loadThreads(),
    loadApprovals(),
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
  // The session is already gone on the hub: unsubscribe here (the hub drops its row).
  stopNotifications();
  forgetNotifications({ server: false });
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
  // Remount #/chat/new so the composer reads a mode the palette just stored.
  window.addEventListener('grouper-palette-remount-chat', () => {
    mainKey = '';
  }, { signal });
  mountPalette(signal);
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
  // Needs you: everything waiting on the owner across all chats (#/inbox).
  const needsBadge = countBadge('need you');
  const needsItem = h(
    'a',
    { class: 'side-item side-needs', href: '#/inbox' },
    icon('inbox'),
    h('span', { class: 'label' }, h('span', { class: 'title' }, 'Needs you'), h('span', { class: 'sub' })),
    needsBadge,
  );
  const chatList = h('ul', { class: 'side-list' });
  const chatEmpty = h('p', { class: 'side-empty' }, 'No conversations yet.');
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
      h(
        'button',
        {
          type: 'button',
          class: 'palette-launch only-desktop',
          'aria-label': 'Search chats, threads, models, devices, pages, actions',
          'aria-haspopup': 'dialog',
          'aria-controls': 'command-palette',
          'aria-expanded': 'false',
          'aria-keyshortcuts': paletteShortcut().keys,
          onclick: () => openPalette(),
        },
        icon('search'),
        h('span', { class: 'palette-launch-label' }, 'Search'),
        h('kbd', { class: 'palette-kbd', 'aria-hidden': 'true' }, paletteShortcut().label),
      ),
    ),
    h(
      'div',
      { class: 'sidebar-scroll' },
      needsItem,
      h('section', { class: 'side-section', 'aria-labelledby': 'side-chats' }, h('h2', { class: 'side-title', id: 'side-chats' }, 'Chats'), chatList, chatEmpty),
    ),
    h('nav', { class: 'side-foot', 'aria-label': 'Sections' }, foot.map((f) => f.a), conn),
  );

  const tab = (href, label, iconName, views, badgeLabel) => {
    const badge = badgeLabel ? countBadge(badgeLabel) : null;
    const a = h('a', { class: 'tab', href }, icon(iconName), h('span', null, label), badge);
    return { a, view: views, badge };
  };
  const tabs = [
    tab('#/chat', 'Chat', 'chat', ['chat'], 'need you'),
    tab('#/devices', 'Devices', 'devices', ['devices']),
    // The inbox is reached from More on phones for now, so More stays lit there.
    tab('#/more', 'More', 'more', ['more', 'models', 'usage', 'memory', 'skills', 'settings', 'inbox'], 'pending'),
  ];
  setChildren(els.tabbar, tabs.map((t) => t.a));
  nav = { needsItem, needsBadge, chatList, chatEmpty, foot, conn, tabs };
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

function renderNav() {
  if (!els || !nav) return;
  const r = parseRoute();

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

  // Needs you: the count badge, and the same in words (status is never colour alone).
  const approvalCount = store.approvals.size;
  const waitingCount = waitingThreads().length;
  setAttr(nav.needsItem, 'aria-current', r.view === 'inbox' ? 'page' : null);
  setBadge(nav.needsBadge, approvalCount + waitingCount);
  const needsParts = [];
  if (approvalCount) needsParts.push(approvalCount === 1 ? '1 approval' : `${approvalCount} approvals`);
  if (waitingCount) needsParts.push(`${waitingCount} waiting`);
  setText(nav.needsItem.querySelector('.sub'), needsParts.length ? needsParts.join(' · ') : 'Nothing right now');

  for (const f of nav.foot) {
    setAttr(f.a, 'aria-current', f.view.includes(r.view) ? 'page' : null);
    if (f.badge) setBadge(f.badge, store.pendingProposals);
  }
  nav.conn.classList.toggle('online', store.online);
  setText(nav.conn.querySelector('.conn-text'), store.online ? `Connected · ${store.me?.name || 'signed in'}` : 'Offline · reconnecting');

  const needs = needsYouCount();
  for (const t of nav.tabs) {
    setAttr(t.a, 'aria-current', t.view.includes(r.view) ? 'page' : null);
    if (t.badge) setBadge(t.badge, t.view[0] === 'chat' ? needs : store.pendingProposals);
  }
}

// ---- router

export function parseRoute() {
  const raw = currentHash().replace(/^#\/?/, '');
  const qIndex = raw.indexOf('?');
  const pathStr = qIndex >= 0 ? raw.slice(0, qIndex) : raw;
  const query = new URLSearchParams(qIndex >= 0 ? raw.slice(qIndex + 1) : '');
  const parts = pathStr
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
    // #/chat/<id>/threads is the chat with the threads panel open, not its own page.
    if (a && a !== 'new' && b === 'threads' && !c) {
      return { view: 'chat', conversation: a, threadsPanel: true };
    }
    if (a === 'new' && b === 'thread' && c) {
      return { view: 'thread-redirect', thread: c, approval: query.get('approval') || undefined };
    }
    return {
      view,
      conversation: a && a !== 'new' ? a : a === 'new' ? null : undefined,
      thread: b === 'thread' ? c : undefined,
      // The approval dock's selection; not part of the view's key (it changes in place).
      approval: query.get('approval') || undefined,
    };
  }
  if (view === 'threads' && !a) return { view: 'threads-redirect' };
  if ((view === 'threads' || view === 'thread') && a) {
    return { view: 'thread-redirect', thread: a, approval: query.get('approval') || undefined };
  }
  if (view === 'usage') {
    const conversation = query.get('conversation') || (a === 'conversation' ? b : a) || null;
    // The provider filter changes in place: it's not part of the route key below.
    return { view: 'usage', conversation, provider: query.get('provider') || null };
  }
  if (view === 'claim') {
    return { view: 'claim', code: query.get('code') || '', error: query.get('error') || '' };
  }
  return { view, id: a };
}

/** `?approval=<id>` for a route that carries one (redirects keep it). */
function approvalQuery(r) {
  return r.approval ? `?approval=${enc(r.approval)}` : '';
}

function renderThreadRedirect(container, { thread: threadId, approval }) {
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
        location.replace(`#/chat/${enc(thread.conversation_id)}/thread/${enc(threadId)}${approvalQuery({ approval })}`);
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

function lastConversationId() {
  if (store.currentConversation) return store.currentConversation;
  try {
    return localStorage.getItem(LAST_CONV);
  } catch {
    return null;
  }
}

function route() {
  if (!els || !store.me) return;
  closeDrawer();
  const r = parseRoute();

  if (r.view === 'claim') {
    if (r.error) toast(r.error, 'error');
    location.replace('#/');
    return;
  }

  // #/threads opens the panel on the chat you were in (or the last one).
  if (r.view === 'threads-redirect') {
    const id = lastConversationId();
    location.replace(id ? `#/chat/${enc(id)}/threads` : '#/chat');
    return;
  }

  // Deep links like #/threads/<id>: resolve the thread's conversation and open it there.
  if (r.view === 'thread-redirect' && r.thread) {
    const known = store.threads.get(r.thread);
    if (known?.conversation_id) {
      location.replace(`#/chat/${enc(known.conversation_id)}/thread/${enc(r.thread)}${approvalQuery(r)}`);
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
      location.replace(`#/chat/${enc(known.conversation_id)}/thread/${enc(r.thread)}${approvalQuery(r)}`);
      return;
    }
  }

  // Track which conversation is currently open (null when none is open).
  // The threads panel is part of the chat, so opening it does not clear this.
  if (r.view === 'chat') {
    store.currentConversation = r.conversation || null;
  } else {
    store.currentConversation = null;
  }

  if (store.currentConversation && r.view === 'chat') {
    try {
      localStorage.setItem(LAST_CONV, store.currentConversation);
    } catch {
      /* storage unavailable */
    }
  }

  let mainChanged = false;
  // Panel open/closed stays on the same chat key so the transcript is not rebuilt.
  const key =
    r.view === 'chat'
      ? `chat:${r.conversation || 'new'}`
      : r.view === 'thread-redirect'
        ? `thread-redirect:${r.thread || ''}`
        : r.view === 'usage'
          ? `usage:${r.conversation || ''}`
          : `${r.view}:${r.id || ''}`;
  if (key !== mainKey) {
    mainChanged = true;
    mainKey = key;
    mainCleanup?.();
    mainCleanup = null;
    els.main.replaceChildren();
    const params = {
      ...r,
      onMissing: () => forgetConversation(r.conversation),
    };
    const renderers = {
      chat: renderChat,
      'thread-redirect': renderThreadRedirect,
      devices: renderDevices,
      models: renderModels,
      usage: renderUsage,
      memory: renderMemory,
      skills: renderSkills,
      settings: renderSettings,
      more: renderMore,
      inbox: renderInbox,
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
      // Back from a thread to the open panel: the panel already focused its heading.
      if (r.threadsPanel) {
        inspectorOpener = null;
        return;
      }
      const opener = inspectorOpener;
      inspectorOpener = null;
      // A thread opened from the panel leaves the opener inside the hidden dock.
      const visible = opener?.isConnected && !opener.closest('[inert]') && !opener.closest('[hidden]');
      const back = visible ? opener : els.main.querySelector('.threads-btn');
      if (back) back.focus({ preventScroll: true });
      else focusMain();
      return;
    }
    if (!mainChanged) return;
    const viewFocused = active && active !== document.body && (els.main.contains(active) || els.inspector.contains(active) || active.closest('dialog'));
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
