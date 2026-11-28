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
//   #/usage                the two summaries (API costs, Subscriptions); `?provider=<id>` opens
//                          with that key focused
//   #/usage?conversation=<id>  one chat's usage (API costs scoped to it, then its threads)
//   #/usage/breakdown      the breakdown table (`?provider=<id>` filters it)
//   #/usage/limits         limits and budgets
//   #/devices #/devices/<id> #/memory #/skills #/settings #/more
//   #/link?code=<c>        approve or deny a device's sign-in (`grouper login`); signed out,
//                          it goes through sign-in and comes back here
//   #/token-signin         recovery sign-in with a hub token (linked from nowhere; README)
//   #/claim?code=<c>       claim an unclaimed hub (signed out)
//
// The shell (sidebar, tab bar) is built once per sign-in and updated in place, so
// focus and scroll survive the live updates. Everything it subscribes to is torn
// down on sign-out.

import { h, icon, setChildren, syncList, setText, setAttr, srOnly } from './lib/dom.js';
import { get, enc, isAbort, onApiError } from './lib/api.js';
import { store, on, emit, threadsIn, approvalsIn, needsYouCount, waitingThreads, resetStore } from './lib/store.js';
import { startLive, stopLive, loadConversations, loadThreads, loadDevices, loadModels, loadApprovals } from './lib/live.js';
import { closeDrawer, navigate, syncInert, bar, skeletonBlocks, errorBanner, toast, isNarrow } from './lib/ui.js';
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
import { renderLink, rememberPendingLink, takePendingLink } from './views/link.js';
import { startNotifications, stopNotifications, forgetNotifications } from './lib/push.js';
import { mountPalette, openPalette } from './views/palette.js';
import { mountStatusline } from './views/statusline.js';

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
  // A device's sign-in request opened while signed out: come back to it after signing in
  // (Google sign-in returns to #/, so the page remembers it for this tab).
  if (r.view === 'link') rememberPendingLink(r.code);
  const opts =
    r.view === 'claim'
      ? { claim: true, code: r.code, error: r.error }
      : r.view === 'token-signin'
        ? { tokenOnly: true }
        : r.view === 'link'
          ? { link: true }
          : undefined;
  setChildren(
    root,
    renderLogin((me) => {
      store.me = me;
      startApp();
    }, opts),
  );
}

