// Limits and budgets: the editor for the hub's caps (#/usage/limits). Moved out of the old
// single-page Usage view without changing behaviour. The daily budget also shows as one
// line on the Usage page (views/usage.js); this page owns editing it.

import { h, icon, setChildren, setText, throttle } from '../lib/dom.js';
import { put, describe, isAbort } from '../lib/api.js';
import { store, on, isAdmin } from '../lib/store.js';
import { bar, toast, skeleton, guardWrites, isOffline } from '../lib/ui.js';
import { num, usd } from '../lib/format.js';
import { load, nowSecs, est, meter, inDuration, sourceProblem } from './usage-common.js';

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

export function renderUsageLimits(container) {
  const admin = isAdmin();
  const barSlot = h('div', { class: 'bar-slot' });
  const body = h('div', { class: 'view-body u-body' });
  container.append(h('section', { class: 'view' }, barSlot, body));
  setChildren(barSlot, bar({ title: 'Limits and budgets', caption: 'Spend caps, thread caps and timeouts', back: '#/usage' }));
  document.title = 'Limits and budgets · Grouper';

  if (!admin) {
    body.append(h('div', { class: 'banner warn' }, icon('shield'), 'Usage and limits need an admin token. Sign in with one in Settings.'));
    return null;
  }

  // Leaving the view cancels its reads; a write in flight still finishes.
  const aborter = new AbortController();
  const { signal } = aborter;
  const unguard = guardWrites(body);

  const limitsSlot = h('div', { 'aria-busy': 'true' }, skeleton(4));
  body.append(
    h(
      'section',
      { class: 'section', 'aria-labelledby': 'u-limits' },
      h('div', { class: 'section-head' }, h('h2', { id: 'u-limits' }, 'Limits & budgets')),
      h('p', { class: 'section-note' }, 'All off until you switch them on. Budgets use Grouper’s estimated spend, not provider bills. Changes save when you leave a field.'),
      limitsSlot,
    ),
  );

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
    refreshLimits();
  }, 4000);
  const offEvent = on('event', (e) => {
    if (!e) return;
    const usageEvent = e.kind === 'thread_activity' && e.event?.type === 'usage';
    if (usageEvent || e.kind === 'turn_finished' || e.kind === 'thread_finished' || e.kind === 'budget_warning') metered();
  });
  const offOnline = on('online', (online) => {
    if (online && !signal.aborted) refreshLimits();
  });
  // Countdowns move on their own.
  const minute = setInterval(() => {
    if (document.hidden || signal.aborted) return;
    if (lastReport && limitsForm) limitsForm.stats.replaceWith((limitsForm.stats = statCards(lastReport)));
  }, 60000);

  renderLimits();
  return () => {
    aborter.abort();
    unguard();
    metered.cancel();
    clearInterval(minute);
    offEvent();
    offOnline();
  };
}
