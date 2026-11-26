// The chat's model picker: a covering panel (a full-height sheet on phones, a
// large centred overlay with a scrim on desktop), because there are many models.
//
// A modal <dialog> holding a combobox search field and one listbox. Focus stays
// in the search field: typing filters, arrow keys move the active option
// (aria-activedescendant), Enter picks it, Escape closes and focus returns to the
// button that opened it. "Default model" comes first; then the models with this
// chat's role, then "Other models", each grouped under its provider (id, kind,
// key set / no key from GET /v1/providers). Provider headers are group labels,
// not options. A model reached through two providers is two entries, one under
// each.

import { h, icon, setChildren, setText, setAttr } from '../lib/dom.js';
import { get, isAbort } from '../lib/api.js';
import { store, on, isAdmin } from '../lib/store.js';
import { familyKey, familyMark, familyInfo } from '../lib/family.js';

let providersCache = null; // the last GET /v1/providers, shared between openings
const compact = new Intl.NumberFormat(undefined, { notation: 'compact', maximumFractionDigits: 1 });

function price(n) {
  return n < 0.1 && n > 0 ? n.toFixed(3) : n.toFixed(2);
}

function roleTitle(role) {
  return role === 'standard' ? 'Standard model' : 'Orchestrator model';
}

/**
 * Open the picker. `role` is 'orchestrator' or 'standard', `current` the chosen
 * registry id ('' for the default). Resolves with the picked id ('' = default), or
 * undefined when closed without a choice.
 */
