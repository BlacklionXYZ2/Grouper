// The metered breakdown: by provider, model, chat, thread or day over a range, as a table
// (cards on a phone). `#/usage/breakdown` is the page; `?provider=<id>` filters it. The same
// section serves the threads table of a chat's usage (`#/usage?conversation=<id>`).
// Moved out of the old single-page Usage view without changing behaviour.

import { h, icon, setChildren, setText, keepFocus, throttle } from '../lib/dom.js';
import { qs, enc, isAbort } from '../lib/api.js';
import { store, on, isAdmin } from '../lib/store.js';
import { bar, toast, skeleton, guardWrites, isOffline, navigate } from '../lib/ui.js';
import { num, usd, isoDay } from '../lib/format.js';
import { familyKey, familyMark, familyLabel } from '../lib/family.js';
import { load, est, sumRows, tokensOf, sourceProblem } from './usage-common.js';

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

/**
 * The breakdown section and its controls. `filterCid` scopes it to one chat;
 * `provider` is the provider filter (it lives in the URL: `navigate` changes it and the
 * page's route handler calls `applyProviderFilter`). Returns the elements to place and
 * the hooks to call.
 */
export function createBreakdown({ filterCid = null, provider: routeProvider = null, signal, base = '#/usage/breakdown', group: initialGroup = 'model' }) {
  let rangeKey = filterCid ? 'all' : '30';
  const todayIso = isoDay(new Date());
  let custom = { since: isoDay(new Date(Date.now() - 29 * 86400000)), until: todayIso };
  let group = initialGroup;
  // A provider filter: the breakdown shows only that provider's calls.
  let providerFilter = routeProvider || null;
  const st = { rows: null };

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

  // ---- layout (built once; the table redraws its own body in place)
  const rangeSelect = h(
    'select',
    {
      class: 'select',
      id: 'u-range',
      onchange: (e) => {
        rangeKey = e.target.value;
        customFields.hidden = rangeKey !== 'custom';
        loadRows();
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
          loadRows();
        },
      }),
    );
  const customFields = h('div', { class: 'u-custom', hidden: rangeKey !== 'custom' }, dateInput('since', 'From (UTC)'), dateInput('until', 'To (UTC, inclusive)'));
  const toolbar = h(
    'div',
    { class: 'u-toolbar' },
    h('label', { class: 'field u-range' }, h('span', null, filterCid ? 'Range (UTC)' : 'Range (UTC)'), rangeSelect),
    customFields,
  );

  const breakdownBody = h('div', { 'aria-busy': 'true' }, skeleton(4));
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
    navigate(`${base}${qs({ conversation: filterCid, provider: next })}`);
  }

  /** Show the filter the URL names, in place: the chip, the cards' state, the rows. */
  function applyProviderFilter(id) {
    const next = id || null;
    if (next === providerFilter) return;
    const chipHadFocus = filterSlot.contains(document.activeElement);
    providerFilter = next;
    drawFilterChip();
    setText(filterStatus, next ? `Breakdown filtered to ${next}.` : `Filter removed: the breakdown shows all providers.`);
    if (chipHadFocus) {
      // The chip that had focus is gone: move to the heading.
      breakdownHeading.focus();
    }
    loadRows();
  }

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
  drawFilterChip();

  // ---- loading

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
                  loadRows();
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

  // ---- live updates: in place, never moving focus or scroll

  const metered = throttle(() => {
    if (signal.aborted || isOffline()) return;
    loadRows();
  }, 4000);
  const relabel = throttle(() => {
    if (st.rows && (group === 'conversation' || group === 'thread')) drawBreakdown();
  }, 1000);
  const offs = [
    on('event', (e) => {
      if (!e || (filterCid && e.conversation_id !== filterCid)) return;
      const usageEvent = e.kind === 'thread_activity' && e.event?.type === 'usage';
      if (usageEvent || e.kind === 'turn_finished' || e.kind === 'thread_finished' || e.kind === 'budget_warning') metered();
    }),
    on('conversations', relabel),
    on('threads', relabel),
    on('models', () => {
      if (st.rows?.group === 'model') drawBreakdown();
    }),
    on('online', (online) => {
      if (online && !signal.aborted) loadRows(); // back online: replace any cached copy
    }),
  ];

  loadRows();
  return {
    toolbar,
    chatStats,
    section: breakdown,
    applyProviderFilter,
    destroy() {
      metered.cancel();
      relabel.cancel();
      offs.forEach((off) => off());
    },
  };
}

/** `#/usage/breakdown`: the breakdown on its own page. */
export function renderUsageBreakdown(container, { provider = null } = {}) {
  const admin = isAdmin();
  const barSlot = h('div', { class: 'bar-slot' });
  const body = h('div', { class: 'view-body u-body' });
  container.append(h('section', { class: 'view' }, barSlot, body));
  setChildren(barSlot, bar({ title: 'Breakdown', caption: 'Estimated API cost by provider, model, chat, thread or day', back: '#/usage' }));
  document.title = 'Breakdown · Usage · Grouper';
  if (!admin) {
    body.append(h('div', { class: 'banner warn' }, icon('shield'), 'Usage and limits need an admin token. Sign in with one in Settings.'));
    return null;
  }
  const aborter = new AbortController();
  const { signal } = aborter;
  const unguard = guardWrites(body);
  const bd = createBreakdown({ provider, signal });
  body.append(bd.toolbar, bd.section);
  // The route key ignores `provider=`, so this view stays and only the filter moves.
  const offRoute = on('route', (r) => {
    if (r.view === 'usage' && r.sub === 'breakdown') bd.applyProviderFilter(r.provider);
  });
  return () => {
    aborter.abort();
    unguard();
    bd.destroy();
    offRoute();
  };
}
