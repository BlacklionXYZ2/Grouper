// Usage & limits. Two tracks, kept apart and never added together:
//   1. API costs, one card per provider key: Grouper's metered estimate for the
//      chosen range beside what the provider itself reports (spend, balance).
//   2. Subscription usage: plan windows (Claude, ChatGPT/Codex) read on devices,
//      shown as unofficial snapshots.
// Then the metered breakdown (by provider, model, chat, thread or day; a provider
// card's Filter button narrows it to that provider) and the hub's limits and
// budgets. `#/usage?conversation=<id>` shows one chat's usage; `provider=<id>`
// (alone or with `conversation=`) is the provider filter, so it survives a reload
// and Back undoes it. Changing only the filter updates the page in place.

import { h, icon, setChildren, setText, keepFocus, throttle } from '../lib/dom.js';
import { get, post, put, qs, enc, describe, isAbort } from '../lib/api.js';
import { store, on, isAdmin, deviceName, deviceOnline } from '../lib/store.js';
import { bar, toast, toastError, skeleton, errorBanner, guardWrites, isOffline, navigate } from '../lib/ui.js';
import { num, usd, ago, dateTime, isoDay, plural } from '../lib/format.js';
import { familyKey, familyMark, familyLabel } from '../lib/family.js';

const GROUPS = [
  ['provider', 'Provider'],
  ['model', 'Model'],
  ['conversation', 'Chat'],
  ['thread', 'Thread'],
  ['day', 'Day'],
];

const RANGES = [
  ['today', 'Today'],
  ['7', 'Last 7 days'],
  ['30', 'Last 30 days'],
  ['90', 'Last 90 days'],
  ['all', 'All time'],
  ['custom', 'Custom dates'],
];

// A snapshot older than this is labelled stale, with its time.
const STALE_SECS = 3600;

// Limits as the editor shows them. `off` is what null (switch off) means for that
// key; `prefix`/`suffix` show the unit next to the number; `scale` converts what
// the field shows to what the hub stores (the warning share is shown in percent).
const LIMITS = [
  { key: 'daily_budget_usd', label: 'Daily budget', unit: 'US dollars', prefix: '$', step: '0.01', help: 'All usage in one UTC day.', off: 'Off: no daily budget' },
  { key: 'conversation_budget_usd', label: 'Budget per conversation', unit: 'US dollars', prefix: '$', step: '0.01', help: 'A conversation’s orchestrator or standard chat and its threads.', off: 'Off: no budget' },
  { key: 'thread_budget_usd', label: 'Budget per thread', unit: 'US dollars', prefix: '$', step: '0.01', help: 'Default cap for one thread; the orchestrator may ask for less.', off: 'Off: no budget' },
  { key: 'warn_at_fraction', label: 'Warn at', unit: 'percent of a budget', suffix: '%', step: '5', max: '100', scale: 100, help: 'Warn when spend reaches this share of a budget.', off: 'Default: 80%' },
  { key: 'max_running_threads', label: 'Running threads', unit: 'threads', suffix: 'threads', step: '1', help: 'Threads running at once across the account.', off: 'Off: no cap' },
  { key: 'max_running_threads_per_device', label: 'Running threads per device', unit: 'threads', suffix: 'threads', step: '1', help: 'Threads running at once on any one device, the hub included.', off: 'Off: no cap' },
  { key: 'approval_timeout_secs', label: 'Approval timeout', unit: 'seconds', suffix: 's', step: '1', help: 'How long an approval waits before it’s answered for you.', off: 'Off: waits for you' },
  { key: 'ask_timeout_secs', label: 'Question timeout', unit: 'seconds', suffix: 's', step: '1', help: 'How long a thread’s ask_orchestrator waits.', off: 'Off: waits forever' },
];

// ---- last good copy of each read, so an offline visit still shows something

const cache = new Map(); // `${hub}|${path}` -> {data, at}

async function load(path, signal) {
  const key = `${store.hub?.hub_id || ''}|${path}`;
  try {
    const data = await get(path, { signal });
    const at = Date.now();
    cache.set(key, { data, at });
    return { data, at, error: null, cached: false };
  } catch (error) {
    if (isAbort(error)) throw error;
    const hit = cache.get(key);
    if (hit) return { data: hit.data, at: hit.at, error, cached: true };
    return { data: null, at: 0, error, cached: false };
  }
}

// ---- small formatting helpers

const nowSecs = () => Date.now() / 1000;

/** The "estimated" tag that goes beside every Grouper cost figure. */
function est() {
  return h('span', { class: 'badge u-est' }, 'estimated');
}

/** An amount in its own unit: USD as dollars, anything else with its code. */
function money(amount, currency) {
  if (amount === null || amount === undefined || Number.isNaN(amount)) return '—';
  const c = String(currency || '').trim();
  if (!c) return num(Math.round(amount * 100) / 100);
  if (c.toUpperCase() === 'USD') return usd(amount);
  if (c.toLowerCase() === 'credits') return `${num(Math.round(amount * 100) / 100)} credits`;
  return `${amount.toFixed(2)} ${c}`;
}

function sumRows(rows) {
  return rows.reduce(
    (t, r) => ({
      calls: t.calls + r.calls,
      input_tokens: t.input_tokens + r.input_tokens,
      output_tokens: t.output_tokens + r.output_tokens,
      cache_read_tokens: t.cache_read_tokens + r.cache_read_tokens,
      cache_write_tokens: t.cache_write_tokens + r.cache_write_tokens,
      cost: t.cost + r.cost,
    }),
    { calls: 0, input_tokens: 0, output_tokens: 0, cache_read_tokens: 0, cache_write_tokens: 0, cost: 0 },
  );
}

const tokensOf = (r) => r.input_tokens + r.output_tokens + r.cache_read_tokens + r.cache_write_tokens;

/** "in 2h 14m" style, for reset times. */
function inDuration(secs) {
  const s = Math.round(secs);
  if (s <= 0) return 'now';
  if (s < 60) return 'in under a minute';
  if (s < 3600) return `in ${Math.round(s / 60)} min`;
  if (s < 86400) return `in ${Math.floor(s / 3600)}h ${String(Math.floor((s % 3600) / 60)).padStart(2, '0')}m`;
  return `in ${Math.floor(s / 86400)}d ${Math.floor((s % 86400) / 3600)}h`;
}

function slug(s) {
  return String(s).replace(/[^a-zA-Z0-9_-]+/g, '-');
}

function hostOf(provider) {
  try {
    return new URL(provider.base_url).host.toLowerCase();
  } catch {
    return '';
  }
}

/** Which official balance API the hub reads for this provider (see usage.rs `collector`). */
function balanceApi(provider) {
  if (!provider || provider.kind !== 'openai') return null;
  // Includes "openrouter" with no base_url, which defaults to OpenRouter.
  if (isOpenRouter(provider)) return { name: 'OpenRouter', source: 'OpenRouter key and credits API' };
  if (hostOf(provider) === 'api.deepseek.com') return { name: 'DeepSeek', source: 'DeepSeek balance API' };
  return null;
}

/** Whether a provider talks to OpenRouter (see usage.rs `is_openrouter`). */
function isOpenRouter(provider) {
  if (!provider || provider.kind !== 'openai') return false;
  return provider.base_url ? hostOf(provider) === 'openrouter.ai' : provider.id === 'openrouter';
}

// OpenRouter's daily free-model allowance, as the hub records it from a 429.
const FREE_KIND = 'requests';
const FREE_WINDOW = 'free requests/day';
const isFreeAllowance = (s) => s.kind === FREE_KIND && s.window === FREE_WINDOW;

/** Local "14:05" for a Unix time. */
function clock(secs) {
  return new Date(secs * 1000).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
}

function isLocalHost(host) {
  return /^(localhost|127\.|\[::1\]|0\.0\.0\.0)/.test(host) || host.endsWith('.local') || host.endsWith('.localhost');
}