export function openModelPicker({ role, current = '', opener = null }) {
  return new Promise((resolve) => {
    const aborter = new AbortController();
    let done = false;
    let activeValue = current;
    let options = []; // visible option elements, in order

    const titleEl = h('h2', { id: 'mp-title' }, roleTitle(role));
    const currentEl = h('p', { class: 'caption', id: 'mp-current' });
    const closeBtn = h('button', { class: 'icon-btn', type: 'button', 'aria-label': 'Close model picker', onclick: () => close(undefined) }, icon('close'));
    const search = h('input', {
      id: 'mp-search',
      class: 'input mp-search-input',
      type: 'text',
      role: 'combobox',
      'aria-expanded': 'true',
      'aria-controls': 'mp-list',
      'aria-autocomplete': 'list',
      'aria-describedby': 'mp-current mp-help',
      'aria-label': 'Search models',
      placeholder: 'Search by name, provider, family or role',
      autocomplete: 'off',
      spellcheck: 'false',
      autocapitalize: 'off',
      oninput: () => {
        activeValue = null;
        render();
      },
      onkeydown: onSearchKey,
    });
    const help = h('p', { class: 'visually-hidden', id: 'mp-help' }, 'Arrow keys move through the models, Enter chooses, Escape closes.');
    const countEl = h('p', { class: 'visually-hidden', role: 'status' });
    const list = h('div', { id: 'mp-list', class: 'mp-list', role: 'listbox', 'aria-label': `${roleTitle(role)} choices` });
    const emptyEl = h('p', { class: 'mp-empty hint', hidden: true });
    const providerNote = h('p', { class: 'hint mp-provider-note', hidden: true });
    // Focus normally stays in the search field; the list is a tab stop too, so the
    // long list can be scrolled from the keyboard without picking anything.
    const scroller = h('div', { class: 'mp-body', tabindex: '0', role: 'region', 'aria-label': 'Model list' }, providerNote, list, emptyEl);

    const dialog = h(
      'dialog',
      { class: 'sheet-dialog model-picker', 'aria-labelledby': 'mp-title', 'aria-describedby': 'mp-current' },
      h(
        'div',
        { class: 'mp-head' },
        h('div', { class: 'sheet-head-row' }, titleEl, closeBtn),
        currentEl,
        h('div', { class: 'mp-search' }, icon('search'), search),
        help,
        countEl,
      ),
      scroller,
    );

    function close(value) {
      if (done) return;
      done = true;
      aborter.abort();
      offModels();
      if (dialog.open) dialog.close();
      dialog.remove();
      if (opener?.isConnected) opener.focus({ preventScroll: true });
      resolve(value);
    }

    dialog.addEventListener('cancel', (e) => {
      e.preventDefault();
      close(undefined);
    });
    dialog.addEventListener('click', (e) => {
      if (e.target === dialog) close(undefined); // the scrim
    });

    function onSearchKey(e) {
      if (e.isComposing) return;
      if (e.key === 'Escape') {
        // Ours, not the app's: the shell's Escape would also close the inspector.
        e.preventDefault();
        e.stopPropagation();
        close(undefined);
        return;
      }
      if (e.key === 'Enter') {
        e.preventDefault();
        const el = options.find((o) => o.dataset.value === activeValue);
        if (el) close(el.dataset.value);
        return;
      }
      const moves = { ArrowDown: 1, ArrowUp: -1, PageDown: 8, PageUp: -8 };
      if (e.key in moves && options.length) {
        e.preventDefault();
        const i = options.findIndex((o) => o.dataset.value === activeValue);
        const next = i < 0 ? 0 : Math.max(0, Math.min(options.length - 1, i + moves[e.key]));
        setActive(options[next].dataset.value, true);
      }
    }

    function setActive(value, scroll) {
      activeValue = value;
      let activeEl = null;
      for (const o of options) {
        const isActive = o.dataset.value === value;
        o.classList.toggle('active', isActive);
        if (isActive) activeEl = o;
      }
      setAttr(search, 'aria-activedescendant', activeEl ? activeEl.id : null);
      if (activeEl && scroll) activeEl.scrollIntoView({ block: 'nearest' });
    }

    function providerList() {
      return providersCache || [];
    }

    function roleDefault() {
      return store.models.find((m) => m.roles.includes(role)) || null;
    }

    function renderCurrent() {
      const d = roleDefault();
      setText(
        currentEl,
        current ? `Current: ${current}` : `Current: Default model${d ? ` (uses ${d.id})` : ''}`,
      );
    }

    function matches(m, words, provider) {
      if (!words.length) return true;
      const fam = familyInfo(familyKey(m));
      const hay = [
        m.id,
        m.model,
        m.provider_id,
        provider?.kind || '',
        m.roles.join(' '),
        fam.key === 'unknown' ? '' : `${fam.label} ${fam.maker}`,
        m.free ? 'free' : 'paid',
        m.local ? 'local' : '',
      ]
        .join(' ')
        .toLowerCase();
      return words.every((w) => hay.includes(w));
    }

    function optionEl(value, idx, content, selected) {
      return h(
        'div',
        {
          id: `mp-opt-${idx}`,
          class: ['mp-option', selected && 'selected'],
          role: 'option',
          'aria-selected': selected ? 'true' : 'false',
          dataset: { value },
          onclick: () => close(value),
        },
        content,
        h('span', { class: 'mp-check', 'aria-hidden': 'true' }, selected ? icon('check') : null),
      );
    }

    function modelOption(m, idx) {
      const famKey = familyKey(m);
      const fam = familyInfo(famKey);
      const known = m.input_price > 0 || m.output_price > 0;
      const meta = [];
      if (famKey !== 'unknown') meta.push(h('span', null, `${fam.label} family`));
      meta.push(h('span', null, m.roles.length ? `Roles: ${m.roles.join(', ')}` : 'No roles'));
      if (!m.free && known) meta.push(h('span', { class: 'mp-price' }, `$${price(m.input_price)} in · $${price(m.output_price)} out per 1M`));
      if (m.context_window > 0) meta.push(h('span', null, `${compact.format(m.context_window)} context`));
      return optionEl(
        m.id,
        idx,
        h(
          'div',
          { class: ['mp-opt-main', 'has-fam', `fam-${famKey}`] },
          familyMark(famKey),
          h(
            'div',
            { class: 'mp-opt-text' },
            h(
              'div',
              { class: 'mp-opt-line' },
              h('span', { class: 'mp-opt-id' }, m.id),
              m.free ? h('span', { class: 'badge accent' }, 'Free') : h('span', { class: 'badge muted' }, 'Paid'),
              m.local ? h('span', { class: 'badge' }, 'Local') : null,
              m.roles.includes(role) ? h('span', { class: 'badge' }, 'Role default') : null,
            ),
            h('div', { class: 'mp-opt-model mono' }, m.model),
            h('div', { class: 'meta mp-opt-meta' }, meta),
          ),
        ),
        current === m.id,
      );
    }

    function render() {
      const words = search.value.toLowerCase().split(/\s+/).filter(Boolean);
      const providers = new Map(providerList().map((p) => [p.id, p]));
      const providersKnown = Boolean(providersCache);
      const shown = store.models.filter((m) => matches(m, words, providers.get(m.provider_id)));
      const primary = shown.filter((m) => m.roles.includes(role));
      const other = shown.filter((m) => !m.roles.includes(role));
      let idx = 0;
      const parts = [];

      const d = roleDefault();
      parts.push(
        optionEl(
          '',
          idx++,
          h(
            'div',
            { class: 'mp-opt-main' },
            h('span', { class: 'fam-mark fam-default', 'aria-hidden': 'true' }, icon('models')),
            h(
              'div',
              { class: 'mp-opt-text' },
              h('div', { class: 'mp-opt-line' }, h('span', { class: 'mp-opt-id' }, 'Default model')),
              h('div', { class: 'mp-opt-meta meta' }, h('span', null, d ? `The ${role} default, now ${d.id}` : `The hub's ${role} default`)),
            ),
          ),
          current === '',
        ),
      );

      const sections = [];
      if (primary.length) sections.push({ title: role === 'standard' ? 'Standard models' : 'Orchestrator models', models: primary });
      if (other.length) sections.push({ title: primary.length ? 'Other models' : 'All models', models: other });

      sections.forEach((section, s) => {
        parts.push(h('div', { class: 'mp-section', 'aria-hidden': 'true' }, section.title));
        const byProvider = new Map();
        for (const m of section.models) {
          if (!byProvider.has(m.provider_id)) byProvider.set(m.provider_id, []);
          byProvider.get(m.provider_id).push(m);
        }
        [...byProvider.keys()]
          .sort((a, b) => a.localeCompare(b))
          .forEach((pid, g) => {
            const p = providers.get(pid);
            const models = byProvider.get(pid).sort((a, b) => a.id.localeCompare(b.id));
            const labelId = `mp-g-${s}-${g}`;
            parts.push(
              h(
                'div',
                { class: 'mp-group', role: 'group', 'aria-labelledby': labelId },
                h(
                  'div',
                  { class: 'mp-provider', id: labelId },
                  h('span', { class: 'visually-hidden' }, `${section.title}, provider `),
                  h('span', { class: 'mp-provider-id' }, pid || 'Unknown provider'),
                  p?.kind ? h('span', { class: 'badge' }, p.kind) : null,
                  p ? h('span', { class: ['badge', p.has_key ? 'accent' : 'muted'] }, p.has_key ? 'Key set' : 'No key') : null,
                  p?.via_device ? h('span', { class: 'badge muted' }, `via ${p.via_device}`) : null,
                  providersKnown && !p ? h('span', { class: 'badge warn' }, 'Unknown provider') : null,
                ),
                models.map((m) => modelOption(m, idx++)),
              ),
            );
          });
      });

      setChildren(list, parts);
      options = [...list.querySelectorAll('[role="option"]')];
      const count = shown.length;
      emptyEl.hidden = count > 0 || !words.length;
      setText(emptyEl, words.length ? `No models match “${search.value.trim()}”. Default model is still available.` : '');
      setText(countEl, words.length ? (count === 1 ? '1 model matches' : `${count} models match`) : '');
      const keep = options.some((o) => o.dataset.value === activeValue);
      // Typing puts the first match under the cursor; otherwise stay on the current choice.
      const fallback = words.length && options.length > 1 ? options[1].dataset.value : current;
      setActive(keep ? activeValue : options.some((o) => o.dataset.value === fallback) ? fallback : '', false);
      renderCurrent();
    }

    async function refreshProviders() {
      if (!isAdmin()) return;
      try {
        const list = (await get('/v1/providers', { signal: aborter.signal })) || [];
        if (aborter.signal.aborted) return;
        const changed = JSON.stringify(list) !== JSON.stringify(providersCache);
        providersCache = list;
        providerNote.hidden = true;
        if (changed && !done) render();
      } catch (err) {
        if (isAbort(err) || aborter.signal.aborted) return;
        if (!providersCache) {
          providerNote.hidden = false;
          setText(providerNote, 'Provider details (kind, key) could not be loaded. Models are still grouped by provider id.');
        }
      }
    }

    const offModels = on('models', () => {
      if (!done) render();
    });

    render();
    document.body.append(dialog);
    dialog.showModal();
    search.focus();
    requestAnimationFrame(() => {
      const el = options.find((o) => o.dataset.value === activeValue);
      el?.scrollIntoView({ block: 'nearest' });
    });
    refreshProviders();
  });
}
