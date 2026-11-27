// Usage: two summaries on one page (usage-tab-spec 3-5), kept apart and never added
// together.
//   1. API costs: a donut by API key, a Day / Week / Month meter, the key list (the
//      legend) with the models under each key, tokens, and what each provider itself
//      reports, in the key's expanded row.
//   2. Subscriptions: one card per signed-in plan account, with whatever windows the
//      plan reports, read on the devices (unofficial snapshots).
// `#/usage/breakdown` and `#/usage/limits` are the old breakdown table and limits editor
// (views/usage-breakdown.js, views/usage-limits.js). `#/usage?provider=<id>` opens with that
// key focused. `#/usage?conversation=<id>` scopes section 1 to one chat and adds that chat's
// threads table; section 2 is hidden there.

import { h, icon, setChildren, setText, keepFocus, throttle } from '../lib/dom.js';
import { post, qs, enc, isAbort } from '../lib/api.js';
import { store, on, isAdmin, deviceName } from '../lib/store.js';
import { loadDevices } from '../lib/live.js';
import { bar, toast, toastError, skeleton, guardWrites, isOffline } from '../lib/ui.js';
import { num, usd, dateTime, plural, abbr, tokensIn, tokensOut } from '../lib/format.js';
import { familyKey, familyMark } from '../lib/family.js';
import { harnessSignInInfo } from './model-picker.js';
import { renderUsageBreakdown, createBreakdown } from './usage-breakdown.js';
import { renderUsageLimits } from './usage-limits.js';
import { colorPicker, colorUse, keyClass, keySwatch, keyColorLabel, setProviderColor } from './key-color.js';
import { load, nowSecs, est, slug, balanceApi, isOpenRouter, isFreeAllowance, isLocalHost, hostOf, sourceProblem, ageLine, reportedRow, freeAllowance, STALE_SECS } from './usage-common.js';

const PERIODS = [
  ['day', 'Day', 'Today', 'today'],
  ['week', 'Week', 'Last 7 days', 'in the last 7 days'],
  ['month', 'Month', 'Last 30 days', 'in the last 30 days'],
];
const WHOLE_CHAT = ['all', 'Whole chat', 'Whole chat', 'in this chat'];
const PERIOD_STORE = 'grouper.usagePeriod';
const MAX_SEGMENTS = 8; // paid keys drawn before the rest become "Other"
const MIN_ARC = 3 / 360; // a paid key always gets at least 3 degrees
const MODELS_SHOWN = 6;

const SVG_NS = 'http://www.w3.org/2000/svg';
function svg(tag, attrs, ...children) {
  const el = document.createElementNS(SVG_NS, tag);
  for (const [k, v] of Object.entries(attrs || {})) if (v !== null && v !== undefined && v !== false) el.setAttribute(k, String(v));
  for (const c of children.flat()) if (c) el.append(c instanceof Node ? c : document.createTextNode(String(c)));
  return el;
}

export function renderUsage(container, route = {}) {
  if (route.sub === 'breakdown') return renderUsageBreakdown(container, route);
  if (route.sub === 'limits') return renderUsageLimits(container, route);
  return renderUsageSummary(container, route);
}

// ---- period

/** Local-midnight boundaries (usage-tab-spec 4.1): rolling, in the device's own time zone. */
export function periodSince(key, now = new Date()) {
  const back = key === 'week' ? 6 : key === 'month' ? 29 : 0;
  return Math.floor(new Date(now.getFullYear(), now.getMonth(), now.getDate() - back).getTime() / 1000);
}

function storedPeriod() {
  try {
    const v = localStorage.getItem(PERIOD_STORE);
    return PERIODS.some(([k]) => k === v) ? v : 'day';
  } catch {
    return 'day';
  }
}

// ---- small helpers

const pctText = (share) => (share > 0 && share < 0.01 ? '<1%' : `${Math.round(share * 100)}%`);

function inWords(n) {
  return h('span', { title: num(n) }, h('span', { 'aria-hidden': 'true' }, abbr(n)), h('span', { class: 'visually-hidden' }, num(n)));
}

/** "3 min ago" (the Usage page spells it out). */
function agoLong(unix) {
  const s = Math.max(0, Math.round(nowSecs() - unix));
  if (s < 45) return 'just now';
  if (s < 3600) return `${Math.max(1, Math.round(s / 60))} min ago`;
  if (s < 86400) return `${Math.round(s / 3600)} h ago`;
  return `${Math.round(s / 86400)} d ago`;
}

const clockOf = (secs) => new Date(secs * 1000).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });

/** "resets in 38 min", "resets in 2h 14m", "resets Thu 3 Dec, 09:00" (spec 5.2). */
function resetWords(resetsAt, { long = false } = {}) {
  const s = Math.round(resetsAt - nowSecs());
  if (s < 3600) {
    const m = Math.max(1, Math.round(s / 60));
    return long ? `resets in ${m} ${m === 1 ? 'minute' : 'minutes'}` : `resets in ${m} min`;
  }
  if (s < 86400) {
    const hh = Math.floor(s / 3600);
    const mm = Math.floor((s % 3600) / 60);
    return long ? `resets in ${hh} ${hh === 1 ? 'hour' : 'hours'} ${mm} ${mm === 1 ? 'minute' : 'minutes'}` : `resets in ${hh}h ${String(mm).padStart(2, '0')}m`;
  }
  return `resets ${new Date(resetsAt * 1000).toLocaleString([], { weekday: 'short', day: 'numeric', month: 'short', hour: '2-digit', minute: '2-digit' })}`;
}

// ---- subscription naming

const PLAN_PROVIDERS = {
  claude: { name: 'Claude', fam: 'claude', app: 'Claude Code' },
  codex: { name: 'ChatGPT', fam: 'gpt', app: 'Codex' },
  grok: { name: 'Grok', fam: 'grok', app: 'Grok' },
  antigravity: { name: 'Antigravity', fam: 'gemini', app: 'Antigravity' },
};
const ACCOUNT_NAMES = { supergrok: 'SuperGrok' };

const providerName = (p) => PLAN_PROVIDERS[p]?.name || p;

/** "Claude Max", "ChatGPT Plus", "SuperGrok", "Antigravity". */
function planTitle(s) {
  const provider = providerName(s.provider);
  const acct = String(s.account || '').toLowerCase();
  if (ACCOUNT_NAMES[acct]) return ACCOUNT_NAMES[acct];
  const prefix = s.provider === 'codex' ? 'chatgpt' : s.provider;
  let plan = acct;
  if (acct.startsWith(`${prefix}-`)) plan = acct.slice(prefix.length + 1);
  else if (acct === prefix || acct === s.provider || !acct) plan = '';
  plan = plan.replace(/[_-]+/g, ' ').replace(/\b\w/g, (c) => c.toUpperCase());
  return plan ? `${provider} ${plan}` : provider;
}

const SPAN_SECS = { m: 60, h: 3600, d: 86400 };

function parseWindow(w) {
  const [base, ...rest] = String(w || '').split(':');
  const m = /^(\d+)([mhd])$/.exec(base);
  return { span: m ? Number(m[1]) * SPAN_SECS[m[2]] : Infinity, raw: base, n: m ? Number(m[1]) : 0, unit: m ? m[2] : '', scope: rest.join(':') };
}

