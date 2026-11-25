// Usage and limits: metered totals, provider balances, and the hub's limits
// (thread caps, budgets, timeouts) with today's estimated spend.

import { h, icon, setChildren } from '../lib/dom.js';
import { get, post, put, qs, describe } from '../lib/api.js';
import { store, isAdmin } from '../lib/store.js';
import { bar, toast, toastError, skeleton, errorBanner } from '../lib/ui.js';
import { num, usd, ago, dateTime, isoDay } from '../lib/format.js';

const GROUPS = [
  ['day', 'Day'],
  ['provider', 'Provider'],
  ['model', 'Model'],
  ['conversation', 'Chat'],
  ['thread', 'Thread'],
];

// Limits as the editor shows them. `off` is what null means for that key.
const LIMITS = [
  { key: 'daily_budget_usd', label: 'Daily budget', unit: 'USD', step: '0.01', help: 'All usage in one UTC day.', off: 'Off' },
  { key: 'conversation_budget_usd', label: 'Budget per conversation', unit: 'USD', step: '0.01', help: 'A conversation’s orchestrator or standard chat and its threads.', off: 'Off' },
  { key: 'thread_budget_usd', label: 'Budget per thread', unit: 'USD', step: '0.01', help: 'Default cap for one thread; the orchestrator may ask for less.', off: 'Off' },
  { key: 'warn_at_fraction', label: 'Warn at', unit: 'fraction', step: '0.05', help: 'Warn when spend reaches this share of a budget (0.8 = 80%).', off: 'Default (0.8)' },
  { key: 'max_running_threads', label: 'Running threads', unit: 'threads', step: '1', help: 'Threads running at once across the account.', off: 'Off' },
  { key: 'max_running_threads_per_device', label: 'Running threads per device', unit: 'threads', step: '1', help: 'Threads running at once on any one device, the hub included.', off: 'Off' },
  { key: 'approval_timeout_secs', label: 'Approval timeout', unit: 'seconds', step: '1', help: 'How long an approval waits before it’s answered for you.', off: 'Waits for you' },
  { key: 'ask_timeout_secs', label: 'Question timeout', unit: 'seconds', step: '1', help: 'How long a thread’s ask_orchestrator waits.', off: 'Waits forever' },
];

export function renderUsage(container) {
  const admin = isAdmin();
  const body = h('div', { class: 'view-body' });
  container.append(h('section', { class: 'view' }, bar({ title: 'Usage & limits', caption: 'Spend is estimated from registry prices' }), body));
  if (!admin) {
    body.append(h('div', { class: 'banner warn' }, icon('shield'), 'Usage and limits need an admin token. Sign in with one in Settings.'));
    return null;
  }
  const limitsSlot = h('section', { class: 'section', 'aria-labelledby': 'u-limits' });
  const usageSlot = h('section', { class: 'section', 'aria-labelledby': 'u-usage' });
  const balancesSlot = h('section', { class: 'section', 'aria-labelledby': 'u-balances' });
  body.append(limitsSlot, usageSlot, balancesSlot);

  // ---- usage table
  let group = 'day';
  const today = new Date();
  const since = new Date(Date.UTC(today.getUTCFullYear(), today.getUTCMonth(), today.getUTCDate() - 29));
  let range = { since: isoDay(since), until: isoDay(today) };

  async function renderUsageTable() {
    const seg = h(
      'div',
      { class: 'seg', role: 'radiogroup', 'aria-label': 'Group by' },
      GROUPS.map(([v, label]) =>
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
      rows = (await get(`/v1/usage${qs({ since: range.since, until: range.until, group })}`)) || [];
    } catch (err) {
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
        onclick: async () => {
          refreshBtn.disabled = true;
          try {
            await post('/v1/usage/refresh');
          } catch (err) {
            toastError(err);
          }
          renderBalances(true);
        },
      },
      icon('refresh'),
      'Refresh',
    );
    if (!refresh) setChildren(balancesSlot, h('div', { class: 'section-head' }, h('h2', { id: 'u-balances' }, 'Balances'), refreshBtn), skeleton(2));
    let snaps;
    try {
      snaps = (await get('/v1/usage/snapshots')) || [];
    } catch (err) {
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
  async function renderLimits() {
    setChildren(limitsSlot, h('div', { class: 'section-head' }, h('h2', { id: 'u-limits' }, 'Limits')), skeleton(4));
    let report;
    try {
      report = await get('/v1/limits');
    } catch (err) {
      setChildren(limitsSlot, h('div', { class: 'section-head' }, h('h2', { id: 'u-limits' }, 'Limits')), errorBanner(err, renderLimits));
      return;
    }
    drawLimits(report);
  }

  function drawLimits(report) {
    const l = report.limits;
    const daily = l.daily_budget_usd;
    const stats = h(
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

    const rows = LIMITS.map((spec) => limitRow(spec, l[spec.key]));
    const action = h(
      'label',
      { class: 'field' },
      h('span', null, 'When an approval times out'),
      h(
        'select',
        {
          class: 'select',
          name: 'approval_timeout_action',
          onchange: (e) => save({ approval_timeout_action: e.target.value }),
        },
        h('option', { value: 'deny', selected: l.approval_timeout_action === 'deny' }, 'Deny the call; the thread goes on'),
        h('option', { value: 'stop', selected: l.approval_timeout_action === 'stop' }, 'Deny and stop the thread'),
      ),
    );
    setChildren(
      limitsSlot,
      h('div', { class: 'section-head' }, h('h2', { id: 'u-limits' }, 'Limits')),
      h('p', { class: 'section-note' }, 'All off until you set them. Budgets use estimated spend.'),
      stats,
      h('ul', { class: 'list card' }, rows.map((r) => h('li', null, r))),
      h('div', { class: 'card card-pad section' }, action),
    );
  }

  function limitRow(spec, value) {
    const isOff = value === null || value === undefined;
    const input = h('input', {
      class: 'input limit-input-field',
      type: 'number',
      min: '0',
      step: spec.step,
      max: spec.key === 'warn_at_fraction' ? '1' : null,
      inputmode: 'decimal',
      value: isOff ? '' : String(value),
      disabled: isOff,
      'aria-label': `${spec.label} in ${spec.unit}`,
      onchange: () => {
        if (input.value === '') return;
        const n = Number(input.value);
        if (!Number.isFinite(n) || n < 0) {
          toast(`${spec.label} must be a positive number.`, 'error');
          return;
        }
        save({ [spec.key]: spec.step === '1' ? Math.round(n) : n });
      },
    });
    const off = h('input', {
      type: 'checkbox',
      checked: isOff,
      onchange: (e) => {
        if (e.target.checked) {
          save({ [spec.key]: null });
        } else {
          input.disabled = false;
          input.focus();
        }
      },
    });
    return h(
      'div',
      { class: 'limit-row' },
      h('div', null, h('div', { class: 'limit-name' }, spec.label), h('div', { class: 'limit-help' }, spec.help)),
      h('div', { class: 'limit-input' }, input),
      h('label', { class: 'check', title: spec.off }, off, spec.off),
    );
  }

  async function save(changes) {
    try {
      const report = await put('/v1/limits', changes);
      toast('Limits saved.');
      drawLimits(report);
    } catch (err) {
      toast(describe(err), 'error');
      renderLimits();
    }
  }

  renderLimits();
  renderUsageTable();
  renderBalances();
  return null;
}