/** A bar plus the same facts in words; the bar alone never carries the meaning. */
function meter(pct, words) {
  const p = Math.max(0, Math.min(100, pct));
  const tone = pct >= 100 ? 'danger' : pct >= 80 ? 'warn' : 'ok';
  const fill = h('span', { class: 'u-meter-fill' });
  fill.style.width = `${p}%`;
  return h(
    'div',
    { class: ['u-meter-wrap', tone] },
    h('div', { class: 'u-meter', 'aria-hidden': 'true' }, fill),
    h('div', { class: 'u-meter-text' }, words, pct >= 100 ? h('span', { class: 'badge danger' }, icon('warning'), 'Used up') : pct >= 80 ? h('span', { class: 'badge warn' }, icon('warning'), 'Nearly used') : null),
  );
}

/** "Fetched 5m ago", plus a stale label with the exact time once it's over an hour old. */
function ageLine(fetchedAt, source = '') {
  const stale = nowSecs() - fetchedAt > STALE_SECS;
  return h(
    'div',
    { class: 'u-meta' },
    icon('clock'),
    h('span', null, `Fetched ${ago(fetchedAt)}${source ? ` · ${source}` : ''}`),
    stale ? h('span', { class: 'badge warn' }, `Stale · ${dateTime(fetchedAt)}`) : null,
  );
}

/** End a message with a full stop unless it already has one. */
function sentence(text) {
  const t = String(text || '').trim();
  return /[.!?]$/.test(t) ? t : `${t}.`;
}

/** What a read that failed (or fell back to the cached copy) looks like inline. */
function sourceProblem(res, what, retry) {
  if (!res?.error) return null;
  if (res.cached) {
    return h(
      'div',
      { class: 'banner warn u-banner', role: 'status' },
      icon('offline'),
      h('div', { class: 'grow' }, `Couldn’t load ${what}: ${sentence(describe(res.error))} Showing the copy from ${ago(res.at / 1000)}.`),
      retry ? h('button', { class: 'btn btn-sm', type: 'button', onclick: retry }, 'Try again') : null,
    );
  }
  const banner = errorBanner(res.error, retry);
  banner.classList.add('u-banner');
  banner.setAttribute('role', 'status'); // a background read, not an interrupted action
  banner.querySelector('.grow')?.prepend(`Couldn’t load ${what}. `);
  return banner;
}

// ---- subscription labels

const PLAN_PROVIDERS = { claude: 'Claude', codex: 'ChatGPT (Codex)' };

function planName(s) {
  const provider = PLAN_PROVIDERS[s.provider] || s.provider;
  const prefix = s.provider === 'codex' ? 'chatgpt' : s.provider;
  let plan = String(s.account || '');
  if (plan.toLowerCase().startsWith(`${prefix}-`)) plan = plan.slice(prefix.length + 1);
  else if (plan.toLowerCase() === prefix) plan = '';
  plan = plan.replace(/[_-]+/g, ' ').replace(/\b\w/g, (c) => c.toUpperCase());
  return plan ? `${provider} ${plan}` : provider;
}

function windowName(s) {
  const w = String(s.window || '');
  if (s.kind === 'spend' && w === 'extra') return 'Extra usage (paid)';
  if (s.kind === 'balance' && w === 'credits') return 'Credits';
  const [base, ...rest] = w.split(':');
  const m = /^(\d+)([mhd])$/.exec(base);
  let name = m ? `${m[1]}-${{ m: 'minute', h: 'hour', d: 'day' }[m[2]]} window` : w || s.kind;
  if (rest.length) name += ` · ${rest.join(':').replace(/_/g, ' ')}`;
  return name;
}

function windowOrder(s) {
  const m = /^(\d+)([mhd])/.exec(String(s.window || ''));
  const secs = m ? Number(m[1]) * { m: 60, h: 3600, d: 86400 }[m[2]] : 1e12;
  return [s.provider, s.account, s.device_id || '', s.kind === 'window' ? 0 : 1, secs, String(s.window)];
}

function compareTuples(a, b) {
  for (let i = 0; i < a.length; i++) {
    if (a[i] < b[i]) return -1;
    if (a[i] > b[i]) return 1;
  }
  return 0;
}

// ---- the view