/** `5h` -> "5-hour", `7d` -> "Weekly", `30d` -> "Monthly", `3h` -> "3-hour", `14d` -> "14-day". */
function spanLabel(p) {
  if (!p.unit) return p.raw || 'Window';
  if (p.unit === 'd' && p.n === 7) return 'Weekly';
  if (p.unit === 'd' && p.n === 30) return 'Monthly';
  return `${p.n}-${{ m: 'minute', h: 'hour', d: 'day' }[p.unit]}`;
}

const titleCase = (s) => String(s).replace(/[_-]+/g, ' ').replace(/\b\w/g, (c) => c.toUpperCase());

/** Order inside a card or group: windows before spend and balance, plain before scoped, shortest first. */
function rowOrder(r) {
  const p = parseWindow(r.window);
  return [r.kind === 'window' ? 0 : 1, p.scope ? 1 : 0, p.span, p.scope, String(r.window)];
}

function compareTuples(a, b) {
  for (let i = 0; i < a.length; i++) {
    if (a[i] < b[i]) return -1;
    if (a[i] > b[i]) return 1;
  }
  return 0;
}

/** The probe provider a harness provider reads through (claude-code -> claude). */
function probeProviderOf(harness) {
  const slugName = String(harness || '').toLowerCase();
  if (slugName === 'claude' || slugName === 'claude-code') return 'claude';
  if (slugName === 'codex') return 'codex';
  if (slugName === 'grok') return 'grok';
  if (slugName === 'antigravity' || slugName === 'agy') return 'antigravity';
  return null;
}

// ---- the page

