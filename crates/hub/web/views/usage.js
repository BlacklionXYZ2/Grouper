// Usage and limits: metered totals, provider balances, and the hub's limits
// (thread caps, budgets, timeouts) with today's estimated spend.

import { h, icon, setChildren } from '../lib/dom.js';
import { get, post, put, qs, enc, describe, isAbort } from '../lib/api.js';
import { store, on, isAdmin } from '../lib/store.js';
import { bar, toast, toastError, skeleton, errorBanner, guardWrites } from '../lib/ui.js';
import { num, usd, ago, dateTime, isoDay } from '../lib/format.js';

const GROUPS = [
  ['day', 'Day'],
  ['provider', 'Provider'],
  ['model', 'Model'],
  ['conversation', 'Chat'],
  ['thread', 'Thread'],
];

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

export function renderUsage(container, { conversation: filterCid = null } = {}) {
  const admin = isAdmin();
  const barSlot = h('div', { class: 'bar-slot' });
  const body = h('div', { class: 'view-body' });
  container.append(h('section', { class: 'view' }, barSlot, body));

  const filterBanner = h('div', { hidden: !filterCid });
  const chatTitle = () => (filterCid ? store.conversations.find((c) => c.id === filterCid)?.title || filterCid : null);

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
      setChildren(
        filterBanner,
        h(
          'div',
          { class: 'banner info' },
          icon('chat'),
          h('div', { class: 'grow' }, 'Showing metered usage for ', h('strong', null, name), '.'),
          h('a', { class: 'btn btn-sm', href: '#/usage' }, 'Show all'),
        ),
      );
      document.title = `${name} · Usage · Grouper`;
    } else {
      setChildren(barSlot, bar({ title: 'Usage & limits', caption: 'Spend is estimated from registry prices' }));
      document.title = 'Usage & limits · Grouper';
    }
  }
  renderHeader();

  if (!admin) {
    body.append(h('div', { class: 'banner warn' }, icon('shield'), 'Usage and limits need an admin token. Sign in with one in Settings.'));
    return null;
  }
  // Leaving the view cancels its reads; a write in flight still finishes.
  const aborter = new AbortController();
  const { signal } = aborter;
  const unguard = guardWrites(body);
  const limitsSlot = h('section', { class: 'section', 'aria-labelledby': 'u-limits' });
  const usageSlot = h('section', { class: 'section', 'aria-labelledby': 'u-usage' });
  const balancesSlot = h('section', { class: 'section', 'aria-labelledby': 'u-balances' });
  if (filterCid) body.append(filterBanner, usageSlot);
  else body.append(limitsSlot, usageSlot, balancesSlot);

  // ---- usage table
  let group = filterCid ? 'model' : 'day';
  const today = new Date();
  const since = new Date(Date.UTC(today.getUTCFullYear(), today.getUTCMonth(), today.getUTCDate() - 29));
  let range = filterCid ? { since: '', until: '' } : { since: isoDay(since), until: isoDay(today) };

  async function renderUsageTable() {
    const groups = filterCid ? GROUPS.filter(([v]) => v !== 'conversation') : GROUPS;
    const seg = h(
      'div',
      { class: 'seg', role: 'radiogroup', 'aria-label': 'Group by' },
      groups.map(([v, label]) =>
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
              renderUsageTable();
            },
          }),
          label,
        ),
      ),
    );
    const dates = h(
      'form',
      {
        class: 'row-form',
        onchange: (e) => {
          range = { ...range, [e.target.name]: e.target.value };
          renderUsageTable();
        },
        onsubmit: (e) => e.preventDefault(),
      },
      h('label', { class: 'field' }, h('span', null, 'From (UTC)'), h('input', { class: 'input', type: 'date', name: 'since', value: range.since })),
      h('label', { class: 'field' }, h('span', null, 'To (UTC, inclusive)'), h('input', { class: 'input', type: 'date', name: 'until', value: range.until })),
    );
    setChildren(usageSlot, h('div', { class: 'section-head' }, h('h2', { id: 'u-usage' }, 'Usage'), seg), dates, skeleton(4));
    let rows;
    try {
      const query = { since: range.since, until: range.until, group, ...(filterCid ? { conversation: filterCid } : null) };
      rows = (await get(`/v1/usage${qs(query)}`, { signal })) || [];
    } catch (err) {
      if (isAbort(err)) return;
      setChildren(usageSlot, h('div', { class: 'section-head' }, h('h2', { id: 'u-usage' }, 'Usage'), seg), dates, errorBanner(err, renderUsageTable));
      return;
    }
    const label = (key) => {
      if (group === 'conversation') return store.conversations.find((c) => c.id === key)?.title || key || '(none)';
      if (group === 'thread') return store.threads.get(key)?.title || key || '(orchestrator)';
      return key || '—';
    };
    const total = rows.reduce(
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
    const cells = (r) => [
      h('td', { class: 'num' }, num(r.calls)),
      h('td', { class: 'num' }, num(r.input_tokens)),
      h('td', { class: 'num' }, num(r.output_tokens)),
      h('td', { class: 'num' }, num(r.cache_read_tokens)),
      h('td', { class: 'num' }, num(r.cache_write_tokens)),
      h('td', { class: 'num' }, usd(r.cost)),
    ];
    setChildren(
      usageSlot,
      h('div', { class: 'section-head' }, h('h2', { id: 'u-usage' }, 'Usage'), seg),
      dates,
      rows.length
        ? h(
            'div',
            { class: 'table-wrap' },
            h(
              'table',
              { class: 'table' },
              h('thead', null, h('tr', null, [GROUPS.find((g) => g[0] === group)[1], 'Calls', 'Input', 'Output', 'Cache read', 'Cache write', 'Est. cost'].map((t, i) => h('th', { class: i ? 'num' : null, scope: 'col' }, t)))),
              h('tbody', null, rows.map((r) => h('tr', null, h('td', { class: 'wrap' }, label(r.key)), cells(r)))),
              h('tfoot', null, h('tr', null, h('td', null, 'Total'), cells(total))),
            ),
          )
        : h('div', { class: 'empty' }, h('p', null, 'No metered calls in this range. Only calls made through Grouper are counted.')),
    );
  }

  // ---- balances
  async function renderBalances(refresh = false) {
    const refreshBtn = h(
      'button',
      {
        class: 'btn btn-sm',
        type: 'button',
        'data-write': true,
        onclick: async () => {
          refreshBtn.disabled = true;
          try {
            await post('/v1/usage/refresh');
          } catch (err) {
            toastError(err);
          }
          if (!signal.aborted) renderBalances(true);
        },
      },
      icon('refresh'),
      'Refresh',
    );
    if (!refresh) setChildren(balancesSlot, h('div', { class: 'section-head' }, h('h2', { id: 'u-balances' }, 'Balances'), refreshBtn), skeleton(2));
    let snaps;
    try {
      snaps = (await get('/v1/usage/snapshots', { signal })) || [];
    } catch (err) {
      if (isAbort(err)) return;
      setChildren(balancesSlot, h('div', { class: 'section-head' }, h('h2', { id: 'u-balances' }, 'Balances'), refreshBtn), errorBanner(err, () => renderBalances()));
      return;
    }
    const now = Date.now() / 1000;
    setChildren(
      balancesSlot,
      h('div', { class: 'section-head' }, h('h2', { id: 'u-balances' }, 'Balances'), refreshBtn),
      snaps.length
        ? h(
            'div',
            { class: 'table-wrap' },
            h(
              'table',
              { class: 'table' },
              h('thead', null, h('tr', null, ['Provider', 'Kind', 'Window', 'Used', 'Limit', 'Resets', 'Fetched'].map((t) => h('th', { scope: 'col' }, t)))),
              h(
                'tbody',
                null,
                snaps.map((s) =>
                  h(
                    'tr',
                    null,
                    h('td', null, s.provider, s.account && s.account !== s.provider ? h('span', { class: 'hint' }, ` ${s.account}`) : null),
                    h('td', null, s.kind, s.source === 'estimate' ? h('span', { class: 'badge' }, 'estimated') : null),
                    h('td', null, s.window),
                    h('td', null, s.used_pct !== null && s.used_pct !== undefined ? `${Math.round(s.used_pct)}%` : s.used_amount !== null && s.used_amount !== undefined ? `${s.used_amount.toFixed(2)} ${s.currency || ''}` : '—'),
                    h('td', null, s.limit_amount !== null && s.limit_amount !== undefined ? `${s.limit_amount.toFixed(2)} ${s.currency || ''}` : '—'),
                    h('td', null, s.resets_at ? dateTime(s.resets_at) : '—'),
                    h('td', null, ago(s.fetched_at), now - s.fetched_at > 3600 ? h('span', { class: 'badge warn' }, 'stale') : null),
                  ),
                ),
              ),
            ),
          )
        : h('p', { class: 'hint' }, 'No balances yet. OpenRouter and DeepSeek balances are polled hourly when their keys are set.'),
    );
  }

  // ---- limits
  // The form is built once; saves update it in place (a field being typed in is
  // never replaced), and only the stat cards are redrawn.
  let limitsForm = null; // {stats, rows: Map(key -> row), action}

  async function renderLimits() {
    limitsForm = null;
    setChildren(limitsSlot, h('div', { class: 'section-head' }, h('h2', { id: 'u-limits' }, 'Limits')), skeleton(4));
    let report;
    try {
      report = await get('/v1/limits', { signal });
    } catch (err) {
      if (isAbort(err)) return;
      setChildren(limitsSlot, h('div', { class: 'section-head' }, h('h2', { id: 'u-limits' }, 'Limits')), errorBanner(err, renderLimits));
      return;
    }
    drawLimits(report);
  }

  function drawLimits(report) {
    if (limitsForm) {
      limitsForm.stats.replaceWith((limitsForm.stats = statCards(report)));
      for (const spec of LIMITS) limitsForm.rows.get(spec.key).set(report.limits[spec.key]);
      if (document.activeElement !== limitsForm.action) limitsForm.action.value = report.limits.approval_timeout_action;
      return;
    }
    const l = report.limits;
    const stats = statCards(report);
    const rows = new Map(LIMITS.map((spec) => [spec.key, limitRow(spec, l[spec.key])]));
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
    setChildren(
      limitsSlot,
      h('div', { class: 'section-head' }, h('h2', { id: 'u-limits' }, 'Limits')),
      h('p', { class: 'section-note' }, 'All off until you switch them on. Budgets use estimated spend. Changes save when you leave a field.'),
      stats,
      h('ul', { class: 'list card' }, [...rows.values()].map((r) => h('li', null, r.el))),
      h('div', { class: 'card card-pad limit-action' }, action),
    );
    limitsForm = { stats, rows, action: actionSelect };
  }

  function statCards(report) {
    const l = report.limits;
    const daily = l.daily_budget_usd;
    return h(
      'div',
      { class: 'stat-grid' },
      h(
        'div',
        { class: 'card stat' },
        h('div', { class: 'k' }, 'Spent today (estimated)'),
        h('div', { class: 'v' }, usd(report.spent_today_usd)),
        h('div', { class: 's' }, daily ? `of ${usd(daily)} · resets ${dateTime(report.day_resets_at)}` : `No daily budget · day resets ${dateTime(report.day_resets_at)}`),
        daily ? h('meter', { class: 'meter', min: '0', max: String(daily), low: String(daily * report.warn_at_fraction), high: String(daily * 0.999), optimum: '0', value: String(Math.min(report.spent_today_usd, daily)), 'aria-label': 'Spent of daily budget' }) : null,
      ),
      h('div', { class: 'card stat' }, h('div', { class: 'k' }, 'Running threads'), h('div', { class: 'v' }, num(report.running_threads)), h('div', { class: 's' }, l.max_running_threads ? `cap ${l.max_running_threads}` : 'no cap')),
      h('div', { class: 'card stat' }, h('div', { class: 'k' }, 'Warning at'), h('div', { class: 'v' }, `${Math.round(report.warn_at_fraction * 100)}%`), h('div', { class: 's' }, 'of each budget')),
    );
  }

  /**
   * One limit: an "Enabled" switch (off means null: no limit, or the default) and
   * the number with its unit beside it. Returns {el, set(value)}; `set` updates the
   * row from a saved report without touching a field the person is typing in.
   */
  function limitRow(spec, value) {
    const id = `limit-${spec.key}`;
    const scale = spec.scale || 1;
    const shown = (v) => (v === null || v === undefined ? '' : String(Math.round(v * scale * 1000) / 1000));
    const input = h('input', {
      class: 'input',
      id,
      type: 'number',
      min: '0',
      step: spec.step,
      max: spec.max || null,
      inputmode: spec.step === '1' ? 'numeric' : 'decimal',
      'data-write': true,
      'aria-describedby': `${id}-help`,
      onchange: () => {
        if (input.value === '') return;
        const n = Number(input.value);
        if (!Number.isFinite(n) || n < 0 || (spec.max && n > Number(spec.max))) {
          toast(`${spec.label} must be ${spec.max ? `between 0 and ${spec.max}` : 'zero or more'}.`, 'error');
          return;
        }
        const stored = n / scale;
        save({ [spec.key]: spec.step === '1' ? Math.round(stored) : stored });
      },
    });
    const enabled = h('input', {
      type: 'checkbox',
      role: 'switch',
      'data-write': true,
      'aria-label': `${spec.label} enabled`,
      onchange: () => {
        if (enabled.checked) {
          input.disabled = false;
          input.placeholder = '';
          input.focus();
        } else {
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
    const set = (v) => {
      const on = v !== null && v !== undefined;
      enabled.checked = on;
      input.disabled = !on || 'offlineDisabled' in input.dataset;
      input.placeholder = on ? '' : spec.off.replace(/^Off: /, '');
      if (document.activeElement !== input) input.value = shown(v);
    };
    set(value);
    const el = h(
      'div',
      { class: 'limit-row' },
      h('div', null, h('label', { class: 'limit-name', for: id }, spec.label, h('span', { class: 'visually-hidden' }, ` in ${spec.unit}`)), h('div', { class: 'limit-help', id: `${id}-help` }, spec.help, ' ', h('span', { class: 'limit-off' }, `Switched off: ${spec.off.replace(/^Off: /, '').replace(/^Default: /, 'default ')}.`))),
      field,
      h('label', { class: 'switch' }, enabled, 'Enabled'),
    );
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

  const offConvs = filterCid ? on('conversations', renderHeader) : null;
  if (!filterCid) {
    renderLimits();
    renderBalances();
  }
  renderUsageTable();
  return () => {
    aborter.abort();
    unguard();
    offConvs?.();
  };
}