export function renderUsage(container, { conversation: filterCid = null, provider: routeProvider = null } = {}) {
  const admin = isAdmin();
  const barSlot = h('div', { class: 'bar-slot' });
  const body = h('div', { class: 'view-body u-body' });
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
    refreshBtn.setAttribute('aria-describedby', 'u-refresh-help');
  };

  function renderHeader() {
    if (filterCid) {
      const name = chatTitle();
      setChildren(
        barSlot,
        bar({
          title: 'Chat usage',
          caption: name,
          back: `#/chat/${enc(filterCid)}`,
          actions: [h('a', { class: 'btn btn-sm', href: '#/usage' }, 'All usage')],
        }),
      );
      document.title = `${name} · Usage · Grouper`;
    } else {
      setChildren(barSlot, bar({ title: 'Usage & limits', caption: 'API costs and subscription quotas, tracked apart', actions: [refreshBtn] }));
      document.title = 'Usage & limits · Grouper';
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
  let rangeKey = filterCid ? 'all' : '30';
  const todayIso = isoDay(new Date());
  let custom = { since: isoDay(new Date(Date.now() - 29 * 86400000)), until: todayIso };
  let group = 'model';
  // A provider card's filter: the breakdown shows only that provider's calls.
  // It lives in the URL (`provider=`); clicks navigate and the route applies it.
  let providerFilter = routeProvider || null;
  const st = { providers: null, byProvider: null, free: null, snaps: null, rows: null };

  function rangeQuery() {
    const today = new Date();
    const back = (n) => isoDay(new Date(Date.UTC(today.getUTCFullYear(), today.getUTCMonth(), today.getUTCDate() - n)));
    switch (rangeKey) {
      case 'today':
        return { since: isoDay(today), until: isoDay(today) };
      case '7':
      case '30':
      case '90':
        return { since: back(Number(rangeKey) - 1), until: isoDay(today) };
      case 'custom':
        return { since: custom.since, until: custom.until };
      default:
        return {};
    }
  }

  function rangeLabel() {
    if (rangeKey === 'custom') {
      const { since, until } = custom;
      if (since && until) return since === until ? since : `${since} to ${until}`;
      if (since) return `from ${since}`;
      if (until) return `up to ${until}`;
      return 'all time';
    }
    const r = RANGES.find(([k]) => k === rangeKey);
    return r ? r[1].charAt(0).toLowerCase() + r[1].slice(1) : '';
  }

  /** "yet", "today", "in the last 30 days" or "in 2026-11-01 to 2026-11-20". */
  function inRange() {
    if (rangeKey === 'all') return 'yet';
    if (rangeKey === 'today') return 'today';
    if (rangeKey === 'custom') return `in ${rangeLabel()}`;
    return `in the ${rangeLabel()}`;
  }

  // ---- layout skeleton (built once; sections redraw their own bodies in place)
  const rangeSelect = h(
    'select',
    {
      class: 'select',
      id: 'u-range',
      onchange: (e) => {
        rangeKey = e.target.value;
        customFields.hidden = rangeKey !== 'custom';
        onRangeChange();
      },
    },
    RANGES.map(([k, label]) => h('option', { value: k, selected: k === rangeKey }, label)),
  );
  const dateInput = (name, labelText) =>
    h(
      'label',
      { class: 'field' },
      h('span', null, labelText),
      h('input', {
        class: 'input',
        type: 'date',
        name,
        id: `u-${name}`,
        value: custom[name],
        max: todayIso,
        onchange: (e) => {
          custom = { ...custom, [name]: e.target.value };
          if (custom.since && custom.until && custom.since > custom.until) {
            toast('“From” must be on or before “To”.', 'error');
            return;
          }
          onRangeChange();
        },
      }),
    );
  const customFields = h('div', { class: 'u-custom', hidden: rangeKey !== 'custom' }, dateInput('since', 'From (UTC)'), dateInput('until', 'To (UTC, inclusive)'));
  const toolbar = h(
    'div',
    { class: 'u-toolbar' },
    h('label', { class: 'field u-range' }, h('span', null, filterCid ? 'Range (UTC)' : 'API cost range (UTC)'), rangeSelect),
    customFields,
    filterCid ? null : h('p', { class: 'hint u-toolbar-note', id: 'u-refresh-help' }, 'Refresh asks provider APIs and opted-in devices for new figures now. The hub also checks every hour (devices every 10 minutes).'),
  );

  const apiBody = h('div', { class: 'u-track-body', 'aria-busy': 'true' }, skeleton(3), h('div', { class: 'skeleton block' }));
  const subBody = h('div', { class: 'u-track-body', 'aria-busy': 'true' }, skeleton(3), h('div', { class: 'skeleton block' }));
  const breakdownBody = h('div', { 'aria-busy': 'true' }, skeleton(4));
  const limitsSlot = h('div', { 'aria-busy': 'true' }, skeleton(4));
  const chatStats = h('div', { class: 'u-chat-stats' });

  const groupSeg = h(
    'div',
    { class: 'seg', role: 'radiogroup', 'aria-label': 'Group breakdown by' },
    GROUPS.filter(([v]) => !(filterCid && v === 'conversation')).map(([v, label]) =>
      h(
        'label',
        null,
        h('input', {
          type: 'radio',
          name: 'usage-group',
          value: v,
          checked: group === v,
          onchange: () => {
            group = v;
            loadRows();
          },
        }),
        label,
      ),
    ),
  );
  const breakdownCaption = h('p', { class: 'section-note u-caption' });
  const breakdownHeading = h('h2', { id: 'u-breakdown-h', tabindex: '-1' }, filterCid ? 'Breakdown' : 'API cost breakdown');
  const filterSlot = h('div', { class: 'u-filter-slot' });
  // Says when the provider filter changes; the card buttons' pressed state says it too.
  const filterStatus = h('p', { class: 'visually-hidden', role: 'status' });
  const breakdown = h(
    'section',
    { class: 'section u-breakdown', 'aria-labelledby': 'u-breakdown-h' },
    h('div', { class: 'section-head' }, breakdownHeading, groupSeg),
    filterSlot,
    filterStatus,
    breakdownCaption,
    breakdownBody,
  );

  /** Filter the breakdown to one provider, or (null, or the same one again) show all. */
  function setProviderFilter(id) {
    const next = id && id !== providerFilter ? id : null;
    if (next === providerFilter) return;
    // A history entry per change, so Back undoes it; the route event applies it.
    navigate(`#/usage${qs({ conversation: filterCid, provider: next })}`);
  }

  /** Show the filter the URL names, in place: the chip, the cards' state, the rows. */
  function applyProviderFilter(id) {
    const next = id || null;
    if (next === providerFilter) return;
    const previous = providerFilter;
    const chipHadFocus = filterSlot.contains(document.activeElement);
    providerFilter = next;
    drawFilterChip();
    drawApi();
    setText(filterStatus, next ? `Breakdown filtered to ${next}.` : `Filter removed: the breakdown shows all providers.`);
    if (chipHadFocus) {
      // The chip that had focus is gone: go back to the card it came from.
      (document.getElementById(`u-filter-${slug(previous)}`) || breakdownHeading).focus();
    }
    loadRows();
  }

  // The route key ignores `provider=`, so this view stays and only the filter moves.
  const offRoute = on('route', (r) => {
    if (r.view === 'usage' && (r.conversation || null) === filterCid) applyProviderFilter(r.provider);
  });

  function drawFilterChip() {
    if (!providerFilter) {
      setChildren(filterSlot);
      filterSlot.hidden = true;
      return;
    }
    filterSlot.hidden = false;
    setChildren(
      filterSlot,
      h(
        'button',
        { class: 'u-filter-chip', type: 'button', id: 'u-filter-chip', onclick: () => setProviderFilter(null) },
        h('span', null, 'Filtered: ', h('strong', null, providerFilter)),
        h('span', { class: 'visually-hidden' }, ', remove filter'),
        icon('x'),
      ),
    );
  }
  drawFilterChip();

  if (filterCid) {
    body.append(
      h(
        'div',
        { class: 'banner info' },
        icon('chat'),
        h('div', { class: 'grow' }, 'Estimated API cost of this chat and its threads. Subscription quotas aren’t tracked per chat. '),
        h('a', { class: 'btn btn-sm', href: `#/chat/${enc(filterCid)}` }, icon('back'), 'Back to chat'),
      ),
      toolbar,
      chatStats,
      breakdown,
    );
  } else {
    body.append(
      toolbar,
      h(
        'div',
        { class: 'u-tracks' },
        h(
          'section',
          { class: 'u-track', 'aria-labelledby': 'u-api-h' },
          h('div', { class: 'u-track-head' }, h('h2', { id: 'u-api-h' }, icon('usage'), 'API costs'), h('span', { class: 'badge' }, 'per provider key')),
          h('p', { class: 'section-note' }, 'Grouper’s estimate counts the calls it routes, priced from the registry. Provider figures may include use outside Grouper, so the two overlap and aren’t added up.'),
          apiBody,
        ),
        h(
          'section',
          { class: 'u-track', 'aria-labelledby': 'u-sub-h' },
          h('div', { class: 'u-track-head' }, h('h2', { id: 'u-sub-h' }, icon('clock'), 'Subscription usage'), h('span', { class: 'badge warn' }, icon('warning'), 'Unofficial snapshot')),
          h('p', { class: 'section-note' }, 'Plan windows read on your devices from the Claude Code and Codex sign-ins. Not an official API, and not affected by the range.'),
          subBody,
        ),
      ),
      breakdown,
      h(
        'section',
        { class: 'section', 'aria-labelledby': 'u-limits' },
        h('div', { class: 'section-head' }, h('h2', { id: 'u-limits' }, 'Limits & budgets')),
        h('p', { class: 'section-note' }, 'All off until you switch them on. Budgets use Grouper’s estimated spend, not provider bills. Changes save when you leave a field.'),
        limitsSlot,
      ),
      h(
        'section',
        { class: 'section u-legend', 'aria-labelledby': 'u-legend-h' },
        h('h2', { id: 'u-legend-h' }, 'About these numbers'),
        h(
          'dl',
          null,
          h('dt', null, h('span', { class: 'badge u-est' }, 'estimated')),
          h('dd', null, 'Tokens from calls made through Grouper times the registry’s current prices. Calls made elsewhere are not seen.'),
          h('dt', null, h('span', { class: 'badge' }, 'reported by provider')),
          h('dd', null, 'Read from the provider’s own API with the key Grouper holds (OpenRouter and DeepSeek). May include use outside Grouper.'),
          h('dt', null, 'Free requests'),
          h('dd', null, 'OpenRouter’s daily free-model limit, as its last “limit reached” answer to Grouper reported it. Unknown until a call runs into it. “Sent by Grouper today” is Grouper’s own count, not the limit.'),
          h('dt', null, h('span', { class: 'badge warn' }, 'Unofficial snapshot')),
          h('dd', null, 'Read on a device from its Claude Code or Codex sign-in, through endpoints that aren’t a published API. May change or stop working; never a guaranteed limit.'),
          h('dt', null, h('span', { class: 'badge warn' }, 'Stale')),
          h('dd', null, 'Fetched more than an hour ago. The last value is kept, with its time.'),
        ),
      ),
    );
  }

  // ---- loading

  let apiSeq = 0;
  async function loadApi() {
    const seq = ++apiSeq;
    apiBody.setAttribute('aria-busy', 'true');
    try {
      const [p, u, f] = await Promise.all([
        load('/v1/providers', signal),
        load(`/v1/usage${qs({ ...rangeQuery(), group: 'provider' })}`, signal),
        load('/v1/usage/free-requests', signal),
      ]);
      if (seq !== apiSeq) return; // a newer range or refresh won
      st.providers = p;
      st.byProvider = u;
      st.free = f;
    } catch (err) {
      if (isAbort(err)) return;
      throw err;
    }
    drawApi();
  }

  async function loadSnaps() {
    subBody.setAttribute('aria-busy', 'true');
    try {
      st.snaps = await load('/v1/usage/snapshots', signal);
    } catch (err) {
      if (isAbort(err)) return;
      throw err;
    }
    drawApi();
    drawSubs();
  }

  let rowsSeq = 0;
  async function loadRows() {
    const seq = ++rowsSeq;
    const provider = providerFilter;
    breakdownBody.setAttribute('aria-busy', 'true');
    let res;
    try {
      res = await load(
        `/v1/usage${qs({ ...rangeQuery(), group, ...(filterCid ? { conversation: filterCid } : null), ...(provider ? { provider } : null) })}`,
        signal,
      );
    } catch (err) {
      if (isAbort(err)) return;
      throw err;
    }
    if (seq !== rowsSeq) return; // a newer range, grouping or filter won
    st.rows = { ...res, group, provider };
    drawBreakdown();
  }

  function onRangeChange() {
    if (!filterCid) loadApi();
    loadRows();
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
    if (!signal.aborted) {
      loadSnaps();
      loadApi();
    }
  }

  // ---- API costs track

  function drawApi() {
    const P = st.providers;
    const U = st.byProvider;
    const S = st.snaps;
    if (!P || !U || !S) return; // the first load of all three draws once
    const providers = Array.isArray(P.data) ? P.data : [];
    const usage = Array.isArray(U.data) ? U.data : [];
    const snaps = (Array.isArray(S.data) ? S.data : []).filter((s) => s.source !== 'probe');
    const byId = new Map(providers.map((p) => [p.id, p]));
    const usageBy = new Map(usage.map((r) => [r.key, r]));
    const ids = [...new Set([...providers.map((p) => p.id), ...usage.map((r) => r.key), ...snaps.map((s) => s.provider)])];
    ids.sort((a, b) => (usageBy.get(b)?.cost || 0) - (usageBy.get(a)?.cost || 0) || (usageBy.get(b)?.calls || 0) - (usageBy.get(a)?.calls || 0) || a.localeCompare(b));
    const total = sumRows(usage);
    const usageKnown = Boolean(U.data);
    const F = st.free;
    // Grouper's own count of `:free` calls today, per OpenRouter provider (null: didn't load).
    const freeSent = F?.data && Array.isArray(F.data.providers) ? new Map(F.data.providers.map((p) => [p.provider, p.sent])) : null;

    const summary = h(
      'div',
      { class: 'card u-summary' },
      h('div', { class: 'u-summary-k' }, `All provider keys, ${rangeLabel()} (UTC)`),
      usageKnown
        ? h(
            'div',
            { class: 'u-summary-v' },
            h('span', { class: 'u-big' }, usd(total.cost)),
            est(),
            h('span', { class: 'u-summary-s' }, `${plural(total.calls, 'call')} · ${num(tokensOf(total))} tokens`),
          )
        : h('div', { class: 'u-summary-v' }, 'Estimate unavailable'),
    );

    const cards = ids.map((id) =>
      providerCard(id, byId.get(id), usageBy.get(id), snaps.filter((s) => s.provider === id), usageKnown, P.data ? !byId.has(id) : false, {
        known: Boolean(freeSent),
        listed: freeSent?.has(id) || false,
        sent: freeSent?.get(id) || 0,
      }),
    );
    keepFocus(apiBody, () =>
      setChildren(
        apiBody,
        sourceProblem(P, 'the provider list', loadApi),
        sourceProblem(U, 'metered usage', loadApi),
        sourceProblem(S, 'provider snapshots', loadSnaps),
        sourceProblem(F, 'Grouper’s free-request count', loadApi),
        summary,
        cards.length
          ? h('div', { class: 'u-cards' }, cards)
          : h('div', { class: 'card card-pad u-empty' }, h('p', null, h('strong', null, 'No provider keys yet.')), h('p', { class: 'hint' }, 'Add a provider and its key on the Models page; its costs show here once Grouper routes calls through it.'), h('a', { class: 'btn btn-sm', href: '#/models' }, 'Open Models')),
      ),
    );
    apiBody.setAttribute('aria-busy', 'false');
  }

  function providerCard(id, info, row, snaps, usageKnown, unlisted, free) {
    const headId = `u-prov-${slug(id)}`;
    const api = balanceApi(info);
    const host = info ? hostOf(info) : '';
    const local = host && isLocalHost(host);
    const sub = [];
    if (info) {
      sub.push(api ? api.name : info.kind === 'scripted' ? 'Scripted test provider' : info.kind === 'anthropic' ? 'Anthropic' : info.kind === 'openai' ? 'OpenAI-compatible' : info.kind);
      if (host) sub.push(host);
      if (info.via_device) sub.push(`via ${deviceName(info.via_device)}`);
    }
    const badges = [];
    if (unlisted) badges.push(h('span', { class: 'badge muted' }, 'Not in provider list'));
    else if (info && !info.has_key && !local && info.kind !== 'scripted' && !info.via_device) badges.push(h('span', { class: 'badge muted' }, 'No key'));
    if (local) badges.push(h('span', { class: 'badge muted' }, 'Local'));

    // Grouper's estimate
    const estimate = h(
      'div',
      { class: 'u-block' },
      h('h4', null, 'Grouper estimate'),
      !usageKnown
        ? h('p', { class: 'hint' }, 'Unavailable: metered usage didn’t load.')
        : h(
            'div',
            null,
            h('div', { class: 'u-figure' }, h('span', { class: 'u-big' }, usd(row?.cost || 0)), est()),
            row
              ? h(
                  'dl',
                  { class: 'u-facts' },
                  h('div', null, h('dt', null, 'Calls'), h('dd', null, num(row.calls))),
                  h('div', null, h('dt', null, 'Tokens'), h('dd', null, num(tokensOf(row)))),
                  h('div', null, h('dt', null, 'In / out'), h('dd', null, `${num(row.input_tokens)} / ${num(row.output_tokens)}`)),
                  row.cache_read_tokens || row.cache_write_tokens ? h('div', null, h('dt', null, 'Cache read / write'), h('dd', null, `${num(row.cache_read_tokens)} / ${num(row.cache_write_tokens)}`)) : null,
                )
              : h('p', { class: 'hint' }, `No calls through Grouper ${inRange()}.`),
          ),
    );

    // What the provider itself reports
    const reported = snaps.filter((s) => s.kind === 'spend' || s.kind === 'balance');
    const allowances = snaps.filter((s) => s.kind !== 'spend' && s.kind !== 'balance' && s.kind !== 'status');
    let reportedBody;
    if (reported.length) {
      const oldest = Math.min(...reported.map((s) => s.fetched_at));
      reportedBody = [reported.map(reportedRow), ageLine(oldest, api ? api.source : 'provider API')];
    } else if (api && info?.has_key) {
      reportedBody = h('p', { class: 'hint' }, `Not fetched yet. ${api.name} spend and balance arrive on the hourly check, or press Refresh.`);
    } else if (api) {
      reportedBody = h('p', { class: 'hint' }, 'No key set, so there’s nothing to read.');
    } else if (info?.kind === 'scripted') {
      reportedBody = h('p', { class: 'hint' }, 'Test provider: nothing is billed.');
    } else if (local) {
      reportedBody = h('p', { class: 'hint' }, 'Runs locally: no provider bill.');
    } else {
      reportedBody = h('p', { class: 'hint' }, 'This provider doesn’t report spend to Grouper. Grouper reads OpenRouter and DeepSeek balances only.');
    }
    const reportedBlock = h('div', { class: 'u-block' }, h('h4', null, 'Reported by provider'), reportedBody);

    // Free allowances: only what the data actually says.
    const openRouter = free.listed || isOpenRouter(info);
    const dailyFree = snaps.find(isFreeAllowance);
    const otherAllowances = allowances.filter((s) => s !== dailyFree);
    let allowanceBlock = null;
    if (openRouter) {
      allowanceBlock = h('div', { class: 'u-block' }, h('h4', null, 'Free allowance'), freeAllowance(dailyFree, free), otherAllowances.map(reportedRow));
    } else if (otherAllowances.length) {
      allowanceBlock = h('div', { class: 'u-block' }, h('h4', null, 'Free allowance'), otherAllowances.map(reportedRow), ageLine(Math.min(...otherAllowances.map((s) => s.fetched_at))));
    }

    // Filter the breakdown to this provider: the button for keyboards and screen
    // readers (its pressed state is announced), a click anywhere else on the card too.
    const filtered = providerFilter === id;
    const filterBtn = h(
      'button',
      {
        class: 'btn btn-sm u-filter-btn',
        type: 'button',
        id: `u-filter-${slug(id)}`,
        'aria-pressed': filtered ? 'true' : 'false',
        onclick: () => setProviderFilter(id),
      },
      icon(filtered ? 'check' : 'usage'),
      'Filter',
      h('span', { class: 'visually-hidden' }, ` breakdown to ${id}`),
    );

    return h(
      'article',
      {
        class: ['card u-card u-provider', filtered && 'is-filtered'],
        'aria-labelledby': headId,
        dataset: { provider: id },
        onclick: (e) => {
          if (e.target.closest('a, button, input, select, textarea, summary')) return;
          if (window.getSelection?.()?.toString()) return; // selecting text isn't a click
          setProviderFilter(id);
        },
      },
      h(
        'header',
        { class: 'u-card-head' },
        h('div', { class: 'u-card-titles' }, h('h3', { id: headId }, id), sub.length ? h('div', { class: 'u-card-sub' }, sub.join(' · ')) : null),
        h('div', { class: 'u-badges' }, badges, filterBtn),
      ),
      estimate,
      reportedBlock,
      allowanceBlock,
    );
  }

  /**
   * OpenRouter's daily free-model allowance: what its last 429 said (never a
   * built-in number), then Grouper's own count of free-model calls today.
   */
  function freeAllowance(s, free) {
    const parts = [];
    if (s) {
      const limit = s.limit_amount ?? null;
      const used = s.used_amount ?? null;
      const left = limit !== null && used !== null ? Math.max(0, limit - used) : s.used_pct >= 100 ? 0 : null;
      const reset = s.resets_at || null;
      // With no reset time, a reading is only trusted for the UTC day it was taken in
      // (OpenRouter's free allowance is daily).
      const passed = reset !== null ? reset <= nowSecs() : Math.floor(s.fetched_at / 86400) < Math.floor(nowSecs() / 86400);
      if (!passed) {
        const value = [
          left !== null ? (limit !== null ? `${num(left)} left of ${num(limit)}` : `${num(left)} left`) : limit !== null ? `limit ${num(limit)}` : 'limit reached',
          reset ? `resets ${clock(reset)}` : null,
        ]
          .filter(Boolean)
          .join(' · ');
        parts.push(
          h('div', { class: 'u-rep' }, h('div', { class: 'u-rep-line' }, h('span', { class: 'u-rep-k' }, 'Free requests'), h('span', { class: 'u-rep-v' }, value))),
          s.used_pct !== null && s.used_pct !== undefined ? meter(s.used_pct, `${Math.round(s.used_pct)}% of today’s free requests used`) : null,
          reset ? h('div', { class: 'u-meta' }, `Resets ${inDuration(reset - nowSecs())} (${dateTime(reset)})`) : h('div', { class: 'u-meta' }, 'Reset time not reported'),
        );
      } else {
        parts.push(
          h(
            'div',
            { class: 'u-rep' },
            h('div', { class: 'u-rep-line' }, h('span', { class: 'u-rep-k' }, 'Free requests'), h('span', { class: 'u-rep-v' }, limit !== null ? `${num(limit)} a day` : 'Limit not reported')),
            h(
              'div',
              { class: 'u-meta' },
              reset !== null
                ? `Ran out ${ago(s.fetched_at)}; it reset at ${clock(reset)} (${dateTime(reset)}), so what’s left now isn’t known.`
                : `Ran out ${ago(s.fetched_at)}, before today (UTC), so what’s left now isn’t known.`,
            ),
          ),
        );
      }
      parts.push(h('div', { class: 'u-meta' }, icon('clock'), h('span', null, `Seen ${ago(s.fetched_at)} · ${s.source}`)));
    } else {
      parts.push(h('p', { class: 'hint' }, 'Allowance unavailable: OpenRouter reports its free-request limit only when a call runs into it.'));
    }
    const count = free.known ? num(free.sent) : 'unavailable';
    parts.push(
      h(
        'div',
        { class: 'u-rep u-free-sent' },
        h('div', { class: 'u-rep-line' }, h('span', { class: 'u-rep-k' }, 'Sent by Grouper today'), h('span', { class: 'u-rep-v' }, count)),
        h('div', { class: 'u-meta' }, 'Grouper’s own count of answered calls to :free models since 00:00 UTC. Not the allowance: other apps on the account aren’t seen.'),
      ),
    );
    return parts;
  }

  function reportedRow(s) {
    const windowWords = { monthly: 'this month', weekly: 'this week', daily: 'today', total: 'all time', current: 'now' }[s.window] || s.window;
    let label;
    let value;
    let pct = s.used_pct;
    if (s.kind === 'spend') {
      label = `Key spend, ${windowWords}`;
      value = s.limit_amount !== null && s.limit_amount !== undefined ? `${money(s.used_amount, s.currency)} of ${money(s.limit_amount, s.currency)} limit` : `${money(s.used_amount, s.currency)} (no limit on the key)`;
    } else if (s.kind === 'balance' && s.limit_amount !== null && s.limit_amount !== undefined) {
      label = 'Credits';
      const left = s.limit_amount - (s.used_amount || 0);
      value = `${money(left, s.currency)} left of ${money(s.limit_amount, s.currency)}`;
    } else if (s.kind === 'balance') {
      label = 'Balance';
      value = money(s.used_amount, s.currency);
      pct = null;
    } else {
      label = `${s.kind}${s.window ? `, ${windowWords}` : ''}`;
      value =
        s.limit_amount !== null && s.limit_amount !== undefined
          ? `${money(s.used_amount, s.currency)} used of ${money(s.limit_amount, s.currency)}`
          : s.used_amount !== null && s.used_amount !== undefined
            ? money(s.used_amount, s.currency)
            : '';
    }
    const resets = s.resets_at ? h('div', { class: 'u-meta' }, `Resets ${inDuration(s.resets_at - nowSecs())} (${dateTime(s.resets_at)})`) : null;
    return h(
      'div',
      { class: 'u-rep' },
      h('div', { class: 'u-rep-line' }, h('span', { class: 'u-rep-k' }, label), h('span', { class: 'u-rep-v' }, value || '—')),
      pct !== null && pct !== undefined ? meter(pct, `${Math.round(pct)}% used`) : null,
      resets,
    );
  }

  // ---- subscription track

  function drawSubs() {
    const S = st.snaps;
    if (!S || filterCid) return;
    const probe = (Array.isArray(S.data) ? S.data : []).filter((s) => s.source === 'probe');
    const statuses = probe.filter((s) => s.kind === 'status').sort((a, b) => compareTuples(windowOrder(a), windowOrder(b)));
    const windows = probe.filter((s) => s.kind !== 'status').sort((a, b) => compareTuples(windowOrder(a), windowOrder(b)));
    let content;
    if (!windows.length && !statuses.length) {
      content = S.data
        ? h(
            'div',
            { class: 'card card-pad u-empty' },
            h('p', null, h('strong', null, 'No plan windows reported yet.')),
            h('p', null, 'A device reports its Claude and ChatGPT (Codex) plan usage when usage probes are on. On a device signed in to Claude Code or Codex, connect with:'),
            h('pre', { class: 'u-cmd' }, h('code', null, 'grouper connect --usage-probes')),
            h('p', { class: 'hint' }, 'Or set ', h('code', null, 'GROUPER_USAGE_PROBES=true'), ' before ', h('code', null, 'grouper connect'), '. Off by default: it reads that CLI’s own sign-in on the device and calls unofficial endpoints; the sign-in token never leaves the device. Readings arrive when the device connects, every 10 minutes, and after a Claude or Codex thread.'),
          )
        : null;
    } else {
      content = [
        statuses.length
          ? h(
              'ul',
              { class: 'u-statuses' },
              statuses.map((s) =>
                h(
                  'li',
                  { class: 'banner warn' },
                  icon('warning'),
                  h('div', { class: 'grow' }, h('strong', null, `${PLAN_PROVIDERS[s.provider] || s.provider} on ${deviceName(s.device_id)}: `), s.window || 'unavailable', h('span', { class: 'u-meta-inline' }, ` · ${ago(s.fetched_at)}`)),
                ),
              ),
            )
          : null,
        windows.length ? h('div', { class: 'u-subgrid' }, windows.map(planCard)) : null,
      ];
    }
    keepFocus(subBody, () => setChildren(subBody, sourceProblem(S, 'subscription snapshots', loadSnaps), content));
    subBody.setAttribute('aria-busy', 'false');
  }

  function planCard(s, i) {
    const headId = `u-plan-${i}`;
    const device = s.device_id ? deviceName(s.device_id) : 'unknown device';
    const offline = s.device_id && !deviceOnline(s.device_id);
    let figure;
    if (s.kind === 'window' && s.used_pct !== null && s.used_pct !== undefined) {
      const used = Math.round(s.used_pct);
      figure = meter(s.used_pct, h('span', null, h('strong', { class: 'u-num' }, `${used}% used`), ` · ${Math.max(0, 100 - used)}% left`));
    } else if (s.kind === 'spend') {
      figure = [
        h('div', { class: 'u-rep-line' }, h('span', { class: 'u-rep-k' }, 'Used'), h('span', { class: 'u-rep-v' }, s.limit_amount !== null && s.limit_amount !== undefined ? `${money(s.used_amount, s.currency)} of ${money(s.limit_amount, s.currency)}` : money(s.used_amount, s.currency))),
        s.used_pct !== null && s.used_pct !== undefined ? meter(s.used_pct, `${Math.round(s.used_pct)}% used`) : null,
      ];
    } else if (s.kind === 'balance') {
      figure = h('div', { class: 'u-rep-line' }, h('span', { class: 'u-rep-k' }, 'Left'), h('span', { class: 'u-rep-v' }, money(s.used_amount, s.currency)));
    } else {
      figure = h('p', { class: 'hint' }, 'No usage figure in this reading.');
    }
    return h(
      'article',
      { class: 'card u-card u-plan', 'aria-labelledby': headId },
      h('header', { class: 'u-card-head' }, h('div', { class: 'u-card-titles' }, h('h3', { id: headId }, planName(s)), h('div', { class: 'u-card-sub' }, windowName(s)))),
      figure,
      s.resets_at
        ? h('div', { class: 'u-meta' }, icon('refresh'), h('span', null, `${s.resets_at > nowSecs() ? `Resets ${inDuration(s.resets_at - nowSecs())}` : 'Reset time passed'} · ${dateTime(s.resets_at)}`))
        : h('div', { class: 'u-meta' }, icon('refresh'), h('span', null, 'Reset time not reported')),
      h(
        'div',
        { class: 'u-meta' },
        icon('devices'),
        h('span', null, `Read on ${device} · ${ago(s.fetched_at)}`),
        nowSecs() - s.fetched_at > STALE_SECS ? h('span', { class: 'badge warn' }, `Stale · ${dateTime(s.fetched_at)}`) : null,
        offline ? h('span', { class: 'badge muted' }, icon('offline'), 'device offline') : null,
      ),
    );
  }

  // ---- breakdown

  /**
   * The registry id behind a usage row's model. Usage records the provider's model
   * string (what answered), so match on that, within the provider filter; a string two
   * entries share stays ambiguous (null), and so does one no entry has.
   */
  function registryIdFor(model, provider) {
    const pool = store.models.filter((m) => !provider || m.provider_id === provider);
    const byModel = pool.filter((m) => m.model === model);
    if (byModel.length === 1) return byModel[0].id;
    return pool.find((m) => m.id === model)?.id || null;
  }

  function rowLabel(r, g) {
    const key = r.key;
    if (g === 'conversation') {
      if (!key || key === '(none)') return h('span', null, 'No chat');
      const title = store.conversations.find((c) => c.id === key)?.title;
      return h('a', { href: `#/chat/${enc(key)}` }, title || h('span', { class: 'mono' }, key));
    }
    if (g === 'thread') {
      if (!key || key === '(orchestrator)') return h('span', null, filterCid ? 'Chat replies (orchestrator or standard)' : 'Chat replies, no thread');
      const t = store.threads.get(key);
      const href = t?.conversation_id ? `#/chat/${enc(t.conversation_id)}/thread/${enc(key)}` : `#/thread/${enc(key)}`;
      return h('a', { href }, t?.title || h('span', { class: 'mono' }, key));
    }
    if (g === 'model') {
      const fam = familyKey(key);
      const id = registryIdFor(key, st.rows?.provider);
      const link = id ? { href: `#/models/${enc(id)}`, title: `Open ${id} in Models` } : { href: '#/models', title: 'Open Models' };
      return h('a', { ...link, class: 'u-model' }, familyMark(fam), h('span', { class: 'mono' }, key), h('span', { class: 'visually-hidden' }, ` (${familyLabel(fam)})`));
    }
    if (g === 'provider') return h('span', { class: 'u-provider-name' }, key);
    return h('span', { class: 'mono' }, key);
  }

  function shareCell(cost, total) {
    const share = total > 0 ? (cost / total) * 100 : 0;
    const fill = h('span', { class: 'u-share-fill' });
    fill.style.width = `${Math.min(100, share)}%`;
    return h('span', { class: 'u-share' }, h('span', { class: 'u-share-bar', 'aria-hidden': 'true' }, fill), `${share < 1 && share > 0 ? '<1' : Math.round(share)}%`);
  }

  function drawBreakdown() {
    const R = st.rows;
    if (!R) return;
    const g = R.group;
    const gLabel = GROUPS.find(([v]) => v === g)[1];
    const rows = (Array.isArray(R.data) ? R.data : []).slice();
    if (g === 'day') rows.sort((a, b) => b.key.localeCompare(a.key));
    else rows.sort((a, b) => b.cost - a.cost || b.calls - a.calls || a.key.localeCompare(b.key));
    const total = sumRows(rows);
    const scope = filterCid ? 'This chat' : R.provider ? `Provider ${R.provider} only` : 'All provider keys';
    setText(breakdownCaption, `${scope} · ${rangeLabel()} (UTC) · grouped by ${gLabel.toLowerCase()}. Costs are Grouper’s estimate.`);

    if (filterCid) drawChatStats(R.data ? total : null);

    let content;
    if (!R.data) {
      content = null;
    } else if (!rows.length) {
      content = h(
        'div',
        { class: 'card card-pad u-empty' },
        h('p', null, h('strong', null, `No metered calls${R.provider ? ` through ${R.provider}` : ''} ${inRange()}.`)),
        h('p', { class: 'hint' }, 'Grouper counts only the calls it routes itself. Use outside Grouper shows up only in provider-reported figures.'),
        rangeKey !== 'all'
          ? h(
              'button',
              {
                class: 'btn btn-sm',
                type: 'button',
                onclick: () => {
                  rangeKey = 'all';
                  rangeSelect.value = 'all';
                  customFields.hidden = true;
                  onRangeChange();
                },
              },
              'Show all time',
            )
          : null,
      );
    } else {
      const cols = ['Calls', 'Input', 'Output', 'Cache read', 'Cache write'];
      const cells = (r) => [
        h('td', { class: 'num' }, num(r.calls)),
        h('td', { class: 'num' }, num(r.input_tokens)),
        h('td', { class: 'num' }, num(r.output_tokens)),
        h('td', { class: 'num' }, num(r.cache_read_tokens)),
        h('td', { class: 'num' }, num(r.cache_write_tokens)),
        h('td', { class: 'num' }, usd(r.cost)),
      ];
      const table = h(
        'div',
        { class: 'table-wrap u-table', tabindex: '0', role: 'region', 'aria-label': `Breakdown table by ${gLabel.toLowerCase()}` },
        h(
          'table',
          { class: 'table' },
          h('caption', { class: 'visually-hidden' }, `Estimated API cost by ${gLabel.toLowerCase()}${R.provider ? `, provider ${R.provider} only` : ''}, ${rangeLabel()}`),
          h(
            'thead',
            null,
            h(
              'tr',
              null,
              h('th', { scope: 'col' }, gLabel),
              cols.map((t) => h('th', { class: 'num', scope: 'col' }, t)),
              h('th', { class: 'num', scope: 'col' }, 'Cost (estimated)'),
              h('th', { class: 'num', scope: 'col' }, 'Share of cost'),
            ),
          ),
          h(
            'tbody',
            null,
            rows.map((r) => h('tr', null, h('th', { scope: 'row', class: 'wrap u-rowhead' }, rowLabel(r, g)), cells(r), h('td', { class: 'num' }, shareCell(r.cost, total.cost)))),
          ),
          h('tfoot', null, h('tr', null, h('th', { scope: 'row' }, 'Total'), cells(total), h('td', { class: 'num' }, total.cost > 0 ? '100%' : '—'))),
        ),
      );
      // On a phone each row is a labelled card instead of a wide table.
      const fact = (k, v) => h('div', null, h('dt', null, k), h('dd', null, v));
      const cardList = h(
        'ul',
        { class: 'u-rowcards', 'aria-label': `Estimated API cost by ${gLabel.toLowerCase()}` },
        h('li', { class: 'card u-rowcard u-rowcard-total' }, h('div', { class: 'u-rowcard-head' }, h('strong', null, 'Total'), h('span', { class: 'u-rowcard-cost' }, usd(total.cost), ' ', est())), h('dl', { class: 'u-facts' }, fact('Calls', num(total.calls)), fact('Tokens', num(tokensOf(total))))),
        rows.map((r) =>
          h(
            'li',
            { class: 'card u-rowcard' },
            h('div', { class: 'u-rowcard-head' }, h('span', { class: 'u-rowcard-label' }, rowLabel(r, g)), h('span', { class: 'u-rowcard-cost' }, usd(r.cost), ' ', est())),
            h(
              'dl',
              { class: 'u-facts' },
              fact('Calls', num(r.calls)),
              fact('Input', num(r.input_tokens)),
              fact('Output', num(r.output_tokens)),
              r.cache_read_tokens || r.cache_write_tokens ? fact('Cache read / write', `${num(r.cache_read_tokens)} / ${num(r.cache_write_tokens)}`) : null,
              fact('Share of cost', shareCell(r.cost, total.cost)),
            ),
          ),
        ),
      );
      content = [table, cardList];
    }
    keepFocus(breakdownBody, () => setChildren(breakdownBody, sourceProblem(R, 'the breakdown', loadRows), content));
    breakdownBody.setAttribute('aria-busy', 'false');
  }

  function drawChatStats(total) {
    if (!total) {
      setChildren(chatStats);
      return;
    }
    setChildren(
      chatStats,
      h(
        'div',
        { class: 'stat-grid' },
        h('div', { class: 'card stat' }, h('div', { class: 'k' }, 'Chat total'), h('div', { class: 'v' }, usd(total.cost)), h('div', { class: 's' }, est(), ` · ${rangeLabel()}`)),
        h('div', { class: 'card stat' }, h('div', { class: 'k' }, 'Calls'), h('div', { class: 'v' }, num(total.calls)), h('div', { class: 's' }, 'through Grouper')),
        h('div', { class: 'card stat' }, h('div', { class: 'k' }, 'Tokens'), h('div', { class: 'v' }, num(tokensOf(total))), h('div', { class: 's' }, `${num(total.input_tokens)} in · ${num(total.output_tokens)} out`)),
      ),
    );
  }

  // ---- limits
  // The form is built once; saves update it in place (a field being typed in is
  // never replaced), and only the stat cards are redrawn.
  let limitsForm = null; // {stats, rows: Map(key -> row), action}
  let lastReport = null;

  async function renderLimits() {
    limitsForm = null;
    setChildren(limitsSlot, skeleton(4));
    limitsSlot.setAttribute('aria-busy', 'true');
    let res;
    try {
      res = await load('/v1/limits', signal);
    } catch (err) {
      if (isAbort(err)) return;
      throw err;
    }
    if (!res.data) {
      setChildren(limitsSlot, sourceProblem(res, 'limits', renderLimits));
      limitsSlot.setAttribute('aria-busy', 'false');
      return;
    }
    drawLimits(res.data, res);
  }

  async function refreshLimits() {
    if (!limitsForm) return;
    try {
      const res = await load('/v1/limits', signal);
      if (res.data && !res.error) drawLimits(res.data);
    } catch {
      /* aborted */
    }
  }

  function drawLimits(report, res = null) {
    lastReport = report;
    if (limitsForm) {
      limitsForm.stats.replaceWith((limitsForm.stats = statCards(report)));
      for (const spec of LIMITS) limitsForm.rows.get(spec.key).set(report.limits[spec.key], report);
      if (document.activeElement !== limitsForm.action) limitsForm.action.value = report.limits.approval_timeout_action;
      return;
    }
    const l = report.limits;
    const stats = statCards(report);
    const rows = new Map(LIMITS.map((spec) => [spec.key, limitRow(spec, l[spec.key], report)]));
    const actionSelect = h(
      'select',
      {
        class: 'select',
        name: 'approval_timeout_action',
        id: 'limit-timeout-action',
        'data-write': true,
        onchange: (e) => save({ approval_timeout_action: e.target.value }),
      },
      h('option', { value: 'deny', selected: l.approval_timeout_action === 'deny' }, 'Deny the call; the thread goes on'),
      h('option', { value: 'stop', selected: l.approval_timeout_action === 'stop' }, 'Deny and stop the thread'),
    );
    const action = h('label', { class: 'field', for: 'limit-timeout-action' }, h('span', null, 'When an approval times out'), actionSelect);
    setChildren(limitsSlot, res ? sourceProblem(res, 'limits', renderLimits) : null, stats, h('ul', { class: 'list card' }, [...rows.values()].map((r) => h('li', null, r.el))), h('div', { class: 'card card-pad limit-action' }, action));
    limitsSlot.setAttribute('aria-busy', 'false');
    limitsForm = { stats, rows, action: actionSelect };
  }

  function statCards(report) {
    const l = report.limits;
    const daily = l.daily_budget_usd;
    const cap = l.max_running_threads;
    return h(
      'div',
      { class: 'stat-grid' },
      h(
        'div',
        { class: 'card stat' },
        h('div', { class: 'k' }, 'Spent today (UTC)'),
        h('div', { class: 'v' }, usd(report.spent_today_usd), ' ', est()),
        h('div', { class: 's' }, daily ? `of ${usd(daily)} daily budget · resets ${inDuration(report.day_resets_at - nowSecs())}` : `No daily budget set · day resets ${inDuration(report.day_resets_at - nowSecs())}`),
        daily ? meter((report.spent_today_usd / daily) * 100, `${Math.round((report.spent_today_usd / daily) * 100)}% of the daily budget`) : null,
      ),
      h(
        'div',
        { class: 'card stat' },
        h('div', { class: 'k' }, 'Running threads'),
        h('div', { class: 'v' }, num(report.running_threads)),
        h('div', { class: 's' }, cap ? `of a cap of ${cap}` : 'No cap set'),
        cap ? meter((report.running_threads / cap) * 100, `${report.running_threads} of ${cap} running`) : null,
      ),
      h('div', { class: 'card stat' }, h('div', { class: 'k' }, 'Warning at'), h('div', { class: 'v' }, `${Math.round(report.warn_at_fraction * 100)}%`), h('div', { class: 's' }, l.warn_at_fraction === null || l.warn_at_fraction === undefined ? 'of each budget (default)' : 'of each budget')),
    );
  }

  /** What a limit is doing right now, in words, from the latest report. */
  function consumption(spec, value, report) {
    if (value === null || value === undefined) return null;
    if (spec.key === 'daily_budget_usd') return `Today: ${usd(report.spent_today_usd)} estimated (${Math.round((report.spent_today_usd / value) * 100)}%)`;
    if (spec.key === 'max_running_threads') return `Running now: ${num(report.running_threads)} of ${value}`;
    if (spec.key === 'conversation_budget_usd') return 'Checked per chat; see a chat’s usage for its spend.';
    if (spec.key === 'thread_budget_usd') return 'Checked per thread as it runs.';
    return null;
  }

  /**
   * One limit: an On/Off state, an "Enabled" switch (off means null: no limit, or
   * the default) and the number with its unit beside it. Returns {el, set(value,
   * report)}; `set` updates the row from a saved report without touching the
   * controls of a row the person is editing.
   */
  function limitRow(spec, value, report) {
    const id = `limit-${spec.key}`;
    const scale = spec.scale || 1;
    const shown = (v) => (v === null || v === undefined ? '' : String(Math.round(v * scale * 1000) / 1000));
    // Live refreshes redraw every row. While focus is in this row, or the switch was
    // turned on here and no number is saved yet (`pending`), they leave its switch and
    // field alone. Focus leaving the row with the field still empty cancels the switch.
    let pending = false;
    let last = [value, report];
    const input = h('input', {
      class: 'input',
      id,
      type: 'number',
      min: '0',
      step: spec.step,
      max: spec.max || null,
      inputmode: spec.step === '1' ? 'numeric' : 'decimal',
      'data-write': true,
      'aria-describedby': `${id}-help ${id}-now`,
      onchange: () => {
        if (input.value === '') return;
        const n = Number(input.value);
        if (!Number.isFinite(n) || n < 0 || (spec.max && n > Number(spec.max))) {
          toast(`${spec.label} must be ${spec.max ? `between 0 and ${spec.max}` : 'zero or more'}.`, 'error');
          return;
        }
        const stored = n / scale;
        const saved = spec.step === '1' ? Math.round(stored) : stored;
        pending = false;
        last = [saved, last[1]]; // until the save's report arrives
        save({ [spec.key]: saved });
      },
    });
    const enabled = h('input', {
      type: 'checkbox',
      role: 'switch',
      'data-write': true,
      'aria-label': `${spec.label} enabled`,
      onchange: () => {
        if (enabled.checked) {
          pending = true;
          input.disabled = false;
          input.placeholder = '';
          input.focus();
        } else {
          pending = false;
          last = [null, last[1]];
          input.disabled = true;
          save({ [spec.key]: null });
        }
      },
    });
    const field = h(
      'div',
      { class: 'unit-field' },
      spec.prefix ? h('span', { class: 'unit', 'aria-hidden': 'true' }, spec.prefix) : null,
      input,
      spec.suffix ? h('span', { class: 'unit', 'aria-hidden': 'true' }, spec.suffix) : null,
    );
    const state = h('span', { class: 'badge' });
    const now = h('div', { class: 'limit-now', id: `${id}-now` });
    const set = (v, r) => {
      last = [v, r];
      const isOn = v !== null && v !== undefined;
      if (!pending && !el.contains(document.activeElement)) {
        enabled.checked = isOn;
        input.disabled = !isOn || 'offlineDisabled' in input.dataset;
        input.placeholder = isOn ? '' : spec.off.replace(/^Off: /, '');
        input.value = shown(v);
      }
      state.className = isOn ? 'badge accent' : 'badge muted';
      setText(state, isOn ? 'On' : spec.off.startsWith('Default') ? 'Default' : 'Off');
      const words = r ? consumption(spec, v, r) : null;
      setText(now, words || '');
      now.hidden = !words;
    };
    const el = h(
      'div',
      {
        class: 'limit-row',
        onfocusout: (e) => {
          if (el.contains(e.relatedTarget)) return;
          if (pending && input.value === '') pending = false; // switched on, nothing entered
          if (!pending) set(...last);
        },
      },
      h(
        'div',
        null,
        h('div', { class: 'limit-title' }, h('label', { class: 'limit-name', for: id }, spec.label, h('span', { class: 'visually-hidden' }, ` in ${spec.unit}`)), state),
        h('div', { class: 'limit-help', id: `${id}-help` }, spec.help, ' ', h('span', { class: 'limit-off' }, `Switched off: ${spec.off.replace(/^Off: /, '').replace(/^Default: /, 'default ')}.`)),
        now,
      ),
      field,
      h('label', { class: 'switch' }, enabled, 'Enabled'),
    );
    set(value, report);
    return { el, set };
  }

  async function save(changes) {
    try {
      const report = await put('/v1/limits', changes);
      toast('Limits saved.');
      if (!signal.aborted) drawLimits(report);
    } catch (err) {
      toast(describe(err), 'error');
      if (!signal.aborted) renderLimits();
    }
  }

  // ---- live updates: in place, never moving focus or scroll

  const metered = throttle(() => {
    if (signal.aborted || isOffline()) return;
    if (!filterCid) loadApi();
    loadRows();
    refreshLimits();
  }, 4000);
  const offEvent = on('event', (e) => {
    if (!e || (filterCid && e.conversation_id !== filterCid)) return;
    const usageEvent = e.kind === 'thread_activity' && e.event?.type === 'usage';
    if (usageEvent || e.kind === 'turn_finished' || e.kind === 'thread_finished' || e.kind === 'budget_warning') metered();
  });
  const relabel = throttle(() => {
    if (st.rows && (group === 'conversation' || group === 'thread')) drawBreakdown();
  }, 1000);
  const offConvs = on('conversations', () => {
    if (filterCid) renderHeader();
    relabel();
  });
  const offThreads = on('threads', relabel);
  const offModels = on('models', () => {
    if (st.rows?.group === 'model') drawBreakdown();
  });
  const offDevices = on('devices', () => drawSubs());
  const offOnline = on('online', (online) => {
    syncRefresh();
    if (online && !signal.aborted) {
      // Back online: replace any cached copies with fresh reads.
      if (!filterCid) {
        loadApi();
        loadSnaps();
        refreshLimits();
      }
      loadRows();
    }
  });
  // Snapshot ages and reset countdowns move on their own: reread every minute.
  const minute = setInterval(() => {
    if (document.hidden || signal.aborted || filterCid) return;
    if (isOffline()) {
      drawApi();
      drawSubs();
    } else loadSnaps();
    if (lastReport && limitsForm) limitsForm.stats.replaceWith((limitsForm.stats = statCards(lastReport)));
  }, 60000);

  if (!filterCid) {
    renderLimits();
    loadSnaps();
    loadApi();
  }
  loadRows();
  return () => {
    aborter.abort();
    unguard();
    metered.cancel();
    relabel.cancel();
    clearInterval(minute);
    offEvent();
    offConvs();
    offThreads();
    offDevices();
    offOnline();
    offRoute();
    offModels();
  };
}