function renderUsageSummary(container, { conversation: filterCid = null, provider: routeProvider = null } = {}) {
  const admin = isAdmin();
  const barSlot = h('div', { class: 'bar-slot' });
  const body = h('div', { class: 'view-body u-body us-page' });
  container.append(h('section', { class: 'view' }, barSlot, body));

  const chatTitle = () => (filterCid ? store.conversations.find((c) => c.id === filterCid)?.title || 'Untitled chat' : null);

  // Refresh: asks the hub to read provider balances and device plan windows now.
  let refreshing = false;
  const refreshText = h('span', null, 'Refresh');
  const refreshBtn = h('button', { class: 'btn btn-sm', type: 'button', onclick: () => doRefresh() }, icon('refresh'), refreshText);
  const syncRefresh = () => {
    refreshBtn.disabled = refreshing || isOffline();
    setText(refreshText, refreshing ? 'Refreshing…' : 'Refresh');
    refreshBtn.setAttribute('aria-busy', refreshing ? 'true' : 'false');
  };

  function renderHeader() {
    if (filterCid) {
      const name = chatTitle();
      setChildren(
        barSlot,
        bar({ title: 'Chat usage', caption: name, back: `#/chat/${enc(filterCid)}`, actions: [h('a', { class: 'btn btn-sm', href: '#/usage' }, 'All usage')] }),
      );
      document.title = `${name} · Usage · Grouper`;
    } else {
      setChildren(barSlot, bar({ title: 'Usage', actions: [refreshBtn] }));
      document.title = 'Usage · Grouper';
    }
  }
  renderHeader();
  syncRefresh();

  if (!admin) {
    body.append(h('div', { class: 'banner warn' }, icon('shield'), 'Usage and limits need an admin token. Sign in with one in Settings.'));
    return null;
  }

  // Leaving the view cancels its reads; a write in flight still finishes.
  const aborter = new AbortController();
  const { signal } = aborter;
  const unguard = guardWrites(body);

  // ---- state
  let periodKey = filterCid ? 'all' : storedPeriod();
  let focusKey = routeProvider || null; // the focused (and expanded) key
  let pickerFor = null; // the key whose colour picker is open
  const showAllModels = new Set();
  const st = { summary: null, providers: null, snaps: null, free: null, limits: null };
  const periods = filterCid ? [...PERIODS, WHOLE_CHAT] : PERIODS;
  const period = () => periods.find(([k]) => k === periodKey) || periods[0];

  function summaryPath() {
    const q = { since: periodKey === 'all' ? 0 : periodSince(periodKey), conversation: filterCid };
    return `/v1/usage/summary${qs(q)}`;
  }

  // ---- layout skeleton (built once; each section redraws its own body in place)
  const skelRing = () => h('div', { class: 'us-skel', 'aria-hidden': 'true' }, h('div', { class: 'us-skel-ring' }), skeleton(3));
  const apiBody = h('div', { class: 'us-body', 'aria-busy': 'true' }, skelRing());
  const subBody = h('div', { class: 'us-body', 'aria-busy': 'true' }, skelRing());

  const apiCard = h(
    'section',
    { class: 'us-card', 'aria-labelledby': 'us-api-h' },
    h('h2', { class: 'us-title', id: 'us-api-h' }, 'API costs'),
    h('p', { class: 'us-sub' }, filterCid ? 'Estimated API cost of this chat and its threads.' : 'Estimated from calls made through Grouper.'),
    apiBody,
  );
  const subCard = h(
    'section',
    { class: 'us-card', 'aria-labelledby': 'us-sub-h' },
    h('h2', { class: 'us-title', id: 'us-sub-h' }, 'Subscriptions'),
    h('p', { class: 'us-sub' }, 'Read from the apps signed in on your devices. Unofficial; may lag.'),
    subBody,
  );

  let breakdown = null;
  if (filterCid) {
    body.append(
      h(
        'div',
        { class: 'banner info' },
        icon('chat'),
        h('div', { class: 'grow' }, 'Subscription quotas aren’t tracked per chat. '),
        h('a', { class: 'btn btn-sm', href: `#/chat/${enc(filterCid)}` }, icon('back'), 'Back to chat'),
      ),
      h('div', { class: 'us-cols us-cols-one' }, apiCard),
    );
    // That chat's threads table (the old per-chat breakdown, threads first).
    breakdown = createBreakdown({ filterCid, signal, base: '#/usage', group: 'thread' });
    body.append(h('div', { class: 'us-chat-breakdown' }, breakdown.toolbar, breakdown.chatStats, breakdown.section));
  } else {
    body.append(
      h('div', { class: 'us-cols' }, apiCard, subCard),
      h(
        'section',
        { class: 'us-more', 'aria-labelledby': 'us-more-h' },
        h('h2', { class: 'us-more-title', id: 'us-more-h' }, 'More'),
        h(
          'ul',
          { class: 'list card' },
          moreRow('#/usage/breakdown', 'Breakdown', 'By provider, model, chat, thread or day'),
          moreRow('#/usage/limits', 'Limits and budgets', 'Budgets, thread caps and timeouts'),
        ),
        aboutNumbers(),
      ),
    );
  }

  function moreRow(href, title, sub) {
    return h('li', null, h('a', { class: 'list-row', href }, h('div', { class: 'grow' }, h('div', { class: 'row-title' }, title), h('div', { class: 'row-sub' }, sub)), icon('chevron')));
  }

  function aboutNumbers() {
    return h(
      'details',
      { class: 'us-about card' },
      h('summary', null, icon('chevron'), 'Where these numbers come from'),
      h(
        'dl',
        null,
        h('dt', null, h('span', { class: 'badge u-est' }, 'estimated')),
        h('dd', null, 'Tokens from calls made through Grouper times the registry’s prices at the time of the call. Calls made elsewhere are not seen. API costs on this page never include subscriptions.'),
        h('dt', null, h('span', { class: 'badge' }, 'reported by provider')),
        h('dd', null, 'Read from the provider’s own API with the key Grouper holds (OpenRouter and DeepSeek). May include use outside Grouper, so it is shown inside the key’s row and never added to the total.'),
        h('dt', null, 'Free requests'),
        h('dd', null, 'OpenRouter’s daily free-model limit, as its last “limit reached” answer to Grouper reported it. Unknown until a call runs into it.'),
        h('dt', null, h('span', { class: 'badge warn' }, icon('warning'), 'Unofficial')),
        h('dd', null, 'Subscription windows are read on a device from the Claude Code, Codex, Grok or Antigravity sign-in, through endpoints that aren’t a published API. They may change or stop working; never a guaranteed limit.'),
        h('dt', null, h('span', { class: 'badge warn' }, 'Stale')),
        h('dd', null, 'Read more than an hour ago. The last value is kept, with its time.'),
        h('dt', null, 'Daily budget'),
        h('dd', null, 'The hub counts a budget in UTC days, so it is shown on its own line and is not the Day total.'),
      ),
    );
  }

  // ---- loading

  let summarySeq = 0;
  async function loadSummary() {
    const seq = ++summarySeq;
    apiBody.setAttribute('aria-busy', 'true');
    let res;
    try {
      res = await load(summaryPath(), signal);
    } catch (err) {
      if (isAbort(err)) return;
      throw err;
    }
    if (seq !== summarySeq) return; // a newer period or refresh won
    st.summary = res;
    drawApi();
    drawSubs(); // "Through Grouper" lines follow the period
  }

  async function loadProviders() {
    try {
      st.providers = await load('/v1/providers', signal);
    } catch (err) {
      if (isAbort(err)) return;
      throw err;
    }
    drawApi();
    drawSubs();
  }

  async function loadSnaps() {
    try {
      st.snaps = await load('/v1/usage/snapshots', signal);
    } catch (err) {
      if (isAbort(err)) return;
      throw err;
    }
    drawApi();
    drawSubs();
  }

  async function loadFree() {
    try {
      st.free = await load('/v1/usage/free-requests', signal);
    } catch (err) {
      if (isAbort(err)) return;
      throw err;
    }
    drawApi();
  }

  async function loadLimits() {
    if (filterCid) return;
    try {
      st.limits = await load('/v1/limits', signal);
    } catch (err) {
      if (isAbort(err)) return;
      throw err;
    }
    drawApi();
  }

  function loadAll() {
    loadSummary();
    loadProviders();
    loadSnaps();
    loadFree();
    loadLimits();
    loadDevices().then(drawSubs, () => {});
  }

  async function doRefresh() {
    if (refreshing || isOffline()) return;
    refreshing = true;
    syncRefresh();
    try {
      const r = await post('/v1/usage/refresh');
      toast(`Checked ${plural(r?.providers_checked || 0, 'provider key')} and ${plural(r?.devices_probed || 0, 'device')}; ${plural(r?.snapshots_stored || 0, 'new reading')}.`);
    } catch (err) {
      toastError(err);
    }
    refreshing = false;
    syncRefresh();
    if (!signal.aborted) loadAll();
  }

  // ---- section 1: API costs

  /** The key rows' data: the summary's keys with the provider record and snapshots joined on. */
  function keyRows() {
    const S = st.summary?.data;
    if (!S) return null;
    const providers = Array.isArray(st.providers?.data) ? st.providers.data : [];
    const byId = new Map(providers.map((p) => [p.id, p]));
    const snaps = (Array.isArray(st.snaps?.data) ? st.snaps.data : []).filter((s) => s.source !== 'probe');
    const rows = (S.keys || []).map((k) => {
      const info = byId.get(k.provider) || null;
      const color = byId.has(k.provider) ? (info.color ?? k.color) : k.color;
      const t = k.totals || {};
      return { ...k, color: k.removed ? null : color, info, totals: t, tokens: tokensIn(t) + tokensOut(t), snaps: snaps.filter((s) => s.provider === k.provider) };
    });
    rows.sort((a, b) => (b.totals.cost || 0) - (a.totals.cost || 0) || b.tokens - a.tokens || a.provider.localeCompare(b.provider));
    return rows;
  }

  const keyName = (k) => (k.removed ? `Removed key (${k.provider})` : k.provider);

  /** What the donut draws: cost shares, or token shares when nothing cost money. */
  function ringModel(rows, total) {
    const keysTokens = rows.reduce((n, k) => n + k.tokens, 0);
    const mode = !(total.cost > 0) && keysTokens > 0 ? 'tokens' : 'cost';
    const val = (k) => (mode === 'cost' ? k.totals.cost || 0 : k.tokens);
    const paid = rows.filter((k) => val(k) > 0).sort((a, b) => val(b) - val(a));
    let kept = paid;
    let other = null;
    if (paid.length > MAX_SEGMENTS) {
      kept = paid.slice(0, MAX_SEGMENTS - 1);
      const rest = paid.slice(MAX_SEGMENTS - 1);
      other = { id: '__other__', name: 'Other', color: null, value: rest.reduce((n, k) => n + val(k), 0), count: rest.length };
    }
    const segs = kept.map((k) => ({ id: k.provider, name: keyName(k), color: k.removed ? null : k.color, value: val(k) }));
    if (other) segs.push(other);
    const sum = segs.reduce((n, s) => n + s.value, 0);
    // At least 3 degrees each; the rest shrink to make room.
    const small = segs.filter((s) => s.value / sum < MIN_ARC);
    const bigSum = segs.filter((s) => s.value / sum >= MIN_ARC).reduce((n, s) => n + s.value, 0);
    const room = 1 - small.length * MIN_ARC;
    for (const s of segs) s.share = s.value / sum;
    for (const s of segs) s.frac = s.share < MIN_ARC ? MIN_ARC : bigSum > 0 ? (s.value / bigSum) * room : s.share;
    return { mode, segs, sum, val };
  }

  const R_OUT = 96;
  const R_IN = 74;
  const R_FOCUS = 99;

  function arcPath(a0, a1, r0, r1) {
    const pt = (r, a) => `${(100 + r * Math.sin(a)).toFixed(2)} ${(100 - r * Math.cos(a)).toFixed(2)}`;
    const large = a1 - a0 > Math.PI ? 1 : 0;
    return `M ${pt(r1, a0)} A ${r1} ${r1} 0 ${large} 1 ${pt(r1, a1)} L ${pt(r0, a1)} A ${r0} ${r0} 0 ${large} 0 ${pt(r0, a0)} Z`;
  }

  /** A 22px ring in the card's surface gaps: one segment per key, clockwise from 12 o'clock, largest first. */
  function donut(model, rows, total, label) {
    const focusSeg = model.segs.find((s) => s.id === focusKey) || null;
    const mid = (R_OUT + R_IN) / 2;
    const parts = [];
    if (!model.segs.length) {
      parts.push(svg('circle', { class: 'us-ring-empty', cx: 100, cy: 100, r: mid, fill: 'none', 'stroke-width': R_OUT - R_IN }));
    } else if (model.segs.length === 1) {
      const s = model.segs[0];
      const on = focusSeg === s;
      parts.push(
        svg(
          'circle',
          { class: ['us-seg', 'us-seg-ring', keyClass(s.color), on && 'is-focus'].filter(Boolean).join(' '), cx: 100, cy: 100, r: on ? mid + 1.5 : mid, fill: 'none', 'stroke-width': on ? R_OUT - R_IN + 3 : R_OUT - R_IN, 'data-key': s.id },
          svg('title', null, `${s.name}: ${segText(model, s, total)}`),
        ),
      );
    } else {
      let a = 0;
      for (const s of model.segs) {
        const a0 = a * 2 * Math.PI;
        a += s.frac;
        const a1 = a * 2 * Math.PI;
        const on = focusSeg === s;
        parts.push(
          svg(
            'path',
            { class: ['us-seg', keyClass(s.color), on && 'is-focus'].filter(Boolean).join(' '), d: arcPath(a0, a1, R_IN, on ? R_FOCUS : R_OUT), 'data-key': s.id },
            svg('title', null, `${s.name}: ${segText(model, s, total)}`),
          ),
        );
      }
    }
    const el = svg('svg', { class: ['us-ring', focusSeg && 'has-focus'].filter(Boolean).join(' '), viewBox: '0 0 200 200', 'aria-hidden': 'true', focusable: 'false' }, parts);
    el.addEventListener('click', (e) => {
      const key = e.target.closest?.('[data-key]')?.dataset.key;
      if (key && key !== '__other__') toggleFocus(key);
    });

    // Centre: the total, or the focused key's name, cost and share.
    const only = model.segs.length === 1 ? model.segs[0] : null;
    let top;
    let mid2;
    let low;
    if (focusSeg) {
      top = focusSeg.name;
      mid2 = model.mode === 'cost' ? usd(focusSeg.value) : `${abbr(focusSeg.value)} tokens`;
      low = pctText(focusSeg.share);
    } else if (model.mode === 'tokens') {
      top = 'All keys';
      mid2 = `${abbr(model.sum)} tokens`;
      low = '';
    } else {
      top = only ? only.name : 'All keys';
      mid2 = usd(total.cost || 0);
      low = '';
    }
    const centre = h(
      'div',
      { class: ['us-centre', focusSeg && 'is-focus'], onclick: () => focusSeg && toggleFocus(focusSeg.id) },
      h('div', { class: 'us-centre-top' }, top),
      h('div', { class: 'us-centre-v' }, mid2),
      low ? h('div', { class: 'us-centre-low' }, low) : null,
    );
    return h('div', { class: 'us-donut', role: 'img', 'aria-label': label }, el, centre);
  }

  const segText = (model, s, total) => (model.mode === 'cost' ? `${usd(s.value)}, ${pctText(s.share)}` : `${num(s.value)} tokens, ${pctText(s.share)}`);

  /** The ring's full text for assistive tech (spec 8). */
  function donutLabel(model, total, caption) {
    if (!model.segs.length) return `API costs, ${caption.toLowerCase()}, ${usd(0)} estimated. No calls through Grouper in this period.`;
    const head = model.mode === 'cost' ? `API costs, ${caption.toLowerCase()}, ${usd(total.cost || 0)} estimated.` : `API costs, ${caption.toLowerCase()}. No paid usage; share of tokens.`;
    return `${head} ${model.segs.map((s) => `${s.name} ${segText(model, s, total)}`).join('. ')}.`;
  }

  function drawApi() {
    const S = st.summary;
    if (!S) return; // the first load draws once
    const caption = period()[2];

    const seg = h(
      'div',
      { class: 'seg us-period', role: 'radiogroup', 'aria-label': 'API cost period' },
      periods.map(([k, label]) =>
        h(
          'label',
          null,
          h('input', {
            type: 'radio',
            name: 'usage-period',
            value: k,
            checked: periodKey === k,
            onchange: () => {
              periodKey = k;
              if (!filterCid) {
                try {
                  localStorage.setItem(PERIOD_STORE, k);
                } catch {
                  /* storage unavailable */
                }
              }
              loadSummary();
            },
          }),
          label,
        ),
      ),
    );

    const problems = [
      sourceProblem(S, 'API costs', loadSummary),
      sourceProblem(st.providers, 'the provider list', loadProviders),
      sourceProblem(st.snaps, 'provider reports', loadSnaps),
      sourceProblem(st.free, 'Grouper’s free-request count', loadFree),
      isOffline() && S.data ? h('div', { class: 'banner warn u-banner', role: 'status' }, icon('offline'), h('div', { class: 'grow' }, `Showing the copy from ${agoLong(S.at / 1000)}.`)) : null,
    ];

    const rows = keyRows();
    let content = null;
    if (rows) {
      const total = S.data.total || {};
      const model = ringModel(rows, total);
      if (focusKey && !rows.some((k) => k.provider === focusKey)) focusKey = null;
      const tokensLine = h(
        'div',
        { class: 'us-total-tok' },
        inWords(tokensIn(total)),
        ' tokens in · ',
        inWords(tokensOut(total)),
        ` out · ${plural(total.calls || 0, 'call')}`,
      );
      const unpriced = S.data.unpriced_calls || 0;
      content = [
        h(
          'div',
          { class: 'us-total' },
          h('div', { class: 'us-total-line' }, h('span', { class: 'us-total-v' }, usd(total.cost || 0)), est()),
          h('div', { class: 'us-total-cap' }, caption),
          tokensLine,
        ),
        donut(model, rows, total, donutLabel(model, total, caption)),
        !model.segs.length ? h('p', { class: 'us-note' }, 'No calls through Grouper in this period.') : null,
        model.mode === 'tokens' ? h('p', { class: 'us-note' }, 'No paid usage. Showing share of tokens.') : null,
        unpriced
          ? h(
              'div',
              { class: 'banner warn us-warn' },
              icon('warning'),
              h('div', { class: 'grow' }, `${plural(unpriced, 'call')} used a model with no price set and ${unpriced === 1 ? 'is' : 'are'} not in this total. `, h('a', { href: '#/models' }, 'Open Models')),
            )
          : null,
        filterCid ? null : budgetLine(),
        keyList(rows, model, total),
      ];
    }
    keepFocus(apiBody, () => setChildren(apiBody, h('div', { class: 'us-period-row' }, seg), problems, content));
    apiBody.setAttribute('aria-busy', 'false');
  }

  /** "Daily budget: $1.20 of $5.00 · resets 00:00 UTC (01:00 your time)", on its own line (spec 4.1). */
  function budgetLine() {
    const report = st.limits?.data;
    if (!report?.limits) return null;
    const daily = report.limits.daily_budget_usd;
    if (!daily) {
      return h('p', { class: 'us-budget' }, 'No daily budget set. ', h('a', { href: '#/usage/limits' }, 'Limits and budgets'));
    }
    const reset = report.day_resets_at;
    return h(
      'p',
      { class: 'us-budget' },
      `Daily budget: ${usd(report.spent_today_usd || 0)} of ${usd(daily)}`,
      reset ? ` · resets 00:00 UTC (${clockOf(reset)} your time)` : ' · resets 00:00 UTC',
      ' · ',
      h('a', { href: '#/usage/limits' }, 'Limits'),
    );
  }

  function keyList(rows, model, total) {
    const heading = h('h3', { class: 'us-keys-h' }, 'Registered keys');
    if (!rows.length) {
      return h(
        'div',
        { class: 'us-keys' },
        heading,
        h('div', { class: 'card card-pad u-empty' }, h('p', null, h('strong', null, 'No provider keys yet.')), h('p', { class: 'hint' }, 'Add a provider and its key in Settings; its costs show here once Grouper routes calls through it.'), h('a', { class: 'btn btn-sm', href: '#/settings' }, 'Open Settings')),
      );
    }
    const list = h(
      'ul',
      { class: 'us-keylist', 'aria-label': 'Registered keys' },
      rows.map((k) => keyItem(k, model, total, rows)),
    );
    list.addEventListener('keydown', (e) => {
      if (!['ArrowDown', 'ArrowUp', 'Home', 'End'].includes(e.key)) return;
      const btns = [...list.querySelectorAll('.us-key-btn')];
      const i = btns.indexOf(document.activeElement);
      if (i < 0) return;
      e.preventDefault();
      const next = e.key === 'Home' ? 0 : e.key === 'End' ? btns.length - 1 : Math.max(0, Math.min(btns.length - 1, i + (e.key === 'ArrowDown' ? 1 : -1)));
      btns[next].focus();
    });
    return h('div', { class: 'us-keys' }, heading, list);
  }

  function lowBalance(k) {
    return k.snaps.some((s) => s.kind === 'balance' && s.limit_amount > 0 && (s.limit_amount - (s.used_amount || 0)) / s.limit_amount < 0.1);
  }

  function keyItem(k, model, total, rows) {
    const id = k.provider;
    const open = focusKey === id;
    const btnId = `us-key-${slug(id)}`;
    const panelId = `${btnId}-panel`;
    const cost = k.totals.cost || 0;
    const shareBase = model.mode === 'cost' ? total.cost || 0 : model.sum;
    const share = shareBase > 0 ? model.val(k) / shareBase : 0;
    const local = k.local || (k.info && ((k.info.base_url && isLocalHost(hostOf(k.info))) || false));
    const badges = [];
    if (k.removed) badges.push(h('span', { class: 'badge muted' }, 'Removed'));
    if (local) badges.push(h('span', { class: 'badge muted' }, 'Local'));
    if (lowBalance(k)) badges.push(h('span', { class: 'badge warn' }, icon('warning'), 'Low balance'));
    const btn = h(
      'button',
      {
        class: 'us-key-btn',
        type: 'button',
        id: btnId,
        'aria-expanded': open ? 'true' : 'false',
        'aria-controls': panelId,
        onclick: () => toggleFocus(id, { focusRow: true }),
      },
      keySwatch(k.color, { ring: !(model.val(k) > 0) }),
      h(
        'span',
        { class: 'us-key-main' },
        h('span', { class: 'us-key-name' }, keyName(k), badges.length ? h('span', { class: 'us-badges' }, badges) : null),
        h('span', { class: 'us-key-meta' }, inWords(tokensIn(k.totals)), ' in · ', inWords(tokensOut(k.totals)), ` out · ${plural(k.totals.calls || 0, 'call')}`),
      ),
      h('span', { class: 'us-key-fig' }, h('span', { class: 'us-key-cost' }, usd(cost)), model.val(k) > 0 ? h('span', { class: 'us-key-share' }, pctText(share)) : null),
      icon('chevron'),
    );
    const panel = h('div', { class: 'us-key-panel', id: panelId, role: 'region', 'aria-labelledby': btnId, hidden: !open }, open ? keyPanel(k, model, rows) : null);
    return h('li', { class: ['us-key', open && 'is-open'], dataset: { provider: id } }, btn, panel);
  }

  /** One model under a key, with a 4px bar in the key's colour: its share of the key's cost (tokens when the key cost nothing). */
  function modelRow(m, k) {
    const t = m.totals || {};
    const keyCost = k.totals.cost || 0;
    const share = keyCost > 0 ? (t.cost || 0) / keyCost : k.tokens > 0 ? (tokensIn(t) + tokensOut(t)) / k.tokens : 0;
    const fill = h('span', { class: ['us-bar-fill', keyClass(k.color)] });
    fill.style.width = `${Math.max(0, Math.min(100, share * 100))}%`;
    const badges = [];
    if (m.free) badges.push(h('span', { class: 'badge ok' }, 'Free'));
    if (m.local) badges.push(h('span', { class: 'badge muted' }, 'Local'));
    if (!m.free && !m.local && m.priced === false) badges.push(h('span', { class: 'badge warn' }, 'No price set'));
    return h(
      'li',
      { class: 'us-model' },
      h(
        'div',
        { class: 'us-model-line' },
        familyMark(familyKey(m.model)),
        h('span', { class: 'us-model-name' }, m.name || m.model),
        badges.length ? h('span', { class: 'us-badges' }, badges) : null,
        h('span', { class: 'us-model-cost' }, usd(t.cost || 0)),
      ),
      h('div', { class: 'us-model-tok' }, inWords(tokensIn(t)), ' in / ', inWords(tokensOut(t)), ' out'),
      h('div', { class: 'us-bar', 'aria-hidden': 'true' }, fill),
    );
  }

  /** The expanded row, in the spec's order: models, key totals, provider reports, actions. */
  function keyPanel(k, model, rows) {
    const parts = [];
    const models = (k.models || []).slice().sort((a, b) => (b.totals?.cost || 0) - (a.totals?.cost || 0) || tokensIn(b.totals) + tokensOut(b.totals) - (tokensIn(a.totals) + tokensOut(a.totals)));
    if (models.length) {
      const all = showAllModels.has(k.provider);
      const shown = all ? models : models.slice(0, MODELS_SHOWN);
      parts.push(
        h(
          'div',
          { class: 'us-block' },
          h('h4', null, 'Models'),
          h('ul', { class: 'us-models' }, shown.map((m) => modelRow(m, k))),
          models.length > MODELS_SHOWN
            ? h(
                'button',
                {
                  class: 'btn btn-sm',
                  type: 'button',
                  id: `us-all-${slug(k.provider)}`,
                  'aria-expanded': all ? 'true' : 'false',
                  onclick: () => {
                    if (all) showAllModels.delete(k.provider);
                    else showAllModels.add(k.provider);
                    drawApi();
                  },
                },
                all ? 'Show fewer' : `Show all ${models.length}`,
              )
            : null,
        ),
      );
    } else {
      parts.push(h('p', { class: 'us-empty-line' }, 'No calls through this key in this period.'));
    }

    const t = k.totals;
    const cache = [];
    if (t.cache_read_tokens) cache.push(h('span', null, inWords(t.cache_read_tokens), ' read from cache'));
    if (t.cache_write_tokens) cache.push(h('span', null, inWords(t.cache_write_tokens), ' written to cache'));
    parts.push(
      h(
        'p',
        { class: 'us-totals' },
        'Input ',
        inWords(tokensIn(t)),
        cache.length ? [' (of which ', cache.flatMap((c, i) => (i ? [', ', c] : [c])), ')'] : null,
        ' · Output ',
        inWords(tokensOut(t)),
      ),
    );

    const reports = providerReports(k);
    if (reports) parts.push(reports);

    const info = k.info;
    const actions = h(
      'div',
      { class: 'us-actions' },
      info && !k.removed
        ? h(
            'button',
            { class: 'btn btn-sm', type: 'button', id: `us-colour-${slug(k.provider)}`, 'data-write': true, 'aria-expanded': pickerFor === k.provider ? 'true' : 'false', onclick: () => togglePicker(k.provider) },
            keySwatch(k.color),
            'Change colour',
          )
        : null,
      h('a', { class: 'btn btn-sm', href: `#/usage/breakdown${qs({ provider: k.provider })}` }, 'See breakdown'),
    );
    parts.push(actions);
    if (pickerFor === k.provider && info) {
      const providers = Array.isArray(st.providers?.data) ? st.providers.data : [];
      parts.push(
        h(
          'div',
          { class: 'us-picker' },
          colorPicker({
            providerId: k.provider,
            current: k.color,
            others: colorUse(providers, k.provider),
            idPrefix: `us-pick-${slug(k.provider)}`,
            onPick: (name) => saveColor(k.provider, name),
          }),
          h('p', { class: 'hint' }, 'Saved for every device. A colour another key uses shows that key under it.'),
        ),
      );
    }
    return parts;
  }

  /** What the provider itself reports (spec 4.3): a balance or spend snapshot, OpenRouter's free allowance. */
  function providerReports(k) {
    const info = k.info;
    const snaps = k.snaps;
    const reported = snaps.filter((s) => s.kind === 'spend' || s.kind === 'balance');
    const allowances = snaps.filter((s) => s.kind !== 'spend' && s.kind !== 'balance' && s.kind !== 'status');
    const F = st.free;
    const freeSent = F?.data && Array.isArray(F.data.providers) ? new Map(F.data.providers.map((p) => [p.provider, p.sent])) : null;
    const free = { known: Boolean(freeSent), listed: freeSent?.has(k.provider) || false, sent: freeSent?.get(k.provider) || 0 };
    const openRouter = free.listed || isOpenRouter(info);
    const dailyFree = snaps.find(isFreeAllowance);
    const otherAllowances = allowances.filter((s) => s !== dailyFree);
    if (!reported.length && !otherAllowances.length && !(openRouter && (dailyFree || free.known))) return null;
    const api = balanceApi(info);
    const body = [];
    if (reported.length) body.push(reported.map(reportedRow), ageLine(Math.min(...reported.map((s) => s.fetched_at)), api ? api.source : 'provider API'));
    if (openRouter) body.push(h('div', { class: 'us-free' }, freeAllowance(dailyFree, free)));
    if (otherAllowances.length) body.push(otherAllowances.map(reportedRow), ageLine(Math.min(...otherAllowances.map((s) => s.fetched_at))));
    return h('div', { class: 'us-block' }, h('h4', null, 'Provider reports'), h('p', { class: 'hint' }, 'What the provider says. It can include use outside Grouper, so it is not in the total.'), body);
  }

  // ---- focus and colour

  function syncUrl() {
    // replaceState does not fire hashchange: the page stays, and reload keeps the focus.
    try {
      history.replaceState(null, '', `#/usage${qs({ conversation: filterCid, provider: focusKey })}`);
    } catch {
      /* ignore */
    }
  }

  function toggleFocus(id, { focusRow = false } = {}) {
    focusKey = focusKey === id ? null : id;
    if (!focusKey) pickerFor = null;
    syncUrl();
    drawApi();
    if (focusRow) document.getElementById(`us-key-${slug(id)}`)?.focus({ preventScroll: true });
  }

  function clearFocus() {
    if (!focusKey) return;
    const id = focusKey;
    focusKey = null;
    pickerFor = null;
    syncUrl();
    drawApi();
    document.getElementById(`us-key-${slug(id)}`)?.focus({ preventScroll: true });
  }

  function applyRouteFocus(id) {
    const next = id || null;
    if (next === focusKey) return;
    focusKey = next;
    if (!next) pickerFor = null;
    drawApi();
    if (next) document.getElementById(`us-key-${slug(next)}`)?.scrollIntoView({ block: 'nearest' });
  }

  const onKey = (e) => {
    if (e.key !== 'Escape' || !focusKey || document.querySelector('dialog[open]')) return;
    if (!apiBody.contains(document.activeElement) && document.activeElement !== document.body) return;
    clearFocus();
  };
  document.addEventListener('keydown', onKey);

  function togglePicker(id) {
    pickerFor = pickerFor === id ? null : id;
    drawApi();
    if (pickerFor) document.getElementById(`us-pick-${slug(id)}-${currentColor(id) || 'teal'}`)?.focus({ preventScroll: true });
  }

  const currentColor = (id) => (Array.isArray(st.providers?.data) ? st.providers.data.find((p) => p.id === id)?.color : null);

  async function saveColor(id, name) {
    try {
      const updated = await setProviderColor(id, name);
      const color = updated?.color || name;
      if (Array.isArray(st.providers?.data)) st.providers.data = st.providers.data.map((p) => (p.id === id ? { ...p, color } : p));
      if (st.summary?.data?.keys) st.summary.data = { ...st.summary.data, keys: st.summary.data.keys.map((k) => (k.provider === id ? { ...k, color } : k)) };
      toast(`${id} is now ${keyColorLabel(color).toLowerCase()}.`);
    } catch (err) {
      toastError(err);
    }
    if (!signal.aborted) {
      drawApi();
      document.getElementById(`us-pick-${slug(id)}-${currentColor(id) || name}`)?.focus({ preventScroll: true });
    }
  }

  // ---- section 2: subscriptions

  /** Cards merged by (provider, account_key), plus the slim cards for states with no bars. */
  function buildPlans() {
    const all = Array.isArray(st.snaps?.data) ? st.snaps.data : [];
    const probe = all.filter((s) => s.source === 'probe');
    const windows = probe.filter((s) => s.kind !== 'status');
    const statuses = probe.filter((s) => s.kind === 'status');
    const devices = store.devices || [];

    // Merge by provider and account_key; with no account_key, one card per device (spec decision 4).
    const cards = new Map();
    for (const s of windows) {
      const key = `${s.provider}|${s.account_key ? `k:${s.account_key}` : `d:${s.device_id || ''}`}`;
      let c = cards.get(key);
      if (!c) {
        c = { key, provider: s.provider, plan: planTitle(s), devices: new Set(), rows: new Map(), newest: 0, notes: [], forceStale: false };
        cards.set(key, c);
      }
      if (s.device_id) c.devices.add(s.device_id);
      const rowKey = `${s.kind}|${s.window}|${s.currency || ''}`;
      const prev = c.rows.get(rowKey);
      if (!prev || s.fetched_at > prev.fetched_at) c.rows.set(rowKey, s);
      if (s.fetched_at > c.newest) {
        c.newest = s.fetched_at;
        c.plan = planTitle(s);
      }
    }

    const withWindows = new Set(windows.map((s) => `${s.provider}|${s.device_id}`));
    const slim = [];
    const classify = (text) => {
      const t = String(text || '').toLowerCase();
      if (/not signed in/.test(t)) return 'not_signed_in';
      if (/expired/.test(t)) return 'expired';
      return 'unavailable';
    };
    for (const s of statuses) {
      const state = classify(s.window);
      const dev = deviceName(s.device_id);
      const name = providerName(s.provider);
      const app = PLAN_PROVIDERS[s.provider]?.app || name;
      const card = [...cards.values()].find((c) => c.provider === s.provider && c.devices.has(s.device_id));
      if (card) {
        // Values from this device are kept (stale), with the reason beside them.
        if (state === 'expired') {
          card.forceStale = true;
          card.notes.push(`Sign-in expired on ${dev}. Open ${app} there to renew it.`);
        } else if (state === 'not_signed_in') {
          card.forceStale = true;
          card.notes.push(`${name} is not signed in on ${dev} now.`);
        } else {
          card.notes.push(`${dev}: ${sentenceCase(s.window)}`);
        }
      } else if (state === 'not_signed_in') {
        slim.push({ kind: 'status', provider: s.provider, device_id: s.device_id, text: `${name} on ${dev}: not signed in.`, at: s.fetched_at });
      } else if (state === 'expired') {
        slim.push({ kind: 'status', provider: s.provider, device_id: s.device_id, text: `${name} on ${dev}: sign-in expired. Open ${app} there to renew it.`, at: s.fetched_at });
      } else {
        slim.push({ kind: 'status', provider: s.provider, device_id: s.device_id, text: `${name} on ${dev}: ${sentenceCase(s.window)}`, at: s.fetched_at });
      }
    }

    // Reading is off on the device (the owner's change to 5.4): a signed-in harness, no reading, and the device says it is off.
    const providers = Array.isArray(st.providers?.data) ? st.providers.data : [];
    const seen = new Set(statuses.map((s) => `${s.provider}|${s.device_id}`));
    for (const p of providers) {
      if (p.kind !== 'harness') continue;
      const slugName = typeof p.options?.harness === 'string' ? p.options.harness : String(p.id).split('@')[0];
      const probeProvider = probeProviderOf(slugName);
      const deviceId = p.via_device || (String(p.id).includes('@') ? String(p.id).split('@')[1] : '');
      if (!probeProvider || !deviceId) continue;
      if (harnessSignInInfo(p)?.label !== 'Signed in') continue;
      const dev = devices.find((d) => d.id === deviceId || d.name === deviceId);
      const id = dev?.id || deviceId;
      if (!dev || readingOn(dev, probeProvider) !== false) continue;
      const k = `${probeProvider}|${id}`;
      if (withWindows.has(k) || seen.has(k)) continue;
      seen.add(k);
      slim.push({ kind: 'off', provider: probeProvider, device_id: id, text: `${providerName(probeProvider)} on ${dev.name}. Reading is off on that device.`, how: `Open Grouper on ${dev.name} › Settings › This device.` });
    }
    // A card whose device now has reading off: values may be stale.
    for (const c of cards.values()) {
      for (const id of c.devices) {
        const dev = devices.find((d) => d.id === id);
        if (dev && readingOn(dev, c.provider) === false) c.notes.push(`Reading is off on ${dev.name}. Open Grouper there › Settings › This device to turn it on.`);
      }
    }

    const list = [...cards.values()].map((c) => ({ ...c, top: Math.max(0, ...[...c.rows.values()].map((r) => (r.used_pct ?? 0))) }));
    list.sort((a, b) => b.top - a.top || a.plan.localeCompare(b.plan));
    return { cards: list, slim };
  }

  /** true/false from the device's flags; null when the hub does not report them (an older hub). */
  function readingOn(dev, probeProvider) {
    if (typeof dev.usage_probes !== 'boolean') return null;
    if (!dev.usage_probes) return false;
    const list = dev.usage_probe_providers;
    if (Array.isArray(list) && list.length && !list.includes(probeProvider)) return false;
    return true;
  }

  const sentenceCase = (t) => {
    const s = String(t || '').trim();
    if (!s) return 'unavailable.';
    // Follows "Antigravity on Gaming PC: ", so it starts in lower case.
    return `${s.charAt(0).toLowerCase()}${s.slice(1)}${/[.!?]$/.test(s) ? '' : '.'}`;
  };

  function drawSubs() {
    if (filterCid) return;
    const S = st.snaps;
    if (!S) return;
    let content;
    if (!S.data) {
      content = null;
    } else {
      const { cards, slim } = buildPlans();
      if (!cards.length && !slim.length) {
        content = h(
          'div',
          { class: 'card card-pad u-empty' },
          h('p', null, h('strong', null, 'No subscriptions found.')),
          h('p', null, 'Sign in to Claude Code, Codex, Grok or Antigravity on one of your devices. Usage reading is on by default there.'),
        );
      } else {
        content = [
          cards.length ? h('div', { class: 'us-plans' }, cards.map(planCard)) : null,
          slim.length ? h('ul', { class: 'us-slims' }, slim.map(slimCard)) : null,
        ];
      }
    }
    keepFocus(subBody, () => setChildren(subBody, sourceProblem(S, 'subscription readings', loadSnaps), content));
    subBody.setAttribute('aria-busy', 'false');
  }

  function slimCard(s) {
    const tone = s.kind === 'off' ? 'muted' : 'warn';
    return h(
      'li',
      { class: ['us-slim', tone] },
      icon(s.kind === 'off' ? 'offline' : 'warning'),
      h('div', { class: 'grow' }, h('div', { class: 'us-slim-text' }, s.text), s.how ? h('div', { class: 'us-slim-how' }, s.how) : null, s.at ? h('div', { class: 'us-slim-how' }, `Read ${agoLong(s.at)}`) : null),
    );
  }

  /** Through-Grouper tokens for this card's provider on its devices, for the chosen period (spec 5.5). */
  function throughGrouper(card) {
    const subs = st.summary?.data?.subscriptions;
    if (!Array.isArray(subs)) return null;
    let tin = 0;
    let tout = 0;
    for (const s of subs) {
      if (probeProviderOf(s.harness) !== card.provider) continue;
      if (s.device_id && card.devices.size && !card.devices.has(s.device_id)) continue;
      tin += tokensIn(s.totals);
      tout += tokensOut(s.totals);
    }
    if (!tin && !tout) return null;
    return h('div', { class: 'us-through' }, `Through Grouper ${period()[3]}: `, inWords(tin), ' tokens in, ', inWords(tout), ' out');
  }

  function planCard(c) {
    const meta = PLAN_PROVIDERS[c.provider];
    const headId = `us-plan-${slug(c.key)}`;
    const stale = nowSecs() - c.newest > STALE_SECS || c.forceStale;
    const names = [...c.devices].map((id) => deviceName(id));
    const signedIn = names.length ? `Signed in on ${names.length > 1 ? `${names.slice(0, -1).join(', ')} and ${names[names.length - 1]}` : names[0]}` : null;
    const rows = [...c.rows.values()].sort((a, b) => compareTuples(rowOrder(a), rowOrder(b)));

    // Plain rows, rows with a scope shown inline, and groups (a label shared by a scope) with a sub-heading.
    const scopes = new Map();
    for (const r of rows) {
      const p = parseWindow(r.window);
      if (r.kind === 'window' && p.scope && r.label) {
        if (!scopes.has(p.scope)) scopes.set(p.scope, []);
        scopes.get(p.scope).push(r);
      }
    }
    const grouped = new Set();
    const groups = [];
    for (const [scope, list] of scopes) {
      if (list.length >= 2 || c.provider === 'antigravity') {
        list.forEach((r) => grouped.add(r));
        groups.push({ scope, label: list[0].label, rows: list });
      }
    }
    const plain = rows.filter((r) => !grouped.has(r));
    const body = [
      plain.length ? h('div', { class: 'us-rows' }, plain.map((r) => meterRow(r, c, stale))) : null,
      groups.map((g) => h('div', { class: 'us-group' }, h('h4', { class: 'us-group-h' }, g.label), h('div', { class: 'us-rows' }, g.rows.map((r) => meterRow(r, c, stale, { grouped: true }))))),
    ];

    return h(
      'article',
      { class: ['us-plan', `fam-${meta?.fam || 'unknown'}`, stale && 'is-stale'], 'aria-labelledby': headId },
      h(
        'header',
        { class: 'us-plan-head' },
        h('div', { class: 'us-plan-titles' }, h('h3', { id: headId }, meta ? familyMark(meta.fam) : null, h('span', null, c.plan)), signedIn ? h('div', { class: 'us-plan-sub' }, signedIn) : null),
        h(
          'div',
          { class: 'us-plan-age' },
          h('span', null, agoLong(c.newest)),
          stale ? h('span', { class: 'badge warn' }, `Stale · read ${dateTime(c.newest)}`) : null,
        ),
      ),
      body,
      c.notes.length ? h('ul', { class: 'us-notes' }, c.notes.map((n) => h('li', null, icon('warning'), n))) : null,
      throughGrouper(c),
    );
  }

  function meterRow(r, card, stale, { grouped = false } = {}) {
    const p = parseWindow(r.window);
    let label;
    if (r.kind === 'spend' && r.window === 'extra') label = 'Extra usage (paid)';
    else if (r.kind === 'balance' && r.window === 'credits') label = 'Credits';
    else if (r.kind !== 'window') label = titleCase(r.window || r.kind);
    else {
      label = spanLabel(p);
      if (p.scope && !grouped) label += ` · ${r.label || titleCase(p.scope)}`;
    }
    const money = (v) => (r.currency && r.currency.toUpperCase() !== 'USD' ? `${(Math.round(v * 100) / 100).toFixed(2)} ${r.currency}` : usd(v));

    // A balance is an amount, not a share: no bar.
    if (r.kind === 'balance') {
      return h('div', { class: 'us-row us-row-amount' }, h('span', { class: 'us-row-label' }, label), h('span', { class: 'us-row-val' }, money(r.used_amount ?? 0)));
    }
    let pct = r.used_pct;
    if ((pct === null || pct === undefined) && r.kind === 'spend' && r.limit_amount > 0) pct = ((r.used_amount || 0) / r.limit_amount) * 100;
    const amount = r.kind === 'spend' ? (r.limit_amount ? `${money(r.used_amount || 0)} of ${money(r.limit_amount)}` : money(r.used_amount || 0)) : null;
    if (pct === null || pct === undefined) {
      return h('div', { class: 'us-row us-row-amount' }, h('span', { class: 'us-row-label' }, label), h('span', { class: 'us-row-val' }, amount || '—'));
    }

    const resetPassed = r.resets_at && r.resets_at <= nowSecs();
    const noWindow = !r.resets_at && !(pct > 0);
    const used = Math.max(0, Math.min(100, pct));
    let state = used >= 100 ? 'danger' : used >= 80 ? 'warn' : 'ok';
    // 99.6 stays "99.6%", not "100%": it is not used up yet.
    const shown = used >= 99 && used < 100 ? String(Math.floor(used * 10) / 10) : String(Math.round(used));
    let valueText = amount ? `${amount} · ${shown}% used` : `${shown}% used`;
    let resetText = r.resets_at ? resetWords(r.resets_at) : '';
    let speak = `${label}, ${Math.round(used)} percent used${r.resets_at ? `, ${resetWords(r.resets_at, { long: true })}` : ''}`;
    let empty = false;
    if (noWindow) {
      state = 'idle';
      empty = true;
      valueText = 'Not in use';
      resetText = 'Starts when you next use it.';
      speak = `${label}, not in use. Starts when you next use it.`;
    } else if (resetPassed) {
      state = 'passed';
      empty = true;
      valueText = 'Reset';
      resetText = `Reset at ${clockOf(r.resets_at)}. Waiting for a new reading.`;
      speak = `${label}, reset at ${clockOf(r.resets_at)}. Waiting for a new reading.`;
    }
    const word = state === 'danger' ? 'Used up' : state === 'warn' ? 'Nearly used' : null;
    if (word) speak += `, ${word.toLowerCase()}`;
    const fill = h('span', { class: 'us-meter-fill' });
    fill.style.width = empty ? '0%' : `${used}%`;
    return h(
      'div',
      { class: ['us-row', `is-${state}`] },
      h('span', { class: 'us-row-label' }, label),
      h(
        'div',
        { class: 'us-meter', role: 'meter', 'aria-label': label, 'aria-valuemin': 0, 'aria-valuemax': 100, 'aria-valuenow': empty ? 0 : Math.round(used), 'aria-valuetext': speak },
        fill,
      ),
      h('span', { class: 'us-row-val' }, valueText, word ? h('span', { class: ['badge', state === 'danger' ? 'danger' : 'warn', 'us-state'] }, icon('warning'), word) : null),
      h('span', { class: 'us-row-reset' }, resetText),
    );
  }

  // ---- live updates: in place, never moving focus or scroll

  const metered = throttle(() => {
    if (signal.aborted || isOffline() || document.hidden) return;
    loadSummary();
  }, 5000);
  const offEvent = on('event', (e) => {
    if (!e || (filterCid && e.conversation_id !== filterCid)) return;
    const usageEvent = e.kind === 'thread_activity' && e.event?.type === 'usage';
    if (usageEvent || e.kind === 'turn_finished' || e.kind === 'thread_finished' || e.kind === 'budget_warning') metered();
    // Another device recoloured a key (the hub's providers-changed notification).
    if (typeof e.kind === 'string' && /^providers?_changed$/.test(e.kind)) loadProviders();
  });
  const offConvs = on('conversations', () => {
    if (filterCid) renderHeader();
  });
  const offDevices = on('devices', () => drawSubs());
  const offPresence = on('device_presence', () => drawSubs());
  const offOnline = on('online', (online) => {
    syncRefresh();
    drawApi();
    if (online && !signal.aborted) loadAll(); // back online: replace any cached copies
  });
  // The key focus lives in the URL (`provider=`), which the route key ignores.
  const offRoute = on('route', (r) => {
    if (r.view === 'usage' && (r.sub || 'summary') === 'summary' && (r.conversation || null) === filterCid) applyRouteFocus(r.provider);
  });
  // Reset countdowns and reading ages move on their own: once a minute.
  const minute = setInterval(() => {
    if (document.hidden || signal.aborted) return;
    if (isOffline()) drawSubs();
    else loadSnaps();
  }, 60000);

  loadAll();
  // When the Usage tab opens and the newest subscription reading is over 2 minutes old, ask for fresh ones.
  if (!filterCid && !isOffline()) {
    load('/v1/usage/snapshots', signal)
      .then((res) => {
        const probe = (Array.isArray(res.data) ? res.data : []).filter((s) => s.source === 'probe');
        const newest = Math.max(0, ...probe.map((s) => s.fetched_at));
        if (!res.error && probe.length && nowSecs() - newest > 120) {
          post('/v1/usage/refresh')
            .then(() => !signal.aborted && loadSnaps())
            .catch(() => {});
        }
      })
      .catch(() => {});
  }

  return () => {
    aborter.abort();
    unguard();
    metered.cancel();
    clearInterval(minute);
    document.removeEventListener('keydown', onKey);
    offEvent();
    offConvs();
    offDevices();
    offPresence();
    offOnline();
    offRoute();
    breakdown?.destroy();
  };
}
