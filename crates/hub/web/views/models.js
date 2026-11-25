// Model registry: models, what threads taught it (stats), proposals waiting for
// you, priority rules, and importing a model from a provider's catalog.

import { h, icon, setChildren } from '../lib/dom.js';
import { get, post, del, qs, enc, describe } from '../lib/api.js';
import { store, isAdmin } from '../lib/store.js';
import { loadModels } from '../lib/live.js';
import { bar, confirmAction, toast, toastError, skeleton, errorBanner } from '../lib/ui.js';
import { num, usd, pct, duration, ago } from '../lib/format.js';

const CONSTRAINTS = ['local', 'remote', 'free', 'paid'];
const PREFERS = ['cheapest', 'local', 'free'];

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

  // ---- models
  async function renderModelList() {
    setChildren(slots.models, head('m-models', 'Models'), skeleton(3));
    try {
      await loadModels();
    } catch (err) {
      setChildren(slots.models, head('m-models', 'Models'), errorBanner(err, renderModelList));
      return;
    }
    const models = store.models;
    if (!models.length) {
      setChildren(slots.models, head('m-models', 'Models'), h('div', { class: 'card card-pad' }, h('p', null, 'No models yet. Add a provider key in Settings (or with grouper keys set), then import a model below.')));
      return;
    }
    setChildren(
      slots.models,
      head('m-models', 'Models'),
      h(
        'div',
        { class: 'table-wrap' },
        h(
          'table',
          { class: 'table' },
          h('thead', null, h('tr', null, ['Id', 'Provider model', 'Roles', 'Flags', 'Context', 'In $/M', 'Out $/M', ''].map((t, i) => h('th', { class: i >= 4 && i <= 6 ? 'num' : null, scope: 'col' }, t)))),
          h(
            'tbody',
            null,
            models.map((m) =>
              h(
                'tr',
                null,
                h('td', null, h('strong', null, m.id)),
                h('td', { class: 'mono' }, `${m.provider_id} / ${m.model}`),
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
                        'button',
                        {
                          class: 'icon-btn',
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
                      )
                    : null,
                ),
              ),
            ),
          ),
        ),
      ),
      h('p', { class: 'hint' }, 'Prices are USD per million tokens. Use cases and cache prices are in the CLI (grouper models list).'),
    );
  }

  // ---- proposals
  async function renderProposals() {
    if (!admin) {
      slots.proposals.hidden = true;
      return;
    }
    let list;
    try {
      list = (await get(`/v1/registry/proposals${qs({ status: 'pending' })}`)) || [];
    } catch (err) {
      setChildren(slots.proposals, head('m-proposals', 'Proposals'), errorBanner(err, renderProposals));
      return;
    }
    store.pendingProposals = list.length;
    slots.proposals.hidden = !list.length;
    setChildren(
      slots.proposals,
      head('m-proposals', `Proposals (${list.length})`),
      h('p', { class: 'section-note' }, 'Changes the registry suggests from what threads did. Nothing changes until you accept.'),
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
              h('button', { class: 'btn btn-sm', type: 'button', onclick: () => decide(p, false) }, 'Reject'),
              h('button', { class: 'btn btn-sm btn-primary', type: 'button', onclick: () => decide(p, true) }, 'Accept'),
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
        stats = await get('/v1/registry/stats');
      } catch (err) {
        setChildren(slots.stats, head('m-stats', 'Stats'), errorBanner(err, renderStats));
        return;
      }
    }
    const rows = (stats[statsTab] || []).slice().sort((a, b) => a.subject.localeCompare(b.subject) || a.task_type.localeCompare(b.task_type));
    const seg = h(
      'div',
      { class: 'seg', role: 'radiogroup', 'aria-label': 'Stats for' },
      ['models', 'devices'].map((v) =>
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
          v === 'models' ? 'Models' : 'Devices',
        ),
      ),
    );
    const none = 'No finished threads';
    setChildren(
      slots.stats,
      head('m-stats', 'Stats', seg),
      rows.length
        ? h(
            'div',
            { class: 'table-wrap' },
            h(
              'table',
              { class: 'table' },
              h('thead', null, h('tr', null, [statsTab === 'models' ? 'Model' : 'Device', 'Task type', 'Done', 'Failed', 'Stopped', 'Success', 'Median', 'Mean cost', 'Grade', 'Rating'].map((t, i) => h('th', { class: i >= 2 ? 'num' : null, scope: 'col' }, t)))),
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
      rules = (await get('/v1/registry/rules')) || [];
    } catch (err) {
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
      h('div', null, h('button', { class: 'btn btn-primary', type: 'submit' }, icon('plus'), 'Add rule')),
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
    let providers = [];
    try {
      providers = (await get('/v1/providers')) || [];
    } catch {
      /* the form still works with a typed provider id */
    }
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
          const roles = ['orchestrator', 'standard', 'thread'].filter((r) => f.get(`role-${r}`));
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
        ['orchestrator', 'standard', 'thread'].map((r) => h('label', { class: 'check' }, h('input', { type: 'checkbox', name: `role-${r}` }), r)),
      ),
      h('div', null, h('button', { class: 'btn btn-primary', type: 'submit' }, 'Import'), ' ', status),
    );
    setChildren(slots.import, head('m-import', 'Import from a catalog'), h('p', { class: 'section-note' }, 'Fills prices, context and modalities from the provider’s catalog (OpenRouter), and drafts use cases for you to accept.'), form);
  }

  renderProposals();
  renderModelList();
  renderStats();
  renderRules();
  renderImport();
}
