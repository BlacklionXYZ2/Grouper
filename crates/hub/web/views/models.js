// Model registry: models grouped by provider (edit, refresh catalog prices, run probes, delete),
// what threads and probes taught it (stats), proposals waiting for you,
// priority rules, and importing a model from a provider's catalog.

import { h, icon, setChildren, keepFocus } from '../lib/dom.js';
import { get, post, put, del, qs, enc, describe, isAbort } from '../lib/api.js';
import { store, isAdmin, deviceName } from '../lib/store.js';
import { loadModels } from '../lib/live.js';
import { bar, sheet, confirmAction, toast, toastError, skeleton, errorBanner, guardWrites } from '../lib/ui.js';
import { num, usd, pct, duration, ago, plural } from '../lib/format.js';
import { familyKey, familyMark } from '../lib/family.js';
import { isLocalModel, modelBadge, price } from './model-picker.js';

const CONSTRAINTS = ['local', 'remote', 'free', 'paid'];
const PREFERS = ['cheapest', 'local', 'free'];
const ROLES = ['orchestrator', 'standard', 'thread'];
const COLLAPSED_KEY = 'grouper.models.collapsed';

function collapsedProviders() {
  try {
    const raw = JSON.parse(localStorage.getItem(COLLAPSED_KEY) || '[]');
    return new Set(Array.isArray(raw) ? raw.filter((id) => typeof id === 'string') : []);
  } catch {
    return new Set();
  }
}

function rememberCollapsed(id, isOpen) {
  try {
    const set = collapsedProviders();
    if (isOpen) set.delete(id);
    else set.add(id);
    localStorage.setItem(COLLAPSED_KEY, JSON.stringify([...set]));
  } catch {
    /* storage unavailable */
  }
}

function providerPlace(provider) {
  if (!provider) return '';
  if (provider.via_device) return `via ${deviceName(provider.via_device)}`;
  if (!provider.base_url) return '';
  try {
    return new URL(provider.base_url).host;
  } catch {
    return '';
  }
}

function isOpenRouterProvider(provider) {
  if (!provider?.base_url) return false;
  try {
    const host = new URL(provider.base_url).hostname.toLowerCase();
    return host === 'openrouter.ai' || host.endsWith('.openrouter.ai');
  } catch {
    return false;
  }
}