function startApp() {
  root.className = '';
  root.removeAttribute('aria-busy');
  const pending = takePendingLink();
  if (pending && location.hash !== pending) history.replaceState(null, '', pending);
  else if (parseRoute().view === 'token-signin') history.replaceState(null, '', '#/');
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

  const subs = ['conversations', 'threads', 'approvals', 'online', 'devices'].map((topic) => on(topic, scheduleNav));
  subs.push(
    on('online', (online) => {
      if (els) els.offline.hidden = online || !store.everOnline;
    }),
  );
  // Keep the Needs you age ("3m ago") current while something waits.
  const ageTimer = setInterval(() => {
    if (needsYouCount()) scheduleNav();
  }, 30000);
  signal.addEventListener('abort', () => {
    subs.forEach((off) => off());
    clearInterval(ageTimer);
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
  mountStatusline(shell, signal);
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

// Chats shown in the sidebar before "More chats…" (V4: one-line rows, about eight).
const SIDE_CHATS = 8;
// On phones the palette isn't offered, so "More chats…" lists the rest in the drawer.
let showAllChats = false;

function moreChats() {
  if (isNarrow()) {
    showAllChats = true;
    renderNav();
    nav?.chatList.querySelector(`li:nth-child(${SIDE_CHATS + 1}) a`)?.focus();
  } else {
    // The palette has no "chats only" entry point yet; it opens on everything,
    // with recent chats first.
    openPalette();
  }
}

function buildNav() {
  showAllChats = false;
  const navLink =(href, label, iconName, view, badgeLabel) => {
    const badge = badgeLabel ? countBadge(badgeLabel) : null;
    const a = h('a', { class: 'side-item', href }, icon(iconName), h('span', { class: 'label' }, label), badge);
    return { a, view, badge };
  };
  const newChat = h('a', { class: 'side-new', href: '#/chat/new' }, icon('new-chat'), h('span', null, 'New chat'));
  const chatList = h('ul', { class: 'side-list' });
  const chatEmpty = h('p', { class: 'side-empty' }, 'No conversations yet.');
  const more = h(
    'button',
    { type: 'button', class: 'side-item side-more', hidden: true, onclick: moreChats },
    icon('more'),
    h('span', { class: 'label' }, 'More chats…'),
  );
  // Needs you: everything waiting on the owner across all chats (#/inbox). A section
  // header with the newest item's age and the count; its name says it in words.
  const needsBadge = countBadge('need you');
  needsBadge.setAttribute('aria-hidden', 'true');
  const needsAge = h('span', { class: 'side-needs-age', 'aria-hidden': 'true' });
  // Words for screen readers follow the visible label, so the name starts with what's shown.
  const needsWords = h('span', { class: 'visually-hidden' });
  const needsItem = h('a', { class: 'side-item', href: '#/inbox' }, h('span', { class: 'label' }, 'Needs you', needsWords), needsAge, needsBadge);
  const sections = [
    navLink('#/devices', 'Devices', 'devices-fill', ['devices']),
    navLink('#/models', 'Models', 'models-fill', ['models'], 'waiting'),
    navLink('#/usage', 'Usage', 'usage-fill', ['usage']),
    navLink('#/memory', 'Memory', 'memory-fill', ['memory']),
    navLink('#/skills', 'Skills', 'skills-fill', ['skills']),
    navLink('#/settings', 'Settings', 'settings-fill', ['settings']),
  ];
  // Real state, not a slogan (Q8): opens Devices.
  const status = h(
    'a',
    { class: 'side-item side-status', href: '#/devices' },
    h('span', { class: 'conn-dot', 'aria-hidden': 'true' }),
    h('span', { class: 'label' }),
    icon('chevron'),
  );
  setChildren(
    els.sidebar,
    h('div', { class: 'sidebar-head' }, h('a', { class: 'brand', href: '#/chat' }, h('span', { class: 'brand-mark', 'aria-hidden': 'true' }), 'Grouper')),
    newChat,
    h(
      'div',
      { class: 'sidebar-scroll' },
      h('section', { class: 'side-section', 'aria-labelledby': 'side-chats' }, h('h2', { class: 'side-title', id: 'side-chats' }, 'Chats'), chatList, more, chatEmpty),
      h('div', { class: 'side-needs' }, needsItem),
      h('nav', { class: 'side-nav', 'aria-label': 'Sections' }, sections.map((f) => f.a)),
    ),
    h('div', { class: 'side-foot' }, status),
  );

  // Phone tabs: outline icons, filled when selected (CSS shows one of the two).
  const tab = (href, label, iconName, views, badgeLabel) => {
    const badge = badgeLabel ? countBadge(badgeLabel) : null;
    const a = h('a', { class: 'tab', href }, icon(iconName), icon(`${iconName}-fill`), h('span', null, label), badge);
    return { a, view: views, badge };
  };
  const tabs = [
    tab('#/chat', 'Chat', 'chat', ['chat'], 'need you'),
    // Usage is a tab of its own (usage-tab-spec 2); it has no badge in this round.
    tab('#/usage', 'Usage', 'usage', ['usage']),
    tab('#/devices', 'Devices', 'devices', ['devices']),
    // The inbox is reached from More on phones for now, so More stays lit there.
    tab('#/more', 'More', 'more', ['more', 'models', 'memory', 'skills', 'settings', 'inbox'], 'pending'),
  ];
  setChildren(els.tabbar, tabs.map((t) => t.a));
  nav = { newChat, needsItem, needsWords, needsBadge, needsAge, chatList, chatEmpty, more, sections, status, tabs };
}

function chatSideItem() {
  const a = h(
    'a',
    { class: 'side-item' },
    icon('chat'),
    h('span', { class: 'label' }),
    srOnly(''),
    h('span', { class: 'side-state', hidden: true, 'aria-hidden': 'true' }),
    countBadge('approvals waiting'),
  );
  a.querySelector('.count').setAttribute('aria-hidden', 'true');
  return h('li', null, a);
}

/** The sidebar's chats: the newest few, plus the open one if it's older. */
function sidebarChats(r) {
  const limit = showAllChats ? 50 : SIDE_CHATS;
  const list = store.conversations.slice(0, limit);
  const open = r.view === 'chat' && r.conversation;
  if (open && !list.some((c) => c.id === open)) {
    const c = store.conversations.find((x) => x.id === open);
    if (c) list.splice(limit - 1, 1, c);
  }
  return list;
}

/** Newest thing in Needs you (an approval request or a thread that began waiting). */
function newestNeed() {
  let newest = 0;
  for (const a of store.approvals.values()) newest = Math.max(newest, a.ts || 0);
  for (const t of waitingThreads()) newest = Math.max(newest, t.created || 0);
  return newest;
}

/** The footer's words: "All systems online" only when it's true (Q8). */
function systemStatus() {
  if (!store.online) return { text: store.everOnline ? 'Offline · reconnecting' : 'Connecting…', state: 'offline' };
  // Only devices that run threads count; browsers and phones never connect as nodes.
  const devices = (store.devices || []).filter((d) => d.platform !== 'web' && d.platform !== 'ios');
  const up = devices.filter((d) => d.online).length;
  if (!devices.length) return { text: 'Hub online', state: 'online' };
  if (up === devices.length) return { text: 'All systems online', state: 'online' };
  return { text: `${up} of ${devices.length} devices online`, state: 'partial' };
}

function renderNav() {
  if (!els || !nav) return;
  const r = parseRoute();

  // Chats, newest first: one line each, the live state as a dot or count at the end
  // and in words for screen readers (status is never colour alone).
  const chats = sidebarChats(r);
  syncList(
    nav.chatList,
    chats,
    (c) => c.id,
    chatSideItem,
    (li, c) => {
      const a = li.firstChild;
      setAttr(a, 'href', `#/chat/${enc(c.id)}`);
      setAttr(a, 'aria-current', r.view === 'chat' && r.conversation === c.id ? 'page' : null);
      setText(a.querySelector('.label'), c.title || 'Untitled');
      const chatThreads = threadsIn(c.id);
      const running = chatThreads.filter((t) => t.status === 'running').length;
      const waiting = chatThreads.filter((t) => t.status === 'waiting_device').length;
      const approvals = approvalsIn(c.id).length;
      const parts = [];
      if (approvals) parts.push(approvals === 1 ? '1 approval waiting' : `${approvals} approvals waiting`);
      if (waiting) parts.push(`${waiting} waiting for a device`);
      if (running) parts.push(`${running} running`);
      setText(a.querySelector('.visually-hidden'), parts.length ? `, ${parts.join(', ')}` : '');
      setAttr(a, 'title', [c.mode === 'standard' ? 'Standard' : 'Project', ...(parts.length ? parts : [ago(c.created)])].join(' · '));
      setBadge(a.querySelector('.count'), approvals);
      const dot = a.querySelector('.side-state');
      const state = approvals ? null : waiting ? 'waiting' : running ? 'running' : null;
      dot.hidden = !state;
      dot.classList.toggle('waiting', state === 'waiting');
      dot.classList.toggle('running', state === 'running');
    },
  );
  nav.chatEmpty.hidden = store.conversations.length > 0;
  nav.more.hidden = store.conversations.length <= chats.length;

  // Needs you: the count and the newest item's age, and the same in words.
  const approvalCount = store.approvals.size;
  const waitingCount = waitingThreads().length;
  const newest = approvalCount + waitingCount ? newestNeed() : 0;
  setAttr(nav.needsItem, 'aria-current', r.view === 'inbox' ? 'page' : null);
  setBadge(nav.needsBadge, approvalCount + waitingCount);
  setText(nav.needsAge, newest ? ago(newest) : '');
  const needsParts = [];
  if (approvalCount) needsParts.push(approvalCount === 1 ? '1 approval' : `${approvalCount} approvals`);
  if (waitingCount) needsParts.push(`${waitingCount} waiting for a device`);
  setText(nav.needsWords, `: ${needsParts.length ? `${needsParts.join(', ')}, newest ${ago(newest)}` : 'nothing right now'}`);

  setAttr(nav.newChat, 'aria-current', r.view === 'chat' && r.conversation === null ? 'page' : null);
  for (const f of nav.sections) {
    setAttr(f.a, 'aria-current', f.view.includes(r.view) ? 'page' : null);
    if (f.badge) setBadge(f.badge, store.pendingProposals);
  }
  const sys = systemStatus();
  nav.status.classList.toggle('online', sys.state === 'online');
  nav.status.classList.toggle('partial', sys.state === 'partial');
  setText(nav.status.querySelector('.label'), sys.text);

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
    // #/usage/breakdown and #/usage/limits are their own pages (the old table and editor).
    if (a === 'breakdown' || a === 'limits') {
      return { view: 'usage', sub: a, conversation: null, provider: query.get('provider') || null };
    }
    const conversation = query.get('conversation') || (a === 'conversation' ? b : a) || null;
    // The focused key (summary) or provider filter (breakdown) changes in place: it's
    // not part of the route key below.
    return { view: 'usage', sub: 'summary', conversation, provider: query.get('provider') || null };
  }
  if (view === 'claim') {
    return { view: 'claim', code: query.get('code') || '', error: query.get('error') || '' };
  }
  if (view === 'link') return { view: 'link', id: query.get('code') || '', code: query.get('code') || '' };
  if (view === 'token-signin') return { view: 'token-signin' };
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
  if (r.view === 'token-signin') {
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
          ? `usage:${r.sub}:${r.conversation || ''}`
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
      link: renderLink,
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
