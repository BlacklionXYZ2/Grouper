// The chat's model picker: a covering panel (a full-height sheet on phones, a
// large centred overlay with a scrim on desktop), because there are many models.
//
// A modal <dialog> holding a combobox search field and one listbox. Focus stays
// in the search field: typing filters, arrow keys move the active option
// (aria-activedescendant), Enter picks it, Escape closes and focus returns to the
// button that opened it.
//
// Sections: "Default model" at the top, then "Suggested" (role defaults — no
// last-used model history is stored by the hub or client), then "Local" (models
// marked local, via_device, or using a local provider URL), then "More models…"
// which expands all models grouped under their provider headers (id, kind, key
// status, via_device). Typing to filter automatically reveals matching provider
// groups. A model reached through two providers appears under each provider.

import { h, icon, setChildren, setText, setAttr } from '../lib/dom.js';
import { get, isAbort } from '../lib/api.js';
import { store, on, isAdmin, deviceName } from '../lib/store.js';
import { familyKey, familyMark, familyInfo } from '../lib/family.js';

let providersCache = null; // the last GET /v1/providers, shared between openings

export function price(n) {
  return n < 0.1 && n > 0 ? n.toFixed(3) : n.toFixed(2);
}

function isLocalUrl(url) {
  if (!url) return false;
  try {
    const host = new URL(url).hostname.replace(/^\[|\]$/g, '').toLowerCase();
    return host === 'localhost' || host === '::1' || host.startsWith('127.') || host.endsWith('.local') || host.endsWith('.localhost');
  } catch {
    return false;
  }
}

export function isLocalModel(m, provider) {
  return Boolean(m.local || provider?.via_device || isLocalUrl(provider?.base_url));
}

/** At most one right-aligned badge: Local (with host device if remote-hosted) or Free. */
export function modelBadge(m, provider, { right = false } = {}) {
  if (isLocalModel(m, provider)) {
    const host = provider?.via_device ? deviceName(provider.via_device) : '';
    return h('span', { class: ['badge', 'ok', right && 'mp-badge'] }, host ? `Local · ${host}` : 'Local');
  }
  if (m.free) return h('span', { class: ['badge', 'ok', right && 'mp-badge'] }, 'Free');
  return null;
}

