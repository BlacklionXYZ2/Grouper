// Desktop command palette (Ctrl/Cmd+K). One centred dialog: local chats, threads,
// models, devices, pages and safe actions. No transcript search, no hub request,
// no approve/deny. Phone widths never open it (the shell hides the launcher).

import { h, icon, setChildren, setText, setAttr, syncList } from '../lib/dom.js';
import { enc } from '../lib/api.js';
import { store, on, deviceName, needsYouCount } from '../lib/store.js';
import { threadStatus } from '../lib/format.js';

const RECENTS_KEY = 'grouper.paletteRecents';
const MODE_KEY = 'grouper.newMode';
const THEME_KEY = 'grouper.theme';
const PHONE = '(max-width: 760px)';
const CAP = 8;

const PAGES = [
  { id: 'inbox', title: 'Needs you', hash: '#/inbox', icon: 'inbox', verb: 'Open the inbox' },
  { id: 'devices', title: 'Devices', hash: '#/devices', icon: 'devices', verb: 'Open devices' },
  { id: 'models', title: 'Models', hash: '#/models', icon: 'models', verb: 'Open models' },
  { id: 'usage', title: 'Usage & limits', hash: '#/usage', icon: 'usage', verb: 'Open usage and limits' },
  { id: 'memory', title: 'Memory', hash: '#/memory', icon: 'memory', verb: 'Open memory' },
  { id: 'skills', title: 'Skills', hash: '#/skills', icon: 'skills', verb: 'Open skills' },
  { id: 'settings', title: 'Settings', hash: '#/settings', icon: 'settings', verb: 'Open settings' },
];

const ACTIONS = [
  { id: 'new', title: 'New chat', icon: 'plus', verb: 'Start a new chat', mode: null, search: 'new chat' },
  { id: 'new-project', title: 'New project chat', icon: 'plus', verb: 'Start a project chat', mode: 'project', search: 'new project chat' },
  { id: 'new-standard', title: 'New standard chat', icon: 'plus', verb: 'Start a standard chat', mode: 'standard', search: 'new standard chat' },
  { id: 'theme', title: 'Switch theme', icon: 'settings', verb: 'Choose System, Light or Dark', mode: null, search: 'switch theme appearance system light dark' },
];

/** @returns {{ label: string, keys: string }} */
export function paletteShortcut() {
  const platform = `${navigator.platform || ''} ${navigator.userAgent || ''}`;
  const mac = /Mac|iPhone|iPad|iPod/i.test(platform);
  return mac ? { label: '⌘K', keys: 'Meta+K' } : { label: 'Ctrl K', keys: 'Control+K' };
}

function isPhone() {
  return typeof matchMedia === 'function' && matchMedia(PHONE).matches;
}

function fold(value) {
  return String(value ?? '')
    .normalize('NFD')
    .replace(/\p{M}/gu, '')
    .toLocaleLowerCase();
}

/** Punctuation-only text is an empty query. Exact/prefix uses the word phrase. */
export function analyzeQuery(raw) {
  const folded = fold(String(raw ?? '').trim()).replace(/\s+/g, ' ').trim();
  if (!/[\p{L}\p{N}]/u.test(folded)) return { empty: true, tokens: [], folded: '' };
  const tokens = folded.split(/[^\p{L}\p{N}]+/u).filter(Boolean);
  if (!tokens.length) return { empty: true, tokens: [], folded: '' };
  return { empty: false, tokens, folded: tokens.join(' ') };
}

/** Ranges of `text` (NFC) that contain any token, case- and accent-insensitive. */
export function highlightParts(text, tokens) {
  const source = String(text ?? '');
  if (!tokens?.length || !source) return [{ text: source, match: false }];
  const chars = [...source.normalize('NFC')];
  let folded = '';
  const starts = [];
  for (let i = 0; i < chars.length; i++) {
    const piece = fold(chars[i]);
    for (let k = 0; k < piece.length; k++) starts.push(i);
    folded += piece;
  }
  const ranges = [];
  for (const token of tokens) {
    if (!token) continue;
    let from = 0;
    while (from <= folded.length - token.length) {
      const at = folded.indexOf(token, from);
      if (at < 0) break;
      ranges.push([starts[at], starts[at + token.length - 1] + 1]);
      from = at + Math.max(token.length, 1);
    }
  }
  if (!ranges.length) return [{ text: source.normalize('NFC'), match: false }];
  ranges.sort((a, b) => a[0] - b[0] || a[1] - b[1]);
  const merged = [];
  for (const range of ranges) {
    const last = merged[merged.length - 1];
    if (!last || range[0] > last[1]) merged.push([range[0], range[1]]);
    else last[1] = Math.max(last[1], range[1]);
  }
  const parts = [];
  let i = 0;
  const nfc = chars;
  for (const [a, b] of merged) {
    if (a > i) parts.push({ text: nfc.slice(i, a).join(''), match: false });
    parts.push({ text: nfc.slice(a, b).join(''), match: true });
    i = b;
  }
  if (i < nfc.length) parts.push({ text: nfc.slice(i).join(''), match: false });
  return parts.filter((p) => p.text);
}

