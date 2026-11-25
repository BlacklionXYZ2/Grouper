// Model registry: models (edit, refresh catalog prices, run probes, delete),
// what threads and probes taught it (stats), proposals waiting for you,
// priority rules, and importing a model from a provider's catalog.

import { h, icon, setChildren } from '../lib/dom.js';
import { get, post, put, del, qs, enc, describe, isAbort } from '../lib/api.js';
import { store, isAdmin } from '../lib/store.js';
import { loadModels } from '../lib/live.js';
import { bar, sheet, confirmAction, toast, toastError, skeleton, errorBanner, guardWrites } from '../lib/ui.js';
import { num, usd, pct, duration, ago } from '../lib/format.js';

const CONSTRAINTS = ['local', 'remote', 'free', 'paid'];
const PREFERS = ['cheapest', 'local', 'free'];
const ROLES = ['orchestrator', 'standard', 'thread'];

function isOpenRouterProvider(provider) {
  if (!provider?.base_url) return false;
  try {
    const host = new URL(provider.base_url).hostname.toLowerCase();
    return host === 'openrouter.ai' || host.endsWith('.openrouter.ai');
  } catch {
    return false;
  }
}

export function renderModels(container) {
  const admin = isAdmin();
  const body = h('div', { class: 'view-body' });
  container.append(h('section', { class: 'view' }, bar({ title: 'Models', caption: 'Registry, stats, proposals and rules' }), body));

  const slots = {
    proposals: h('section', { class: 'section', 'aria-labelledby': 'm-proposals' }),
    models: h('section', { class: 'section', 'aria-labelledby': 'm-models' }),
    stats: h('section', { class: 'section', 'aria-labelledby': 'm-stats' }),
    rules: h('section', { class: 'section', 'aria-labelledby': 'm-rules' }),
    import: h('section', { class: 'section', 'aria-labelledby': 'm-import' }),
  };
  body.append(slots.proposals, slots.models, slots.stats, slots.rules, slots.import);

  const head = (id, title, extra) => h('div', { class: 'section-head' }, h('h2', { id }, title), extra || null);
  // Leaving the view cancels its reads; writes are paused while offline.
  const aborter = new AbortController();
  const { signal } = aborter;
  const unguard = guardWrites(body);
  let providersById = new Map();

  async function ensureProviders() {
    if (!admin) return [];
    try {
      const list = (await get('/v1/providers', { signal })) || [];
      providersById = new Map(list.map((p) => [p.id, p]));
      return list;
    } catch {
      return [...providersById.values()];
    }
  }

  // ---- models
  async function renderModelList() {
    setChildren(slots.models, head('m-models', 'Models'), skeleton(3));
    try {
      await Promise.all([loadModels(), ensureProviders()]);
      if (signal.aborted) return;
    } catch (err) {
      if (isAbort(err)) return;
      setChildren(slots.models, head('m-models', 'Models'), errorBanner(err, renderModelList));
      return;
    }
    const models = store.models;
    if (!models.length) {
      setChildren(
        slots.models,
        head('m-models', 'Models'),
        h('div', { class: 'card card-pad' }, h('p', null, 'No models yet. Add a provider key in ', h('a', { href: '#/settings' }, 'Settings'), ', then import a model below.')),
      );
      return;
    }
    // A wide table hides Edit / Refresh prices / Run probes / Delete past the
    // first columns. On a narrow window those actions are buttons on the card.
    if (matchMedia('(max-width: 760px)').matches) {
      setChildren(
        slots.models,
        head('m-models', 'Models'),
        h(
          'ul',
          { class: 'list card' },
          models.map((m) => {
            const isCatalog = isOpenRouterProvider(providersById.get(m.provider_id));
            return h(
              'li',
              { class: 'list-row' },
              h(
                'div',
                { class: 'grow' },
                h('div', { class: 'row-title' }, m.id),
                h('div', { class: 'row-sub mono' }, `${m.provider_id} / ${m.model}`),
                h(
                  'div',
                  { class: 'meta' },
                  h('span', null, m.roles.length ? m.roles.join(', ') : 'No roles'),
                  h('span', null, `$${m.input_price.toFixed(2)} in / $${m.output_price.toFixed(2)} out`),
                  m.free ? h('span', { class: 'badge accent' }, 'Free') : null,
                  m.local ? h('span', { class: 'badge' }, 'Local') : null,
                ),
              ),
              admin
                ? h(
                    'div',
                    { class: 'bar-actions' },
                    h('button', { class: 'btn btn-sm', type: 'button', 'data-write': true, 'aria-label': `Edit ${m.id}`, onclick: () => openEditModelSheet(m) }, icon('edit'), 'Edit'),
                    isCatalog
                      ? h('button', { class: 'btn btn-sm', type: 'button', 'data-write': true, 'aria-label': `Refresh prices for ${m.id}`, onclick: (e) => refreshModelPrices(m, e.currentTarget) }, icon('refresh'), 'Refresh prices')
                      : null,
                    h('button', { class: 'btn btn-sm', type: 'button', 'data-write': true, 'aria-label': `Run probes for ${m.id}`, onclick: () => openProbeSheet(m) }, icon('play'), 'Run probes'),
                    h(
                      'button',
                      {
                        class: 'btn btn-sm',
                        type: 'button',
                        'data-write': true,
                        'aria-label': `Delete ${m.id}`,
                        onclick: async () => {
                          if (!(await confirmAction({ title: `Delete ${m.id}?`, body: 'It leaves the registry. Conversations pinned to it fall back to the default.', confirmLabel: 'Delete', danger: true }))) return;
                          try {
                            await del(`/v1/models/${enc(m.id)}`);
                            toast(`Deleted ${m.id}.`);
                            renderModelList();
                          } catch (err) {
                            toastError(err);
                          }
                        },
                      },
                      icon('trash'),
                      'Delete',
                    ),
                  )
                : null,
            );
          }),
        ),
        h('p', { class: 'hint' }, 'Prices are USD per million tokens. Edit a model to change roles, prices (including cache prices), context window, use cases or local flag.'),
      );
      return;
    }
    setChildren(
      slots.models,
      head('m-models', 'Models'),
      h(
        'div',
        { class: 'table-wrap', tabindex: '0', role: 'region', 'aria-label': 'Models table' },
        h(
          'table',
          { class: 'table' },
          h(
            'thead',
            null,
            h(
              'tr',
              null,
              ['Id', 'Provider model', 'Roles', 'Flags', 'Context', 'In $/M', 'Out $/M', admin ? 'Actions' : ''].map((t, i) =>
                h('th', { class: i >= 4 && i <= 6 ? 'num' : null, scope: 'col' }, t),
              ),
            ),
          ),
          h(
            'tbody',
            null,
            models.map((m) => {
              const isCatalog = isOpenRouterProvider(providersById.get(m.provider_id));
              const cacheNote =
                m.cache_read_price !== null && m.cache_read_price !== undefined
                  ? `cache ${m.cache_read_price.toFixed(2)}${m.cache_write_price !== null && m.cache_write_price !== undefined ? ` / ${m.cache_write_price.toFixed(2)}` : ''}`
                  : m.cache_write_price !== null && m.cache_write_price !== undefined
                    ? `cache write ${m.cache_write_price.toFixed(2)}`
                    : null;
              return h(
                'tr',
                null,
                h(
                  'td',
                  { class: 'wrap' },
                  h('strong', null, m.id),
                  m.use_cases ? h('div', { class: 'row-sub' }, m.use_cases) : null,
                ),
                h(
                  'td',
                  { class: 'mono' },
                  `${m.provider_id} / ${m.model}`,
                  cacheNote ? h('div', { class: 'hint' }, cacheNote) : null,
                ),
                h('td', null, m.roles.length ? m.roles.join(', ') : '—'),
                h('td', null, m.free ? h('span', { class: 'badge accent' }, 'Free') : null, ' ', m.local ? h('span', { class: 'badge' }, 'Local') : null),
                h('td', { class: 'num' }, m.context_window ? num(m.context_window) : '—'),
                h('td', { class: 'num' }, m.input_price.toFixed(2)),
                h('td', { class: 'num' }, m.output_price.toFixed(2)),
                h(
                  'td',
                  null,
                  admin
                    ? h(
                        'div',
                        { class: 'table-actions' },
                        h(
                          'button',
                          {
                            class: 'icon-btn',
                            'data-write': true,
                            type: 'button',
                            'aria-label': `Edit ${m.id}`,
                            title: 'Edit model',
                            onclick: () => openEditModelSheet(m),
                          },
                          icon('edit'),
                        ),
                        isCatalog
                          ? h(
                              'button',
                              {
                                class: 'icon-btn',
                                'data-write': true,
                                type: 'button',
                                'aria-label': `Refresh prices for ${m.id}`,
                                title: 'Refresh prices from catalog',
                                onclick: (e) => refreshModelPrices(m, e.currentTarget),
                              },
                              icon('refresh'),
                            )
                          : null,
                        h(
                          'button',
                          {
                            class: 'icon-btn',
                            'data-write': true,
                            type: 'button',
                            'aria-label': `Run probes for ${m.id}`,
                            title: 'Run probes',
                            onclick: () => openProbeSheet(m),
                          },
                          icon('play'),
                        ),
                        h(
                          'button',
                          {
                            class: 'icon-btn',
                            'data-write': true,
                            type: 'button',
                            'aria-label': `Delete ${m.id}`,
                            title: 'Delete',
                            onclick: async () => {
                              if (!(await confirmAction({ title: `Delete ${m.id}?`, body: 'It leaves the registry. Conversations pinned to it fall back to the default.', confirmLabel: 'Delete', danger: true }))) return;
                              try {
                                await del(`/v1/models/${enc(m.id)}`);
                                toast(`Deleted ${m.id}.`);
                                renderModelList();
                              } catch (err) {
                                toastError(err);
                              }
                            },
                          },
                          icon('trash'),
                        ),
                      )
                    : null,
                ),
              );
            }),
          ),
        ),
      ),
      h('p', { class: 'hint' }, 'Prices are USD per million tokens. Edit a model to change roles, prices (including cache prices), context window, use cases or local flag.'),
    );
  }

  async function refreshModelPrices(m, btn) {
    if (btn) btn.disabled = true;
    try {
      const updated = await post(`/v1/models/${enc(m.id)}/refresh-prices`);
      toast(`Refreshed ${m.id}: $${updated.input_price.toFixed(2)} in / $${updated.output_price.toFixed(2)} out per 1M.`);
      renderModelList();
    } catch (err) {
      toastError(err);
      if (btn) btn.disabled = false;
    }
  }

  function openEditModelSheet(m) {
    const extraRoles = (m.roles || []).filter((r) => !ROLES.includes(r));
    sheet((close) => {
      const errBox = h('div', { class: 'banner error', role: 'alert', hidden: true });
      const roleBoxes = ROLES.map((r) => ({
        role: r,
        input: h('input', { type: 'checkbox', name: `role-${r}`, checked: (m.roles || []).includes(r) }),
      }));
      const inputPrice = h('input', { class: 'input mono', type: 'number', min: '0', step: 'any', required: true, value: String(m.input_price ?? 0) });
      const outputPrice = h('input', { class: 'input mono', type: 'number', min: '0', step: 'any', required: true, value: String(m.output_price ?? 0) });
      const cacheRead = h('input', {
        class: 'input mono',
        type: 'number',
        min: '0',
        step: 'any',
        placeholder: 'Provider default',
        value: m.cache_read_price !== null && m.cache_read_price !== undefined ? String(m.cache_read_price) : '',
      });
      const cacheWrite = h('input', {
        class: 'input mono',
        type: 'number',
        min: '0',
        step: 'any',
        placeholder: 'Provider default',
        value: m.cache_write_price !== null && m.cache_write_price !== undefined ? String(m.cache_write_price) : '',
      });
      const contextWindow = h('input', { class: 'input mono', type: 'number', min: '0', step: '1', inputmode: 'numeric', value: String(m.context_window ?? 0) });
      const useCases = h('textarea', { class: 'textarea', rows: 3, placeholder: 'What this model is good for…', value: m.use_cases || '' });
      const localCheck = h('input', { type: 'checkbox', checked: Boolean(m.local) });
      const freeCheck = h('input', { type: 'checkbox', checked: Boolean(m.free) });

      function parsePrice(el, label, optional) {
        const raw = el.value.trim();
        if (!raw) {
          if (optional) return null;
          throw new Error(`${label} is required.`);
        }
        const n = Number(raw);
        if (!Number.isFinite(n) || n < 0) throw new Error(`${label} must be zero or more.`);
        return n;
      }

      const saveBtn = h('button', { class: 'btn btn-primary', type: 'submit', form: 'edit-model-form' }, 'Save changes');
      const form = h(
        'form',
        {
          id: 'edit-model-form',
          class: 'stack',
          onsubmit: async (e) => {
            e.preventDefault();
            errBox.hidden = true;
            let body;
            try {
              const inP = parsePrice(inputPrice, 'Input price', false);
              const outP = parsePrice(outputPrice, 'Output price', false);
              const crP = parsePrice(cacheRead, 'Cache read price', true);
              const cwP = parsePrice(cacheWrite, 'Cache write price', true);
              const ctxRaw = contextWindow.value.trim();
              const ctx = ctxRaw ? Number(ctxRaw) : 0;
              if (!Number.isFinite(ctx) || ctx < 0 || !Number.isInteger(ctx)) {
                throw new Error('Context window must be a whole number (zero or more).');
              }
              const roles = [...roleBoxes.filter((b) => b.input.checked).map((b) => b.role), ...extraRoles];
              body = {
                id: m.id,
                provider_id: m.provider_id,
                model: m.model,
                input_price: inP,
                output_price: outP,
                cache_read_price: crP,
                cache_write_price: cwP,
                context_window: ctx,
                use_cases: useCases.value.trim(),
                free: freeCheck.checked,
                roles,
                local: localCheck.checked,
                modalities: m.modalities || [],
              };
            } catch (valErr) {
              errBox.textContent = valErr.message;
              errBox.hidden = false;
              return;
            }
            saveBtn.disabled = true;
            try {
              await put(`/v1/models/${enc(m.id)}`, body);
              toast(`Saved ${m.id}.`);
              close(true);
              renderModelList();
              renderProposals();
            } catch (err) {
              errBox.textContent = describe(err);
              errBox.hidden = false;
              saveBtn.disabled = false;
            }
          },
        },
        errBox,
        h(
          'fieldset',
          { class: 'row-form plain' },
          h('legend', { class: 'field-label' }, 'Default roles'),
          roleBoxes.map((b) => h('label', { class: 'check' }, b.input, b.role)),
        ),
        h(
          'div',
          { class: 'form-grid' },
          h('label', { class: 'field' }, h('span', null, 'Input price ($/1M)'), inputPrice),
          h('label', { class: 'field' }, h('span', null, 'Output price ($/1M)'), outputPrice),
          h('label', { class: 'field' }, h('span', null, 'Cache read ($/1M, optional)'), cacheRead),
          h('label', { class: 'field' }, h('span', null, 'Cache write ($/1M, optional)'), cacheWrite),
        ),
        h('label', { class: 'field' }, h('span', null, 'Context window (tokens)'), contextWindow),
        h('label', { class: 'field' }, h('span', null, 'Use cases'), useCases),
        h(
          'fieldset',
          { class: 'row-form plain' },
          h('legend', { class: 'field-label' }, 'Flags'),
          h('label', { class: 'check' }, localCheck, 'Local (runs on your own hardware)'),
          h('label', { class: 'check' }, freeCheck, 'Free'),
        ),
      );

      return {
        title: `Edit ${m.id}`,
        caption: `${m.provider_id} / ${m.model}`,
        body: [form],
        foot: [h('button', { class: 'btn', type: 'button', onclick: () => close(false) }, 'Cancel'), saveBtn],
        onOpen: () => roleBoxes[0].input.focus(),
      };
    });
  }

  function openProbeSheet(m) {
    sheet((close) => {
      const content = h(
        'div',
        { class: 'stack' },
        h('p', { class: 'hint' }, 'Running the built-in probe suite (code, writing, research, data, tool use). Probes count towards usage and respect the daily budget.'),
        skeleton(4),
      );
      const rerunBtn = h('button', { class: 'btn', type: 'button', hidden: true, onclick: () => run() }, icon('play'), 'Run again');
      const closeBtn = h('button', { class: 'btn btn-primary', type: 'button', onclick: () => close() }, 'Close');

      async function run() {
        rerunBtn.hidden = true;
        setChildren(
          content,
          h('p', { class: 'hint', role: 'status' }, `Running probes on ${m.id}… this may take a few seconds.`),
          skeleton(4),
        );
        try {
          const report = await post(`/v1/models/${enc(m.id)}/probe`);
          renderReport(report);
          // Refresh proposals, stats and models in the background.
          stats = null;
          renderProposals();
          renderStats();
          renderModelList();
        } catch (err) {
          setChildren(content, errorBanner(err, run));
        } finally {
          rerunBtn.hidden = false;
        }
      }

      function renderReport(report) {
        const proposalSlot = h('div');
        if (report.proposal_id && report.proposal_text) {
          const decideHere = async (accept) => {
            try {
              await post(`/v1/registry/proposals/${report.proposal_id}`, { accept });
              toast(accept ? 'Proposal accepted.' : 'Proposal rejected.');
              setChildren(proposalSlot, h('div', { class: 'banner info' }, icon('check'), h('span', { class: 'grow' }, accept ? 'Use-case proposal accepted.' : 'Use-case proposal rejected.')));
              renderProposals();
              renderModelList();
            } catch (err) {
              toastError(err);
            }
          };
          setChildren(
            proposalSlot,
            h(
              'div',
              { class: 'callout stack' },
              h('div', { class: 'field-label' }, 'Proposed use cases'),
              h('div', null, report.proposal_text),
              h(
                'div',
                { class: 'bar-actions' },
                h('button', { class: 'btn btn-sm', type: 'button', onclick: () => decideHere(false) }, 'Reject'),
                h('button', { class: 'btn btn-sm btn-primary', type: 'button', onclick: () => decideHere(true) }, 'Accept proposal'),
              ),
            ),
          );
        }
        setChildren(
          content,
          h(
            'div',
            { class: 'probe-summary' },
            h('span', { class: ['badge', report.passed === report.total ? 'accent' : 'warn'] }, `${report.passed}/${report.total} passed`),
            h('span', { class: 'hint' }, `${(report.total_latency_ms / 1000).toFixed(1)}s total`),
            h('span', { class: 'hint' }, `${num(report.total_tokens)} tokens`),
            h('span', { class: 'hint' }, `${usd(report.total_cost)} est.`),
          ),
          report.stopped_reason ? h('div', { class: 'banner warn', role: 'status' }, icon('warning'), h('span', { class: 'grow' }, report.stopped_reason)) : null,
          h(
            'ul',
            { class: 'list card' },
            (report.results || []).map((r) =>
              h(
                'li',
                { class: 'list-row' },
                h(
                  'div',
                  { class: 'grow' },
                  h(
                    'div',
                    { class: 'row-title row-badges' },
                    r.id,
                    h('span', { class: ['badge', r.passed ? 'accent' : 'danger'] }, r.passed ? 'Pass' : 'Fail'),
                  ),
                  h('div', { class: 'row-sub' }, r.detail),
                  h(
                    'div',
                    { class: 'meta' },
                    h('span', null, `${r.latency_ms < 1000 ? `${r.latency_ms}ms` : `${(r.latency_ms / 1000).toFixed(1)}s`}`),
                    h('span', null, `${num(r.input_tokens)} in / ${num(r.output_tokens)} out`),
                    h('span', null, usd(r.cost)),
                  ),
                ),
              ),
            ),
          ),
          proposalSlot,
        );
      }

      run();
      return {
        title: `Probes: ${m.id}`,
        caption: `${m.provider_id} / ${m.model}`,
        body: [content],
        foot: [rerunBtn, closeBtn],
      };
    });
  }

  // ---- proposals
  async function renderProposals() {
    if (!admin) {
      slots.proposals.hidden = true;
      return;
    }
    let list;
    try {
      list = (await get(`/v1/registry/proposals${qs({ status: 'pending' })}`, { signal })) || [];
    } catch (err) {
      if (isAbort(err)) return;
      setChildren(slots.proposals, head('m-proposals', 'Proposals'), errorBanner(err, renderProposals));
      return;
    }
    store.pendingProposals = list.length;
    slots.proposals.hidden = !list.length;
    setChildren(
      slots.proposals,
      head('m-proposals', `Proposals (${list.length})`),
      h('p', { class: 'section-note' }, 'Changes the registry suggests from threads, catalogs or probes. Nothing changes until you accept.'),
      h(
        'ul',
        { class: 'list card' },
        list.map((p) => {
          const model = store.models.find((m) => m.id === p.model_id);
          return h(
            'li',
            { class: 'list-row' },
            h(
              'div',
              { class: 'grow' },
              h('div', { class: 'row-title' }, `${p.model_id}: ${p.field.replace('_', ' ')}`),
              h('div', { class: 'row-sub' }, h('strong', null, 'Now: '), model?.[p.field] || '—'),
              h('div', { class: 'row-sub' }, h('strong', null, 'Proposed: '), p.text),
              h('div', { class: 'meta' }, h('span', null, p.reason), h('span', null, `from ${p.source}`), h('span', null, ago(p.created))),
            ),
            h(
              'div',
              { class: 'bar-actions' },
              h('button', { class: 'btn btn-sm', type: 'button', 'data-write': true, onclick: () => decide(p, false) }, 'Reject'),
              h('button', { class: 'btn btn-sm btn-primary', type: 'button', 'data-write': true, onclick: () => decide(p, true) }, 'Accept'),
            ),
          );
        }),
      ),
    );
  }

  async function decide(p, accept) {
    try {
      await post(`/v1/registry/proposals/${p.id}`, { accept });
      toast(accept ? 'Accepted.' : 'Rejected.');
    } catch (err) {
      toast(err.status === 409 ? 'Already decided.' : describe(err), 'error');
    }
    renderProposals();
    renderModelList();
  }

  // ---- stats
  let statsTab = 'models';
  let stats = null;
  async function renderStats() {
    if (!stats) {
      setChildren(slots.stats, head('m-stats', 'Stats'), skeleton(3));
      try {
        stats = await get('/v1/registry/stats', { signal });
      } catch (err) {
        if (isAbort(err)) return;
        setChildren(slots.stats, head('m-stats', 'Stats'), errorBanner(err, renderStats));
        return;
      }
    }
    const rows = (stats[statsTab] || []).slice().sort((a, b) => a.subject.localeCompare(b.subject) || a.task_type.localeCompare(b.task_type));
    const tabLabels = [
      ['models', 'Models'],
      ['devices', 'Devices'],
      ['probes', 'Probes'],
    ];
    const seg = h(
      'div',
      { class: 'seg', role: 'radiogroup', 'aria-label': 'Stats for' },
      tabLabels.map(([v, label]) =>
        h(
          'label',
          null,
          h('input', {
            type: 'radio',
            name: 'stats-tab',
            value: v,
            checked: statsTab === v,
            onchange: () => {
              statsTab = v;
              renderStats();
            },
          }),
          label,
        ),
      ),
    );
    const none = statsTab === 'probes' ? 'No probe runs' : 'No finished threads';
    setChildren(
      slots.stats,
      head('m-stats', 'Stats', seg),
      rows.length
        ? h(
            'div',
            { class: 'table-wrap', tabindex: '0', role: 'region', 'aria-label': 'Stats table' },
            h(
              'table',
              { class: 'table' },
              h('thead', null, h('tr', null, [statsTab === 'devices' ? 'Device' : 'Model', 'Task type', 'Done', 'Failed', 'Stopped', 'Success', 'Median', 'Mean cost', 'Grade', 'Rating'].map((t, i) => h('th', { class: i >= 2 ? 'num' : null, scope: 'col' }, t)))),
              h(
                'tbody',
                null,
                rows.map((r) =>
                  h(
                    'tr',
                    null,
                    h('td', null, r.subject),
                    h('td', null, r.task_type || 'all'),
                    h('td', { class: 'num' }, num(r.done)),
                    h('td', { class: 'num' }, num(r.failed)),
                    h('td', { class: 'num' }, num(r.stopped)),
                    h('td', { class: 'num' }, r.success_rate === null ? none : pct(r.success_rate)),
                    h('td', { class: 'num' }, r.median_duration_s === null ? '—' : duration(r.median_duration_s)),
                    h('td', { class: 'num' }, r.mean_cost === null ? '—' : usd(r.mean_cost)),
                    h('td', { class: 'num' }, r.mean_grade === null ? '—' : r.mean_grade.toFixed(1)),
                    h('td', { class: 'num' }, r.net_rating > 0 ? `+${r.net_rating}` : String(r.net_rating)),
                  ),
                ),
              ),
            ),
          )
        : h('p', { class: 'hint' }, `${none} yet.`),
      h('p', { class: 'hint' }, 'Routing uses a task type’s stats once three threads of that type have finished.'),
    );
  }

  // ---- rules
  async function renderRules() {
    if (!admin) {
      setChildren(slots.rules, head('m-rules', 'Priority rules'), h('p', { class: 'hint' }, 'Rules need an admin token.'));
      return;
    }
    let rules;
    try {
      rules = (await get('/v1/registry/rules', { signal })) || [];
    } catch (err) {
      if (isAbort(err)) return;
      setChildren(slots.rules, head('m-rules', 'Priority rules'), errorBanner(err, renderRules));
      return;
    }
    const summary = (r) =>
      [
        r.when_tag && `tag ${r.when_tag}`,
        r.when_task_type && `task ${r.when_task_type}`,
        r.require && `require ${r.require}`,
        r.forbid && `forbid ${r.forbid}`,
        r.prefer && `prefer ${r.prefer}`,
      ]
        .filter(Boolean)
        .join(' · ') || 'always';
    setChildren(
      slots.rules,
      head('m-rules', 'Priority rules'),
      h('p', { class: 'section-note' }, 'Rules filter and order models before stats choose one for a thread.'),
      rules.length
        ? h(
            'ul',
            { class: 'list card' },
            rules.map((r) =>
              h(
                'li',
                { class: 'list-row' },
                h('div', { class: 'grow' }, h('div', { class: 'row-title' }, r.text), h('div', { class: 'meta mono' }, summary(r.rule))),
                h(
                  'label',
                  { class: 'switch' },
                  h('input', {
                    type: 'checkbox',
                    role: 'switch',
                    'data-write': true,
                    checked: r.enabled,
                    'aria-label': `Rule enabled: ${r.text}`,
                    onchange: async (e) => {
                      try {
                        await post(`/v1/registry/rules/${r.id}`, { enabled: e.target.checked });
                      } catch (err) {
                        e.target.checked = !e.target.checked;
                        toastError(err);
                      }
                    },
                  }),
                ),
                h(
                  'button',
                  {
                    class: 'icon-btn',
                    'data-write': true,
                    type: 'button',
                    'aria-label': `Delete rule: ${r.text}`,
                    onclick: async () => {
                      if (!(await confirmAction({ title: 'Delete this rule?', body: `“${r.text}”`, confirmLabel: 'Delete', danger: true }))) return;
                      try {
                        await del(`/v1/registry/rules/${r.id}`);
                        renderRules();
                      } catch (err) {
                        toastError(err);
                      }
                    },
                  },
                  icon('trash'),
                ),
              ),
            ),
          )
        : null,
      ruleForm(rules.length === 0),
    );
  }

  function ruleForm(example) {
    const select = (name, options, label) =>
      h('label', { class: 'field' }, h('span', null, label), h('select', { class: 'select', name }, h('option', { value: '' }, '—'), options.map((o) => h('option', { value: o }, o))));
    const text = h('input', { class: 'input', name: 'text', required: true, placeholder: 'Sensitive data stays on this machine', value: example ? 'Sensitive data stays on this machine' : '' });
    const form = h(
      'form',
      {
        class: 'card card-pad',
        onsubmit: async (e) => {
          e.preventDefault();
          const f = new FormData(form);
          const rule = {};
          for (const k of ['when_tag', 'when_task_type', 'require', 'forbid', 'prefer']) {
            const v = String(f.get(k) || '').trim();
            if (v) rule[k] = v;
          }
          try {
            await post('/v1/registry/rules', { text: String(f.get('text')).trim(), rule });
            toast('Rule added.');
            renderRules();
          } catch (err) {
            toastError(err);
          }
        },
      },
      h('div', { class: 'field-label' }, 'Add a rule'),
      h('div', { class: 'row-form' }, h('label', { class: 'field' }, h('span', null, 'In your words'), text)),
      h(
        'div',
        { class: 'row-form' },
        h('label', { class: 'field' }, h('span', null, 'When tag'), h('input', { class: 'input', name: 'when_tag', placeholder: 'sensitive', value: example ? 'sensitive' : '' })),
        h('label', { class: 'field' }, h('span', null, 'When task type'), h('input', { class: 'input', name: 'when_task_type', placeholder: 'code' })),
        select('require', CONSTRAINTS, 'Require'),
        select('forbid', CONSTRAINTS, 'Forbid'),
        select('prefer', PREFERS, 'Prefer'),
      ),
      h('div', null, h('button', { class: 'btn btn-primary', type: 'submit', 'data-write': true }, icon('plus'), 'Add rule')),
    );
    if (example) requestAnimationFrame(() => (form.elements.require.value = 'local'));
    return form;
  }

  // ---- import
  async function renderImport() {
    if (!admin) {
      slots.import.hidden = true;
      return;
    }
    const providers = await ensureProviders();
    if (signal.aborted) return;
    const status = h('div', { role: 'status', class: 'hint' });
    const form = h(
      'form',
      {
        class: 'card card-pad',
        onsubmit: async (e) => {
          e.preventDefault();
          const f = new FormData(form);
          const body = { provider: String(f.get('provider')).trim(), model: String(f.get('model')).trim() };
          const id = String(f.get('id') || '').trim();
          if (id) body.id = id;
          const roles = ROLES.filter((r) => f.get(`role-${r}`));
          if (roles.length) body.roles = roles;
          status.textContent = 'Importing from the catalog…';
          try {
            const r = await post('/v1/models/import', body);
            status.textContent = `Saved ${r.model.id}.${r.proposal_id ? ' Drafted use cases are waiting in Proposals.' : ''}`;
            form.reset();
            renderModelList();
            renderProposals();
          } catch (err) {
            status.textContent = describe(err);
          }
        },
      },
      h(
        'div',
        { class: 'row-form' },
        h(
          'label',
          { class: 'field' },
          h('span', null, 'Provider'),
          providers.length
            ? h('select', { class: 'select', name: 'provider', required: true }, providers.map((p) => h('option', { value: p.id }, `${p.id} (${p.kind})`)))
            : h('input', { class: 'input', name: 'provider', required: true, placeholder: 'openrouter' }),
        ),
        h('label', { class: 'field' }, h('span', null, 'Provider’s model name'), h('input', { class: 'input mono', name: 'model', required: true, placeholder: 'anthropic/claude-sonnet-4' })),
        h('label', { class: 'field' }, h('span', null, 'Registry id (optional)'), h('input', { class: 'input', name: 'id', placeholder: 'sonnet' })),
      ),
      h(
        'fieldset',
        { class: 'row-form plain' },
        h('legend', { class: 'field-label' }, 'Default for (optional)'),
        ROLES.map((r) => h('label', { class: 'check' }, h('input', { type: 'checkbox', name: `role-${r}` }), r)),
      ),
      h('div', null, h('button', { class: 'btn btn-primary', type: 'submit', 'data-write': true }, 'Import'), ' ', status),
    );
    setChildren(slots.import, head('m-import', 'Import from a catalog'), h('p', { class: 'section-note' }, 'Fills prices, context and modalities from the provider’s catalog (OpenRouter), and drafts use cases for you to accept.'), form);
  }

  const narrowMq = matchMedia('(max-width: 760px)');
  const onNarrow = () => {
    if (!signal.aborted) renderModelList();
  };
  narrowMq.addEventListener('change', onNarrow);
  renderProposals();
  renderModelList();
  renderStats();
  renderRules();
  renderImport();
  return () => {
    narrowMq.removeEventListener('change', onNarrow);
    aborter.abort();
    unguard();
  };
}