function priceText(m, isLocal) {
  const known = m.input_price > 0 || m.output_price > 0;
  if (!m.free) return known ? `$${price(m.input_price)} in · $${price(m.output_price)} out / 1M` : 'No price set';
  return isLocal ? 'Free' : null;
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
    let activeKey = null; // unique option key (since a model can appear in Suggested/Local and More models)
    let moreOpen = false;
    let options = []; // visible option elements, in order

    const titleEl = h('h2', { id: 'mp-title' }, roleTitle(role));
    const currentEl = h('p', { class: 'caption', id: 'mp-current' });
    const closeBtn = h(
      'button',
      {
        class: 'icon-btn mp-close',
        type: 'button',
        'aria-label': 'Close model picker',
        onclick: () => close(undefined),
      },
      h('span', { class: 'mp-close-mobile', 'aria-hidden': 'true' }, icon('back')),
      h('span', { class: 'mp-close-desktop', 'aria-hidden': 'true' }, icon('close')),
    );
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
        activeKey = null;
        render();
      },
      onkeydown: onSearchKey,
    });
    const help = h('p', { class: 'visually-hidden', id: 'mp-help' }, 'Arrow keys move through the models, Enter chooses, Escape closes.');
    const countEl = h('p', { class: 'visually-hidden', role: 'status' });
    const list = h('div', { id: 'mp-list', class: 'mp-list', role: 'listbox', 'aria-label': `${roleTitle(role)} choices` });
    const moreHost = h('div', { class: 'mp-more-wrap', hidden: true });
    const emptyEl = h('p', { class: 'mp-empty hint', hidden: true });
    const providerNote = h('p', { class: 'hint mp-provider-note', hidden: true });
    // Focus normally stays in the search field; the list is a tab stop too, so the
    // long list can be scrolled from the keyboard without picking anything.
    const scroller = h('div', { class: 'mp-body', tabindex: '0', role: 'region', 'aria-label': 'Model list' }, providerNote, list, moreHost, emptyEl);

    const dialog = h(
      'dialog',
      { class: 'sheet-dialog model-picker', 'aria-labelledby': 'mp-title', 'aria-describedby': 'mp-current' },
      h(
        'div',
        { class: 'mp-head' },
        h('div', { class: 'sheet-head-row mp-head-row' }, titleEl, closeBtn),
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
      offDevices();
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
        const el = options.find((o) => o.dataset.key === activeKey);
        if (el) close(el.dataset.value);
        return;
      }
      const moves = { ArrowDown: 1, ArrowUp: -1, PageDown: 8, PageUp: -8 };
      if (e.key in moves && options.length) {
        e.preventDefault();
        const i = options.findIndex((o) => o.dataset.key === activeKey);
        const next = i < 0 ? 0 : Math.max(0, Math.min(options.length - 1, i + moves[e.key]));
        setActive(options[next].dataset.key, true);
      }
    }

    function setActive(key, scroll) {
      activeKey = key;
      let activeEl = null;
      for (const o of options) {
        const isActive = o.dataset.key === key;
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
      const host = provider?.via_device ? deviceName(provider.via_device) : '';
      const local = isLocalModel(m, provider);
      const hay = [
        m.id,
        m.model,
        m.provider_id,
        provider?.kind || '',
        host,
        m.roles.join(' '),
        fam.key === 'unknown' ? '' : `${fam.label} ${fam.maker}`,
        m.free ? 'free' : 'paid',
        local ? 'local' : '',
      ]
        .join(' ')
        .toLowerCase();
      return words.every((w) => hay.includes(w));
    }

    function optionEl(value, key, idx, content, selected) {
      return h(
        'div',
        {
          id: `mp-opt-${idx}`,
          class: ['mp-option', selected && 'selected'],
          role: 'option',
          'aria-selected': selected ? 'true' : 'false',
          dataset: { value, key },
          onclick: () => close(value),
        },
        content,
        selected ? h('span', { class: 'mp-check', 'aria-hidden': 'true' }, icon('check')) : null,
      );
    }

    function modelOption(m, idx, keyPrefix, provider, { roleNote = null } = {}) {
      const famKey = familyKey(m);
      const isLocal = isLocalModel(m, provider);
      const badge = modelBadge(m, provider, { right: true });
      const pText = priceText(m, isLocal);
      const subItems = [
        h('span', { class: 'mp-opt-provider' }, m.provider_id || 'Unknown provider'),
        pText ? h('span', { class: 'mp-price' }, pText) : null,
        roleNote ? h('span', null, roleNote) : null,
      ];
      return optionEl(
        m.id,
        `${keyPrefix}:${m.id}`,
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
              badge,
            ),
            h('div', { class: 'meta mp-opt-meta mp-opt-sub' }, subItems),
          ),
        ),
        current === m.id,
      );
    }

    function render() {
      const words = search.value.toLowerCase().split(/\s+/).filter(Boolean);
      const filtering = words.length > 0;
      const providers = new Map(providerList().map((p) => [p.id, p]));
      const providersKnown = Boolean(providersCache);
      const shown = store.models.filter((m) => matches(m, words, providers.get(m.provider_id)));

      // Suggested: current role default(s) first, then any other role defaults.
      // ("Last used" model history isn't stored by the hub or client.)
      const byIdThenProvider = (a, b) => a.id.localeCompare(b.id) || a.provider_id.localeCompare(b.provider_id);
      const primaryRole = shown.filter((m) => m.roles.includes(role)).sort(byIdThenProvider);
      const otherRoles = shown.filter((m) => !m.roles.includes(role) && m.roles.length > 0).sort(byIdThenProvider);
      const suggested = [...primaryRole, ...otherRoles];
      const localModels = shown.filter((m) => isLocalModel(m, providers.get(m.provider_id))).sort(byIdThenProvider);

      let idx = 0;
      const parts = [];

      const d = roleDefault();
      parts.push(
        optionEl(
          '',
          'default',
          idx++,
          h(
            'div',
            { class: 'mp-opt-main' },
            h('span', { class: 'fam-mark fam-default', 'aria-hidden': 'true' }, icon('models')),
            h(
              'div',
              { class: 'mp-opt-text' },
              h('div', { class: 'mp-opt-line' }, h('span', { class: 'mp-opt-id' }, 'Default model')),
              h('div', { class: 'mp-opt-meta meta mp-opt-sub' }, h('span', null, d ? `Uses ${d.id}` : `The hub's ${role} default`)),
            ),
          ),
          current === '',
        ),
      );

      if (suggested.length) {
        parts.push(
          h(
            'div',
            { class: 'mp-group', role: 'group', 'aria-label': 'Suggested' },
            h('div', { class: 'mp-section', 'aria-hidden': 'true' }, 'Suggested'),
            suggested.map((m) =>
              modelOption(m, idx++, 'suggested', providers.get(m.provider_id), {
                roleNote: m.roles.includes(role) ? `${role} default` : `${m.roles.join(', ')} default`,
              }),
            ),
          ),
        );
      }

      if (localModels.length) {
        parts.push(
          h(
            'div',
            { class: 'mp-group', role: 'group', 'aria-label': 'Local' },
            h('div', { class: 'mp-section', 'aria-hidden': 'true' }, 'Local'),
            localModels.map((m) => modelOption(m, idx++, 'local', providers.get(m.provider_id))),
          ),
        );
      }

      const byProvider = new Map();
      for (const m of shown) {
        if (!byProvider.has(m.provider_id)) byProvider.set(m.provider_id, []);
        byProvider.get(m.provider_id).push(m);
      }
      const providerIds = [...byProvider.keys()].sort((a, b) => a.localeCompare(b));

      let firstMoreKey = null;
      if ((moreOpen || filtering) && providerIds.length) {
        parts.push(h('div', { class: 'mp-section', 'aria-hidden': 'true' }, 'More models'));
        providerIds.forEach((pid, g) => {
          const p = providers.get(pid);
          const models = byProvider.get(pid).sort((a, b) => a.id.localeCompare(b.id));
          const labelId = `mp-g-${g}`;
          const rows = models.map((m) => {
            const opt = modelOption(m, idx++, `more:${pid}`, p);
            if (!firstMoreKey) firstMoreKey = opt.dataset.key;
            return opt;
          });
          parts.push(
            h(
              'div',
              { class: 'mp-group', role: 'group', 'aria-labelledby': labelId },
              h(
                'div',
                { class: 'mp-provider', id: labelId },
                h('span', { class: 'visually-hidden' }, 'More models, provider '),
                h('span', { class: 'mp-provider-id' }, pid || 'Unknown provider'),
                p?.kind ? h('span', { class: 'badge' }, p.kind) : null,
                p ? h('span', { class: ['badge', p.has_key ? 'ok' : 'muted'] }, p.has_key ? 'Key set' : 'No key') : null,
                p?.via_device ? h('span', { class: 'badge muted' }, `via ${deviceName(p.via_device)}`) : null,
                providersKnown && !p ? h('span', { class: 'badge warn' }, 'Unknown provider') : null,
              ),
              rows,
            ),
          );
        });
      }

      setChildren(list, parts);
      options = [...list.querySelectorAll('[role="option"]')];

      if (!filtering && shown.length) {
        const providerCount = providerIds.length;
        moreHost.hidden = false;
        setChildren(
          moreHost,
          h(
            'button',
            {
              type: 'button',
              class: ['mp-more-btn', moreOpen && 'is-open'],
              'aria-expanded': moreOpen ? 'true' : 'false',
              'aria-controls': 'mp-list',
              onclick: () => {
                moreOpen = !moreOpen;
                const expanded = moreOpen;
                const key = render();
                if (expanded && key) setActive(key, true);
                search.focus({ preventScroll: true });
              },
            },
            h('span', { class: 'mp-more-label' }, moreOpen ? 'Hide provider groups' : 'More models…'),
            !moreOpen
              ? h('span', { class: 'mp-more-meta' }, `${providerCount} ${providerCount === 1 ? 'provider' : 'providers'}`)
              : null,
            icon(moreOpen ? 'down' : 'chevron'),
          ),
        );
      } else {
        moreHost.hidden = true;
        setChildren(moreHost);
      }

      const count = shown.length;
      emptyEl.hidden = count > 0 || !filtering;
      setText(emptyEl, filtering ? `No models match “${search.value.trim()}”. Default model is still available.` : '');
      setText(countEl, filtering ? (count === 1 ? '1 model matches' : `${count} models match`) : '');

      const keep = activeKey && options.some((o) => o.dataset.key === activeKey);
      let nextKey = keep ? activeKey : null;
      if (!nextKey) {
        if (filtering && options.length > 1) {
          // Typing puts the first match under the cursor.
          nextKey = options[1].dataset.key;
        } else {
          const match = options.find((o) => o.dataset.value === current);
          nextKey = match ? match.dataset.key : options[0]?.dataset.key || 'default';
        }
      }
      setActive(nextKey, false);
      renderCurrent();
      return firstMoreKey;
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
    const offDevices = on('devices', () => {
      if (!done) render();
    });

    render();
    document.body.append(dialog);
    dialog.showModal();
    search.focus();
    requestAnimationFrame(() => {
      const el = options.find((o) => o.dataset.key === activeKey);
      el?.scrollIntoView({ block: 'nearest' });
    });
    refreshProviders();
  });
}