function score(item, query) {
  const titleFold = fold(item.title).replace(/\s+/g, ' ').trim();
  const phrase = query.folded;
  if (titleFold === phrase) return 0;
  if (phrase && titleFold.startsWith(phrase)) return 1;
  const inTitle = query.tokens.every((token) => titleFold.includes(token));
  if (inTitle) return 2;
  const hay = `${titleFold} ${fold(item.search || '')}`;
  if (query.tokens.every((token) => hay.includes(token))) return 3;
  return null;
}

function byRank(items, query) {
  const out = [];
  for (const item of items) {
    const rank = score(item, query);
    if (rank === null) continue;
    out.push({ ...item, rank });
  }
  out.sort(
    (a, b) =>
      a.rank - b.rank ||
      (b.recency || 0) - (a.recency || 0) ||
      a.title.localeCompare(b.title, undefined, { sensitivity: 'base' }) ||
      a.key.localeCompare(b.key),
  );
  return out;
}

function capGroup(group, expanded) {
  if (group.items.length <= CAP || expanded.has(group.id)) return group.items;
  const shown = group.items.slice(0, CAP);
  shown.push({
    key: `more:${group.id}`,
    kind: 'more',
    groupId: group.id,
    firstHiddenKey: group.items[CAP].key,
    type: 'Action',
    title: 'Show all',
    meta: `${group.items.length} ${group.noun}`,
    search: '',
    icon: 'chevron',
    recency: 0,
  });
  return shown;
}

function chatItem(c) {
  const mode = c.mode === 'standard' ? 'Standard' : 'Project';
  const title = c.title || 'Untitled';
  return {
    key: `chat:${c.id}`,
    kind: 'chat',
    id: c.id,
    type: 'Chat',
    title,
    meta: mode,
    search: mode,
    icon: 'chat',
    recency: Number(c.created) || 0,
  };
}

function threadItem(t, conversations) {
  const parent = conversations.find((c) => c.id === t.conversation_id);
  const parentTitle = parent?.title || 'Untitled chat';
  const device = deviceName(t.device_id);
  const model = t.model || 'No model';
  const status = threadStatus(t.status, { checkpoint: Boolean(t.checkpoint) });
  const statusSearch = t.status === 'waiting_device' ? `${status.word} waiting` : status.word;
  return {
    key: `thread:${t.id}`,
    kind: 'thread',
    id: t.id,
    conversationId: t.conversation_id,
    type: 'Thread',
    title: t.title || 'Thread',
    meta: `${parentTitle} · ${device} · ${model} · ${status.word}`,
    search: `${parentTitle} ${device} ${model} ${statusSearch}`,
    icon: 'threads',
    recency: Number(t.finished || t.created) || 0,
  };
}

function modelItem(m) {
  const provider = m.provider_id || 'Unknown provider';
  const availability = m.local ? 'Local' : 'Available';
  return {
    key: `model:${m.id}`,
    kind: 'model',
    id: m.id,
    type: 'Model',
    title: m.id || 'Model',
    meta: `${provider} · ${availability}`,
    search: `${provider} ${m.model || ''} ${availability} ${m.local ? 'local' : ''} ${m.free ? 'free' : ''}`.trim(),
    icon: 'models',
    recency: 0,
  };
}