export function renderModels(container, { id: targetId = null } = {}) {
  const admin = isAdmin();
  // #/models/<id>: open that model's group, bring its row into view and focus it,
  // once, on the first list that loads.
  let pendingTarget = targetId || null;
  const body = h('div', { class: 'view-body' });
  container.append(h('section', { class: 'view' }, bar({ title: 'Models', caption: 'Registry, stats, proposals and rules' }), body));

  const slots = {
    proposals: h('section', { class: 'section', 'aria-labelledby': 'm-proposals' }),
    models: h('section', { class: 'section', 'aria-labelledby': 'm-models' }),
    stats: h('section', { class: 'section', 'aria-labelledby': 'm-stats' }),
    rules: h('section', { class: 'section', 'aria-labelledby': 'm-rules' }),
    import: h('section', { class: 'section', id: 'model-import', 'aria-labelledby': 'm-import' }),
  };
  body.append(slots.proposals, slots.models, slots.stats, slots.rules, slots.import);

  const head = (id, title, extra) => h('div', { class: 'section-head' }, h('h2', { id }, title), extra || null);
  // Leaving the view cancels its reads; writes are paused while offline.
  const aborter = new AbortController();
  const { signal } = aborter;
  const unguard = guardWrites(body);
  let providersById = new Map();
  // Unknown is only for a provider id missing from a list we actually loaded.
  // A failed or non-admin fetch must not file every model under Unknown.
  let providersLoaded = false;

  async function ensureProviders() {
    if (!admin) return [];
    try {
      const list = (await get('/v1/providers', { signal })) || [];
      providersById = new Map(list.map((p) => [p.id, p]));
      providersLoaded = true;
      return list;
    } catch (err) {
      if (isAbort(err)) throw err;
      return [...providersById.values()];
    }
  }

  function groupModels(models) {
    const buckets = new Map();
    for (const m of models) {
      const id = m.provider_id || '';
      if (!buckets.has(id)) buckets.set(id, []);
      buckets.get(id).push(m);
    }
    for (const list of buckets.values()) list.sort((a, b) => a.id.localeCompare(b.id));

    if (!providersLoaded) {
      return [...buckets.keys()].sort((a, b) => a.localeCompare(b)).map((id) => ({
        key: id || '__unknown__',
        title: id || 'Unknown provider',
        provider: null,
        models: buckets.get(id),
        unknown: !id,
      }));
    }

    const groups = [...providersById.keys()]
      .sort((a, b) => a.localeCompare(b))
      .filter((id) => buckets.has(id))
      .map((id) => ({
        key: id,
        title: id,
        provider: providersById.get(id),
        models: buckets.get(id),
        unknown: false,
      }));
    const unknown = [];
    for (const [id, list] of buckets) {
      if (!providersById.has(id)) unknown.push(...list);
    }
    unknown.sort((a, b) => a.id.localeCompare(b.id));
    if (unknown.length) {
      groups.push({ key: '__unknown__', title: 'Unknown provider', provider: null, models: unknown, unknown: true });
    }
    return groups;
  }

  async function deleteModel(m) {
    if (!(await confirmAction({ title: `Delete ${m.id}?`, body: 'It leaves the registry. Conversations pinned to it fall back to the default.', confirmLabel: 'Delete', danger: true }))) return;
    try {
      await del(`/v1/models/${enc(m.id)}`);
      toast(`Deleted ${m.id}.`);
      renderModelList();
    } catch (err) {
      toastError(err);
    }
  }

  function modelActions(m, withText) {
    const isCatalog = isOpenRouterProvider(providersById.get(m.provider_id));
    const btn = (visible, label, ic, title, onclick) =>
      withText
        ? h('button', { class: 'btn btn-sm', type: 'button', 'data-write': true, 'aria-label': label, onclick }, icon(ic), visible)
        : h('button', { class: 'icon-btn', type: 'button', 'data-write': true, 'aria-label': label, title, onclick }, icon(ic));
    return h(
      'div',
      { class: withText ? 'bar-actions' : 'table-actions' },
      btn('Edit', `Edit ${m.id}`, 'edit', 'Edit model', () => openEditModelSheet(m)),
      isCatalog ? btn('Refresh prices', `Refresh prices for ${m.id}`, 'refresh', 'Refresh prices from catalog', (e) => refreshModelPrices(m, e.currentTarget)) : null,
      btn('Run probes', `Run probes for ${m.id}`, 'play', 'Run probes', () => openProbeSheet(m)),
      btn('Delete', `Delete ${m.id}`, 'trash', 'Delete', () => deleteModel(m)),
    );
  }

  function narrowCard(m) {
    const famKey = familyKey(m);
    const provider = providersById.get(m.provider_id) || null;
    const badge = modelBadge(m, provider, { right: true });
    const known = m.input_price > 0 || m.output_price > 0;
    const priceText = !m.free
      ? (known ? `$${price(m.input_price)} in · $${price(m.output_price)} out / 1M` : 'No price set')
      : isLocalModel(m, provider) ? 'Free' : null;
    return h(
      'li',
      { class: 'list-row model-row', dataset: { modelId: m.id } },
      h(
        'div',
        { class: ['model-row-main', 'has-fam', `fam-${famKey}`] },
        familyMark(famKey),
        h(
          'div',
          { class: 'grow' },
          h('div', { class: 'model-row-head' }, h('div', { class: 'row-title' }, m.id), badge),
          h(
            'div',
            { class: 'row-sub model-row-sub' },
            h('span', { class: 'mono' }, `${m.provider_id} / ${m.model}`),
            priceText ? h('span', null, priceText) : null,
          ),
          h(
            'div',
            { class: 'meta' },
            h('span', null, m.roles.length ? m.roles.join(', ') : 'No roles'),
            m.context_window ? h('span', null, `${num(m.context_window)} ctx`) : null,
            m.use_cases ? h('span', null, m.use_cases) : null,
          ),
        ),
      ),
      admin ? modelActions(m, true) : null,
    );
  }

  function cacheNote(m) {
    if (m.cache_read_price !== null && m.cache_read_price !== undefined) {
      const write = m.cache_write_price !== null && m.cache_write_price !== undefined ? ` / ${m.cache_write_price.toFixed(2)}` : '';
      return `cache ${m.cache_read_price.toFixed(2)}${write}`;
    }
    if (m.cache_write_price !== null && m.cache_write_price !== undefined) return `cache write ${m.cache_write_price.toFixed(2)}`;
    return '';
  }

  function wideRow(m) {
    const famKey = familyKey(m);
    const provider = providersById.get(m.provider_id) || null;
    const badge = modelBadge(m, provider);
    const note = cacheNote(m);
    return h(
      'tr',
      { class: 'model-row', dataset: { modelId: m.id } },
      h(
        'td',
        { class: 'wrap' },
        h('div', { class: ['model-id-cell', 'has-fam', `fam-${famKey}`] }, familyMark(famKey), h('strong', null, m.id)),
        m.use_cases ? h('div', { class: 'row-sub model-use-cases' }, m.use_cases) : null,
      ),
      h('td', { class: 'mono' }, `${m.provider_id} / ${m.model}`, note ? h('div', { class: 'hint' }, note) : null),
      h('td', null, m.roles.length ? m.roles.join(', ') : '—'),
      h('td', null, badge || h('span', { class: 'hint' }, '—')),
      h('td', { class: 'num' }, m.context_window ? num(m.context_window) : '—'),
      h('td', { class: 'num' }, m.input_price.toFixed(2)),
      h('td', { class: 'num' }, m.output_price.toFixed(2)),
      h('td', null, admin ? modelActions(m, false) : null),
    );
  }

  function providerSection(group, narrow, collapsed) {
    const provider = group.provider;
    const place = providerPlace(provider);
    const keyLabel = provider ? (provider.has_key ? 'key set' : 'no key') : '';
    const add = admin && !group.unknown
      ? h(
          'button',
          {
            class: 'btn btn-sm provider-add',
            type: 'button',
            'data-write': true,
            'aria-label': `Add model for ${group.title}`,
            onclick: () => focusImport(group.key),
          },
          'Add model',
        )
      : null;
    const summaryId = `provider-summary-${group.key}`;
    const summary = h(
      'summary',
      { id: summaryId },
      icon('chevron'),
      h(
        'span',
        { class: 'grow' },
        h('span', { class: 'provider-id' }, group.title),
        h(
          'span',
          { class: 'provider-meta' },
          provider?.kind ? h('span', { class: 'badge' }, provider.kind) : null,
          place ? h('span', { class: 'badge muted' }, place) : null,
          keyLabel ? h('span', { class: ['badge', provider.has_key ? 'ok' : 'muted'] }, keyLabel) : null,
          h('span', { class: 'badge muted' }, plural(group.models.length, 'model')),
        ),
      ),
    );
    const body = narrow
      ? h('ul', { class: 'list' }, group.models.map(narrowCard))
      : h(
          'div',
          { class: 'table-wrap', tabindex: '0', role: 'region', 'aria-label': `${group.title} models` },
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
            h('tbody', null, group.models.map(wideRow)),
          ),
        );
    const details = h('details', { class: 'provider-group' }, summary, h('div', { class: 'provider-body' }, body));
    details.open = !collapsed.has(group.key);
    // Save on click, not toggle. In Chrome the toggle event is queued after the
    // click returns, so a reload can win, and connecting an open <details> also
    // fires toggle and was erasing the stored id. During the click, `open` is
    // still the old state; the summary's default action flips it afterwards.
    summary.addEventListener('click', () => rememberCollapsed(group.key, !details.open));
    return h('div', { class: add ? 'provider-shell has-add' : 'provider-shell' }, add, details);
  }

  function focusImport(providerId) {
    const form = slots.import.querySelector('form');
    const field = form?.elements?.provider;
    if (field && providerId) {
      if (field.tagName === 'SELECT') {
        const has = [...field.options].some((opt) => opt.value === providerId);
        if (has) field.value = providerId;
      } else {
        field.value = providerId;
      }
    }
    (form || slots.import).scrollIntoView({ block: 'nearest' });
    field?.focus({ preventScroll: true });
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
      showTarget();
      return;
    }
    const narrow = matchMedia('(max-width: 760px)').matches;
    const groups = groupModels(models);
    const collapsed = collapsedProviders();
    const unavailable =
      admin && !providersLoaded
        ? h('p', { class: 'hint' }, 'Provider details are unavailable, so groups are named by the provider id stored on each model.')
        : null;
    keepFocus(slots.models, () => {
      setChildren(
        slots.models,
        head('m-models', 'Models'),
        unavailable,
        ...groups.map((group) => providerSection(group, narrow, collapsed)),
        h('p', { class: 'hint' }, 'Prices are USD per million tokens. Edit a model to change roles, prices (including cache prices), context window, use cases or local flag.'),
      );
    });
    showTarget();
  }

  /** The deep-linked model: expand its group, scroll to it, mark it briefly, focus it. */
  function showTarget() {
    if (!pendingTarget) return;
    const id = pendingTarget;
    pendingTarget = null;
    const row = [...slots.models.querySelectorAll('.model-row')].find((el) => el.dataset.modelId === id);
    if (!row) {
      toast(`No model “${id}” in the registry. Showing all models.`);
      return;
    }
    const details = row.closest('details');
    if (details) details.open = true; // not remembered: collapsing stays the person's choice
    const still = matchMedia('(prefers-reduced-motion: reduce)').matches;
    row.setAttribute('tabindex', '-1');
    row.classList.add('is-target');
    row.addEventListener('blur', () => row.removeAttribute('tabindex'), { once: true });
    setTimeout(() => row.classList.remove('is-target'), 2400);
    row.scrollIntoView({ block: 'center', behavior: still ? 'auto' : 'smooth' });
    // Don't take focus from something the person moved to while the list loaded.
    const active = document.activeElement;
    const free = !active || active === document.body || active === container.querySelector('h1') || active === container.closest('main');
    if (free) row.focus({ preventScroll: true });
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