function deviceItem(d) {
  const presence = d.online ? 'Online' : 'Offline';
  const platform = d.platform || 'Unknown platform';
  return {
    key: `device:${d.id}`,
    kind: 'device',
    id: d.id,
    type: 'Device',
    title: d.id === 'hub' ? 'Hub' : d.name || d.id || 'Device',
    meta: `${presence} · ${platform}`,
    search: `${presence} ${platform}`,
    icon: 'devices',
    recency: Number(d.last_seen) || 0,
  };
}

function pageItem(page, needsYou) {
  const count = page.id === 'inbox' ? needsYou : 0;
  const countText = !count ? '' : count === 1 ? '1 needs you' : `${count} need you`;
  const meta = page.id === 'inbox' ? (countText ? `${countText} · ${page.verb}` : page.verb) : page.verb;
  return {
    key: `page:${page.id}`,
    kind: 'page',
    id: page.id,
    hash: page.hash,
    type: 'Page',
    title: page.title,
    meta,
    search: `${page.verb} ${page.title} ${countText}`,
    icon: page.icon,
    recency: 0,
  };
}

function actionItem(action) {
  return {
    key: `action:${action.id}`,
    kind: 'action',
    id: action.id,
    mode: action.mode,
    type: 'Action',
    title: action.title,
    meta: action.verb,
    search: `${action.search} ${action.verb}`,
    icon: action.icon,
    recency: 0,
  };
}

function readRecents() {
  try {
    const raw = JSON.parse(localStorage.getItem(RECENTS_KEY) || '[]');
    if (!Array.isArray(raw)) return [];
    return raw.filter((key) => typeof key === 'string').slice(0, 5);
  } catch {
    return [];
  }
}

function writeRecent(key) {
  try {
    const next = [key, ...readRecents().filter((k) => k !== key)].slice(0, 5);
    localStorage.setItem(RECENTS_KEY, JSON.stringify(next));
  } catch {
    /* storage unavailable */
  }
}

/**
 * Build grouped palette rows from data already in memory.
 * `threads` is an array of thread records. `expanded` is a Set of group ids.
 * `recents` is a list of stable keys, newest first.
 */
export function buildPalette({
  query,
  conversations = [],
  threads = [],
  models = [],
  devices = [],
  needsYou = 0,
  theme = 'system',
  recents = [],
  expanded = new Set(),
  nested = false,
}) {
  const parsed = analyzeQuery(query);
  if (nested) {
    const choices = [
      { key: 'theme:back', kind: 'back', type: 'Action', title: 'Back', meta: 'Return to results', search: '', icon: 'back', recency: 0 },
      ...[
        ['system', 'System', 'Match the system appearance'],
        ['light', 'Light', 'Always use the light appearance'],
        ['dark', 'Dark', 'Always use the dark appearance'],
      ].map(([id, title, verb]) => ({
        key: `theme:${id}`,
        kind: 'theme',
        id,
        type: 'Action',
        title,
        meta: theme === id ? 'Current' : verb,
        search: '',
        icon: theme === id ? 'check' : 'settings',
        current: theme === id,
        recency: 0,
      })),
    ];
    return {
      groups: [{ id: 'theme', label: 'Appearance', noun: 'choices', items: choices }],
      flat: choices,
      emptyQuery: false,
      noMatches: false,
      tokens: [],
    };
  }

  const chats = conversations.map(chatItem);
  const threadRows = threads.map((t) => threadItem(t, conversations));
  const modelRows = models.map(modelItem);
  const deviceRows = devices.map(deviceItem);
  const pages = PAGES.map((page) => pageItem(page, needsYou));
  const actions = ACTIONS.map(actionItem);
  const byKey = new Map([...actions, ...chats, ...threadRows, ...modelRows, ...deviceRows, ...pages].map((item) => [item.key, item]));

  if (parsed.empty) {
    const recentItems = [];
    for (const key of recents) {
      const item = byKey.get(key);
      if (!item || recentItems.some((row) => row.key === key)) continue;
      recentItems.push({ ...item, meta: `${item.type} · ${item.meta}` });
      if (recentItems.length === 5) break;
    }
    const recentKeys = new Set(recentItems.map((item) => item.key));
    const groups = [];
    if (recentItems.length) groups.push({ id: 'recent', label: 'Recent', noun: 'recent', items: recentItems });
    const actionItems = actions.filter((item) => !recentKeys.has(item.key));
    const pageItems = pages.filter((item) => !recentKeys.has(item.key));
    if (actionItems.length) groups.push({ id: 'actions', label: 'Actions', noun: 'actions', items: actionItems });
    if (pageItems.length) groups.push({ id: 'pages', label: 'Pages', noun: 'pages', items: pageItems });
    const flat = groups.flatMap((group) => group.items);
    return { groups, flat, emptyQuery: true, noMatches: false, tokens: [] };
  }

  const specs = [
    { id: 'actions', label: 'Actions', noun: 'actions', items: byRank(actions, parsed) },
    { id: 'chats', label: 'Chats', noun: 'chats', items: byRank(chats, parsed) },
    { id: 'threads', label: 'Threads', noun: 'threads', items: byRank(threadRows, parsed) },
    { id: 'models', label: 'Models', noun: 'models', items: byRank(modelRows, parsed) },
    { id: 'devices', label: 'Devices', noun: 'devices', items: byRank(deviceRows, parsed) },
    { id: 'pages', label: 'Pages', noun: 'pages', items: byRank(pages, parsed) },
  ];
  const groups = specs
    .filter((group) => group.items.length)
    .map((group) => ({ ...group, items: capGroup(group, expanded) }));
  let flat = groups.flatMap((group) => group.items);
  let noMatches = false;
  if (!flat.length) {
    noMatches = true;
    const clear = {
      key: 'clear',
      kind: 'clear',
      type: 'Action',
      title: 'Clear search',
      meta: 'Show suggestions',
      search: '',
      icon: 'x',
      recency: 0,
    };
    groups.push({ id: 'clear', label: '', noun: 'actions', items: [clear] });
    flat = [clear];
  }
  return { groups, flat, emptyQuery: false, noMatches, tokens: parsed.tokens };
}

function optionDomId(key) {
  let out = 'palette-opt-';
  for (const ch of String(key)) {
    out += /[A-Za-z0-9_-]/.test(ch) ? ch : `_${ch.codePointAt(0).toString(16)}_`;
  }
  return out;
}

function optionLabel(item) {
  return item.meta ? `${item.type}, ${item.title}, ${item.meta}` : `${item.type}, ${item.title}`;
}

function currentTheme() {
  try {
    return localStorage.getItem(THEME_KEY) || 'system';
  } catch {
    return 'system';
  }
}

function applyAppearance(value) {
  try {
    if (value === 'system') localStorage.removeItem(THEME_KEY);
    else localStorage.setItem(THEME_KEY, value);
  } catch {
    /* storage unavailable */
  }
  if (value === 'system') document.documentElement.removeAttribute('data-theme');
  else document.documentElement.dataset.theme = value;
  for (const input of document.querySelectorAll('input[name="theme"]')) {
    if (input instanceof HTMLInputElement) input.checked = input.value === value;
  }
}

function pagePath(hash) {
  const raw = String(hash || '').replace(/^#\/?/, '');
  return raw.split('?')[0];
}

function go(hash) {
  if (location.hash === hash) window.dispatchEvent(new HashChangeEvent('hashchange'));
  else location.hash = hash;
}

let dialog = null;
let input = null;
let list = null;
let emptyEl = null;
let loadingEl = null;
let liveEl = null;
let opener = null;
let savedQuery = '';
let pageKey = '';
let nested = false;
let activeKey = null;
let expanded = new Set();
let currentFlat = [];
let byKey = new Map();
let tokens = [];
let lastSpoken = '';
let speakToken = 0;
let urgent = '';
let renderFrame = 0;
const seen = { conversations: false, threads: false, models: false, devices: false };

function pendingLabels() {
  const labels = [];
  if (!seen.conversations) labels.push('chats');
  if (!seen.threads) labels.push('threads');
  if (!seen.models) labels.push('models');
  if (!seen.devices) labels.push('devices');
  return labels;
}

function ownsFocus() {
  const el = document.activeElement;
  return Boolean(dialog?.open && el && dialog.contains(el));
}

function setLaunchExpanded(open) {
  for (const btn of document.querySelectorAll('.palette-launch')) {
    btn.setAttribute('aria-expanded', open ? 'true' : 'false');
  }
}

function closePalette({ restore = true, discardQuery = false } = {}) {
  if (!dialog?.open) return;
  if (discardQuery) savedQuery = '';
  else savedQuery = input.value;
  const target = opener;
  opener = null;
  nested = false;
  dialog.close();
  setLaunchExpanded(false);
  if (restore && target?.isConnected && !dialog.contains(target)) target.focus({ preventScroll: true });
}

function focusDestination(kind) {
  const pick = () => (kind === 'thread' ? document.getElementById('thread-title') : document.querySelector('#main h1'));
  // Chat rebuilds its bar on conversation updates, which drops the tabindex the
  // shell set on the first heading. A heading without it cannot take focus.
  const place = () => {
    if (kind !== 'chat' && kind !== 'thread') {
      const active = document.activeElement;
      const main = document.getElementById('main');
      const inspector = document.querySelector('.inspector');
      if (active && active !== document.body && (main?.contains(active) || inspector?.contains(active))) return null;
    }
    const el = pick();
    if (!el) return null;
    if (!el.hasAttribute('tabindex')) el.setAttribute('tabindex', '-1');
    if (document.activeElement !== el) el.focus({ preventScroll: true });
    return el;
  };
  // Dialog close restores the opener on the next frame, and a new chat focuses
  // its composer there too. The following frame puts the heading back on top.
  requestAnimationFrame(() => {
    requestAnimationFrame(() => {
      const el = place();
      if (!el || (kind !== 'chat' && kind !== 'thread')) return;
      const root = document.getElementById('main');
      if (!root) return;
      const observer = new MutationObserver(() => {
        if (el.isConnected && document.activeElement === el) return;
        place();
      });
      observer.observe(root, { childList: true, subtree: true });
      setTimeout(() => observer.disconnect(), 400);
    });
  });
}

function stillThere(item) {
  if (item.kind === 'chat') return store.conversations.some((c) => c.id === item.id);
  if (item.kind === 'thread') return store.threads.has(item.id);
  if (item.kind === 'model') return store.models.some((m) => m.id === item.id);
  if (item.kind === 'device') return store.devices.some((d) => d.id === item.id);
  return true;
}

function remember(item) {
  if (item.key && item.kind !== 'more' && item.kind !== 'clear' && item.kind !== 'back' && item.kind !== 'theme') writeRecent(item.key);
}

function activate(item) {
  if (!item) return;
  if (item.kind === 'more') {
    expanded.add(item.groupId);
    activeKey = item.firstHiddenKey || item.key;
    render();
    return;
  }
  if (item.kind === 'clear') {
    input.value = '';
    nested = false;
    expanded = new Set();
    activeKey = null;
    urgent = '';
    render();
    input.focus();
    return;
  }
  if (item.kind === 'back') {
    nested = false;
    activeKey = 'action:theme';
    render();
    input.focus();
    return;
  }
  if (item.kind === 'action' && item.id === 'theme') {
    nested = true;
    activeKey = 'theme:back';
    render();
    input.focus();
    return;
  }
  if (item.kind === 'theme') {
    applyAppearance(item.id);
    input.value = '';
    savedQuery = '';
    nested = false;
    expanded = new Set();
    activeKey = null;
    urgent = `Appearance set to ${item.title}`;
    render();
    input.focus();
    return;
  }
  if (!stillThere(item)) {
    urgent = 'That item is no longer available';
    activeKey = null;
    render();
    input.focus();
    return;
  }
  let threadConversation = item.conversationId;
  if (item.kind === 'thread') {
    threadConversation = threadConversation || store.threads.get(item.id)?.conversation_id;
    if (!threadConversation) {
      urgent = 'That item is no longer available';
      activeKey = null;
      render();
      input.focus();
      return;
    }
  }
  remember(item);
  savedQuery = '';
  closePalette({ restore: false, discardQuery: true });
  if (item.kind === 'chat') {
    go(`#/chat/${enc(item.id)}`);
    focusDestination('chat');
    return;
  }
  if (item.kind === 'thread') {
    go(`#/chat/${enc(threadConversation)}/thread/${enc(item.id)}`);
    focusDestination('thread');
    return;
  }
  if (item.kind === 'model') {
    go(`#/models/${enc(item.id)}`);
    focusDestination('model');
    return;
  }
  if (item.kind === 'device') {
    go(`#/devices/${enc(item.id)}`);
    focusDestination('device');
    return;
  }
  if (item.kind === 'page') {
    go(item.hash);
    focusDestination('page');
    return;
  }
  if (item.kind === 'action') {
    if (item.mode === 'project' || item.mode === 'standard') {
      try {
        localStorage.setItem(MODE_KEY, item.mode);
      } catch {
        /* storage unavailable */
      }
      if (pagePath(location.hash) === 'chat/new') window.dispatchEvent(new Event('grouper-palette-remount-chat'));
    }
    go('#/chat/new');
    focusDestination('chat');
  }
}

function paintHighlight(el, text, queryTokens) {
  if (!queryTokens.length) {
    setText(el, text);
    return;
  }
  const parts = highlightParts(text, queryTokens);
  if (parts.length === 1 && !parts[0].match) {
    setText(el, text);
    return;
  }
  setChildren(
    el,
    parts.map((part) => (part.match ? h('mark', { class: 'palette-mark' }, part.text) : part.text)),
  );
}

function createGroup() {
  const label = h('div', { class: 'palette-group' });
  const items = h('div', { class: 'palette-group-items', role: 'presentation' });
  return h('div', { class: 'palette-group-wrap', role: 'group' }, label, items);
}

function createOption(item) {
  const glyph = h('span', { class: 'palette-icon' }, icon(item.icon));
  const title = h('span', { class: 'palette-title' });
  const meta = h('span', { class: 'palette-meta' });
  const el = h(
    'div',
    {
      role: 'option',
      class: 'palette-option',
      id: optionDomId(item.key),
      onclick: () => activate(byKey.get(el.dataset.key)),
      onmousemove: () => {
        const key = el.dataset.key;
        if (!key || key === activeKey) return;
        activeKey = key;
        syncSelection({ scroll: false });
      },
    },
    glyph,
    title,
    meta,
  );
  return el;
}

function updateOption(el, item) {
  if (el.id !== optionDomId(item.key)) el.id = optionDomId(item.key);
  if (el.dataset.icon !== item.icon) {
    el.dataset.icon = item.icon;
    setChildren(el.querySelector('.palette-icon'), icon(item.icon));
  }
  paintHighlight(el.querySelector('.palette-title'), item.title, tokens);
  const meta = el.querySelector('.palette-meta');
  meta.hidden = !item.meta;
  if (item.meta) paintHighlight(meta, item.meta, tokens);
  else setText(meta, '');
  setAttr(el, 'aria-label', optionLabel(item));
  setAttr(el, 'aria-current', item.current ? 'true' : null);
}

function syncSelection({ scroll }) {
  for (const item of currentFlat) {
    const el = document.getElementById(optionDomId(item.key));
    if (!el) continue;
    setAttr(el, 'aria-selected', item.key === activeKey ? 'true' : 'false');
  }
  const activeId = activeKey ? optionDomId(activeKey) : null;
  setAttr(input, 'aria-activedescendant', activeId);
  setAttr(list, 'aria-activedescendant', activeId);
  if (scroll && activeId) document.getElementById(activeId)?.scrollIntoView({ block: 'nearest' });
}

function statusMessage(result) {
  const pending = pendingLabels();
  const loading = pending.length ? `Loading ${pending.join(', ')}.` : '';
  let count;
  if (nested) count = `${result.flat.length} choices`;
  else if (result.noMatches) count = 'No matches';
  else if (result.emptyQuery) count = result.flat.length === 1 ? '1 suggestion' : `${result.flat.length} suggestions`;
  else count = result.flat.length === 1 ? '1 result' : `${result.flat.length} results`;
  const selected = activeKey ? result.flat.find((item) => item.key === activeKey) : null;
  return [urgent, count, selected ? optionLabel(selected) : '', loading].filter(Boolean).join('. ');
}

function speakLive() {
  if (!liveEl || !dialog?.open) return;
  const msg = statusMessage({
    flat: currentFlat,
    emptyQuery: analyzeQuery(input.value).empty && !nested,
    noMatches: !emptyEl.hidden,
  });
  urgent = '';
  if (msg === lastSpoken) return;
  lastSpoken = msg;
  const token = ++speakToken;
  liveEl.textContent = '';
  setTimeout(() => {
    if (token !== speakToken || !liveEl || !dialog?.open) return;
    liveEl.textContent = msg;
  }, 30);
}

function render() {
  if (!dialog?.open) return;
  const result = buildPalette({
    query: input.value,
    conversations: store.conversations,
    threads: [...store.threads.values()],
    models: store.models,
    devices: store.devices,
    needsYou: needsYouCount(),
    theme: currentTheme(),
    recents: readRecents(),
    expanded,
    nested,
  });
  tokens = result.tokens;
  currentFlat = result.flat;
  byKey = new Map(result.flat.map((item) => [item.key, item]));
  if (!(activeKey && result.flat.some((item) => item.key === activeKey))) {
    activeKey = !result.emptyQuery && result.flat.length ? result.flat[0].key : null;
  }
  emptyEl.hidden = !result.noMatches;
  const pending = pendingLabels();
  loadingEl.hidden = pending.length === 0;
  setText(loadingEl, pending.length ? `Loading ${pending.join(', ')}.` : '');
  setAttr(list, 'aria-label', nested ? 'Appearance' : 'Results');
  syncList(
    list,
    result.groups,
    (group) => group.id,
    createGroup,
    (node, group) => {
      const label = node.querySelector('.palette-group');
      label.id = `palette-g-${group.id}`;
      label.hidden = !group.label;
      setText(label, group.label);
      setAttr(node, 'role', group.label ? 'group' : 'presentation');
      setAttr(node, 'aria-label', group.label || null);
      setAttr(node, 'aria-labelledby', group.label ? label.id : null);
      syncList(node.querySelector('.palette-group-items'), group.items, (item) => item.key, createOption, updateOption);
    },
  );
  syncSelection({ scroll: false });
  speakLive();
}

function scheduleRender() {
  if (!dialog?.open || renderFrame) return;
  renderFrame = requestAnimationFrame(() => {
    renderFrame = 0;
    render();
  });
}

function move(delta) {
  if (!currentFlat.length) return;
  const index = currentFlat.findIndex((item) => item.key === activeKey);
  let next;
  if (index < 0) next = delta > 0 ? 0 : currentFlat.length - 1;
  else next = Math.max(0, Math.min(currentFlat.length - 1, index + delta));
  activeKey = currentFlat[next].key;
  syncSelection({ scroll: true });
  speakLive();
}

function jump(end) {
  if (!currentFlat.length) return;
  activeKey = currentFlat[end ? currentFlat.length - 1 : 0].key;
  syncSelection({ scroll: true });
  speakLive();
}

function onPaletteKey(e) {
  if (e.key === 'Escape') {
    e.preventDefault();
    e.stopPropagation();
    closePalette({ restore: true });
    return;
  }
  if (e.key === 'Tab') return;
  const inQuery = e.target === input;
  const inList = e.target === list;
  if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
    if (!inQuery && !inList) return;
    e.preventDefault();
    move(e.key === 'ArrowDown' ? 1 : -1);
    return;
  }
  if ((e.key === 'Home' || e.key === 'End') && inList) {
    e.preventDefault();
    jump(e.key === 'End');
    return;
  }
  if (e.key === 'Enter' && (inQuery || inList)) {
    e.preventDefault();
    if (!activeKey) return;
    activate(byKey.get(activeKey));
    return;
  }
  if (inList && e.key.length === 1 && !e.ctrlKey && !e.metaKey && !e.altKey && !e.isComposing) {
    e.preventDefault();
    input.focus();
    input.value += e.key;
    input.dispatchEvent(new Event('input', { bubbles: true }));
  }
}

function onShortcut(e) {
  if (e.isComposing || e.keyCode === 229 || e.repeat) return;
  if (e.key.toLowerCase() !== 'k' || e.altKey || e.shiftKey) return;
  if (!e.metaKey && !e.ctrlKey) return;
  if (e.metaKey && e.ctrlKey) return;
  if (isPhone()) return;
  const el = document.activeElement;
  if (el?.isContentEditable) return;
  const openDialog = document.querySelector('dialog[open]');
  if (openDialog && openDialog !== dialog) return;
  if (!dialog) return;
  e.preventDefault();
  e.stopPropagation();
  if (dialog.open && ownsFocus()) closePalette({ restore: true });
  else openPalette();
}

/** Open the one palette. A second call focuses the query; it does not stack. */
export function openPalette() {
  if (isPhone() || !dialog || !input) return;
  if (dialog.open) {
    input.focus();
    return;
  }
  opener = document.activeElement instanceof HTMLElement ? document.activeElement : null;
  input.value = savedQuery;
  nested = false;
  expanded = new Set();
  activeKey = null;
  lastSpoken = '';
  dialog.showModal();
  setLaunchExpanded(true);
  render();
  input.focus();
  const end = input.value.length;
  try {
    input.setSelectionRange(end, end);
  } catch {
    /* some input types refuse a range */
  }
}

/** Install the shortcut, the dialog and store listeners for this sign-in. */
export function mountPalette(signal) {
  if (dialog) {
    closePalette({ restore: false, discardQuery: true });
    dialog.remove();
    dialog = null;
  }
  seen.conversations = store.conversations.length > 0;
  seen.threads = store.threadsLoaded;
  seen.models = store.models.length > 0;
  seen.devices = store.devicesAt > 0;
  pageKey = pagePath(location.hash);
  const shortcut = paletteShortcut();

  input = h('input', {
    id: 'palette-query',
    class: 'palette-query',
    type: 'text',
    role: 'combobox',
    autocomplete: 'off',
    spellcheck: 'false',
    autocapitalize: 'off',
    autocorrect: 'off',
    'aria-autocomplete': 'list',
    'aria-expanded': 'true',
    'aria-controls': 'palette-list',
    'aria-label': 'Search chats, threads, models, devices, pages, actions',
    placeholder: 'Search chats, threads, models, devices, pages, actions',
    oninput: () => {
      nested = false;
      expanded = new Set();
      activeKey = null;
      render();
    },
  });
  list = h('div', { id: 'palette-list', class: 'palette-list', role: 'listbox', tabindex: '0', 'aria-label': 'Results' });
  emptyEl = h('p', { class: 'palette-empty', hidden: true }, 'No matches');
  loadingEl = h('p', { class: 'palette-loading', hidden: true });
  liveEl = h('div', { class: 'visually-hidden', 'aria-live': 'polite', 'aria-atomic': 'true' });
  const kbd = h('kbd', { class: 'palette-kbd', 'aria-hidden': 'true' }, shortcut.label);
  const closeBtn = h('button', { type: 'button', class: 'icon-btn palette-close', 'aria-label': 'Close', onclick: () => closePalette({ restore: true }) }, icon('close'));
  const frame = h(
    'div',
    { class: 'palette-frame' },
    h('div', { class: 'palette-search' }, icon('search'), input, kbd),
    closeBtn,
    h('div', { class: 'palette-scroll' }, emptyEl, list, loadingEl),
    liveEl,
  );
  dialog = h('dialog', {
    id: 'command-palette',
    class: 'command-palette',
    'aria-labelledby': 'palette-title',
    onkeydown: onPaletteKey,
    onclick: (e) => {
      if (e.target === dialog) closePalette({ restore: true });
    },
    oncancel: (e) => {
      e.preventDefault();
      closePalette({ restore: true });
    },
  });
  dialog.append(
    h('h2', { id: 'palette-title', class: 'visually-hidden' }, 'Command palette'),
    frame,
  );
  document.body.append(dialog);

  const offs = [
    on('conversations', () => {
      seen.conversations = true;
      scheduleRender();
    }),
    on('threads', () => {
      seen.threads = true;
      scheduleRender();
    }),
    on('models', () => {
      seen.models = true;
      scheduleRender();
    }),
    on('devices', () => {
      seen.devices = true;
      scheduleRender();
    }),
    on('approvals', scheduleRender),
  ];

  document.addEventListener('keydown', onShortcut, { capture: true, signal });
  window.addEventListener(
    'hashchange',
    () => {
      const next = pagePath(location.hash);
      if (next === pageKey) return;
      pageKey = next;
      savedQuery = '';
      if (input) input.value = '';
      nested = false;
      if (dialog?.open) closePalette({ restore: false, discardQuery: true });
    },
    { signal },
  );
  const phone = typeof matchMedia === 'function' ? matchMedia(PHONE) : null;
  const onPhone = () => {
    if (phone.matches) closePalette({ restore: true });
  };
  phone?.addEventListener?.('change', onPhone);
  signal.addEventListener('abort', () => phone?.removeEventListener?.('change', onPhone));

  signal.addEventListener('abort', () => {
    offs.forEach((off) => off());
    if (renderFrame) cancelAnimationFrame(renderFrame);
    renderFrame = 0;
    closePalette({ restore: false, discardQuery: true });
    dialog?.remove();
    dialog = null;
    input = null;
    list = null;
    liveEl = null;
  });
}
