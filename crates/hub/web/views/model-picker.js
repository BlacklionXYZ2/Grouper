// The chat's model picker: a covering panel (a full-height sheet on phones, a
// large centred overlay with a scrim on desktop), because there are many models.
//
// A modal <dialog> holding a combobox search field and one listbox. Focus stays
// in the search field: typing filters, arrow keys move the active option
// (aria-activedescendant), Enter picks it, Escape closes and focus returns to the
// button that opened it.
//
// Sections: "Default model" at the top, then API sections — "Suggested" (role
// defaults), "Local" (models marked local, via_device, or using a local provider
// URL) — then "Harnesses (subscriptions)" grouped by device, then "More models…"
// which expands all API models grouped under their provider headers (id, kind,
// key status, via_device). Each provider header is a disclosure button with a
// model count; groups above 12 models start collapsed (the chosen model's group
// never does), the choice is remembered per provider, and groups above 50 show
// the first 50 plus "Show all N". Headers take part in arrow-key navigation;
// Enter on one toggles it. Typing to filter ignores collapse state and shows
// every matching row. A model reached through two providers appears under each
// provider. API models and harnesses stay clearly separate.

import { h, icon, setChildren, setText, setAttr } from '../lib/dom.js';
import { get, isAbort } from '../lib/api.js';
import { store, on, isAdmin, deviceName, findDevice } from '../lib/store.js';
import { familyKey, familyMark, familyInfo } from '../lib/family.js';

let providersCache = null; // the last GET /v1/providers, shared between openings

// Provider groups under "More models" collapse, because a catalogue such as
// OpenRouter's has hundreds of models. A group is collapsed by default above
// COLLAPSE_ABOVE models; the owner's choice per provider is remembered.
const COLLAPSE_ABOVE = 12;
const SHOW_FIRST = 50;
const GROUPS_KEY = 'grouper.mp.groups';

function loadGroupPrefs() {
  try {
    const raw = JSON.parse(localStorage.getItem(GROUPS_KEY) || '{}');
    return raw && typeof raw === 'object' && !Array.isArray(raw) ? raw : {};
  } catch {
    return {};
  }
}

function saveGroupPref(pid, expanded) {
  try {
    const prefs = loadGroupPrefs();
    prefs[pid] = Boolean(expanded);
    localStorage.setItem(GROUPS_KEY, JSON.stringify(prefs));
  } catch {
    // storage unavailable: the choice lasts for this opening only
  }
}

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

const HARNESS_NAMES = {
  'claude-code': 'Claude Code',
  codex: 'Codex',
  antigravity: 'Antigravity',
  grok: 'Grok',
  hermes: 'Hermes',
  pi: 'pi',
  opencode: 'OpenCode',
  cursor: 'Cursor',
};

export function isHarnessModel(m, provider) {
  return Boolean(m?.kind === 'harness' || provider?.kind === 'harness');
}

export function harnessDeviceId(m, provider) {
  if (m?.device_id) return m.device_id;
  if (provider?.via_device) return provider.via_device;
  const pid = String(m?.provider_id || provider?.id || '');
  const at = pid.indexOf('@');
  return at >= 0 ? pid.slice(at + 1) : '';
}

export function harnessCliName(m, provider) {
  const optName = typeof provider?.options?.display_name === 'string' ? provider.options.display_name.trim() : '';
  if (optName) return optName;
  const optSlug = typeof provider?.options?.harness === 'string' ? provider.options.harness.trim() : '';
  const pid = String(m?.provider_id || provider?.id || '');
  const pidSlug = pid.includes('@') ? pid.slice(0, pid.indexOf('@')) : '';
  const slug = optSlug || pidSlug;
  return HARNESS_NAMES[slug] || slug || 'Harness';
}

export function harnessSignInInfo(provider) {
  const opts = provider?.options;
  if (!opts || typeof opts !== 'object') return null;
  if (opts.installed === false) return { label: 'Not installed', tone: 'muted' };
  if (opts.sign_in === 'expired') return { label: 'Expired', tone: 'warn' };
  if (opts.sign_in === 'signed_in' || opts.signed_in === true) return { label: 'Signed in', tone: 'ok' };
  if (opts.sign_in === 'not_signed_in' || opts.signed_in === false) return { label: 'Not signed in', tone: 'muted' };
  return null;
}

export function isLocalModel(m, provider) {
  if (isHarnessModel(m, provider)) return false;
  return Boolean(m.local || provider?.via_device || isLocalUrl(provider?.base_url));
}

/** At most one right-aligned badge: Harness, Local (with host device if remote-hosted) or Free. */
export function modelBadge(m, provider, { right = false } = {}) {
  if (isHarnessModel(m, provider)) {
    return h('span', { class: ['badge', 'accent', right && 'mp-badge'] }, 'Harness');
  }
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
    let options = []; // visible navigation items (options, group headers, "Show all" rows), in order
    const groupPrefs = loadGroupPrefs();
    const touched = new Set(); // providers toggled during this opening (their choice beats the defaults)
    const showAll = new Set(); // providers whose "Show all N" row was used
    const withCurrent = new Set(store.models.filter((m) => current && m.id === current).map((m) => m.provider_id));

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
        if (el?.dataset.action) activateNav(el);
        else if (el && el.getAttribute('aria-disabled') !== 'true') close(el.dataset.value);
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

    function isExpanded(pid, count) {
      if (touched.has(pid)) return Boolean(groupPrefs[pid]);
      if (withCurrent.has(pid)) return true; // the chosen model's provider opens expanded
      if (typeof groupPrefs[pid] === 'boolean') return groupPrefs[pid];
      return count <= COLLAPSE_ABOVE;
    }

    function activateNav(el) {
      const pid = el.dataset.pid;
      if (el.dataset.action === 'toggle') {
        const count = Number(el.dataset.count) || 0;
        const next = !isExpanded(pid, count);
        groupPrefs[pid] = next;
        touched.add(pid);
        saveGroupPref(pid, next);
      } else if (el.dataset.action === 'showall') {
        showAll.add(pid);
      }
      activeKey = el.dataset.key;
      render();
      search.focus({ preventScroll: true });
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
      const harness = isHarnessModel(m, provider);
      const devId = harness ? harnessDeviceId(m, provider) : '';
      const dev = devId ? findDevice(devId) : null;
      const host = harness
        ? (devId ? deviceName(devId) : '')
        : (provider?.via_device ? deviceName(provider.via_device) : '');
      const local = isLocalModel(m, provider);
      const hay = [
        m.id,
        m.model,
        m.display_name || '',
        m.provider_id,
        provider?.kind || '',
        harness ? `${harnessCliName(m, provider)} harness subscription` : '',
        harness && m.can_orchestrate ? 'orchestrates mcp' : '',
        dev ? (dev.online ? 'online' : 'offline') : '',
        host,
        m.roles.join(' '),
        fam.key === 'unknown' ? '' : `${fam.label} ${fam.maker}`,
        harness ? '' : (m.free ? 'free' : 'paid'),
        local ? 'local' : '',
      ]
        .join(' ')
        .toLowerCase();
      return words.every((w) => hay.includes(w));
    }

    function optionEl(value, key, idx, content, selected, { disabled = false } = {}) {
      return h(
        'div',
        {
          id: `mp-opt-${idx}`,
          class: ['mp-option', selected && 'selected', disabled && 'is-disabled'],
          role: 'option',
          'aria-selected': selected ? 'true' : 'false',
          'aria-disabled': disabled ? 'true' : null,
          dataset: { value, key },
          onclick: disabled ? undefined : () => close(value),
        },
        content,
        selected ? h('span', { class: 'mp-check', 'aria-hidden': 'true' }, icon('check')) : null,
      );
    }

    function harnessOption(m, idx, keyPrefix, provider) {
      const famKey = familyKey(m);
      const cliName = harnessCliName(m, provider);
      const devId = harnessDeviceId(m, provider);
      const dev = devId ? findDevice(devId) : null;
      const devName = devId ? deviceName(devId) : 'unknown device';
      const canOrch = Boolean(m.can_orchestrate);
      const disabled = role === 'orchestrator' && !canOrch;
      const badge = modelBadge(m, provider, { right: true });
      const subItems = [
        h('span', { class: 'mp-opt-provider' }, `${cliName} on ${devName}`),
        dev ? h('span', null, dev.online ? 'online' : 'offline') : null,
        h('span', { class: 'mp-price' }, 'Subscription'),
        role === 'orchestrator'
          ? h('span', { class: 'mp-opt-note' }, canOrch ? 'Orchestrates via MCP' : 'Standard chats and threads only')
          : m.roles.includes(role)
            ? h('span', null, `${role} default`)
            : null,
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
              h('span', { class: 'mp-opt-id' }, `${cliName} · ${m.model}`),
              badge,
            ),
            h('div', { class: 'meta mp-opt-meta mp-opt-sub' }, subItems),
          ),
        ),
        current === m.id,
        { disabled },
      );
    }

    function modelOption(m, idx, keyPrefix, provider, { roleNote = null } = {}) {
      const famKey = familyKey(m);
      const isLocal = isLocalModel(m, provider);
      // One right-aligned badge per row: the role default when there is one,
      // otherwise Local/Free. When the role note takes the slot, Local/Free
      // moves to line two as plain text.
      const kindBadge = modelBadge(m, provider, { right: true });
      const badge = roleNote ? h('span', { class: ['badge', 'mp-badge'] }, roleNote) : kindBadge;
      const pText = priceText(m, isLocal);
      const subItems = [
        h('span', { class: 'mp-opt-provider' }, m.provider_id || 'Unknown provider'),
        pText ? h('span', { class: 'mp-price' }, pText) : null,
        roleNote && kindBadge && pText !== 'Free' ? h('span', null, kindBadge.textContent) : null,
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
      const apiShown = shown.filter((m) => !isHarnessModel(m, providers.get(m.provider_id)));
      const harnessShown = shown.filter((m) => isHarnessModel(m, providers.get(m.provider_id)));

      // Suggested: API role default(s) first, then any other API role defaults.
      // ("Last used" model history isn't stored by the hub or client.)
      const byIdThenProvider = (a, b) => a.id.localeCompare(b.id) || a.provider_id.localeCompare(b.provider_id);
      const primaryRole = apiShown.filter((m) => m.roles.includes(role)).sort(byIdThenProvider);
      const otherRoles = apiShown.filter((m) => !m.roles.includes(role) && m.roles.length > 0).sort(byIdThenProvider);
      const suggested = [...primaryRole, ...otherRoles];
      const localModels = apiShown.filter((m) => isLocalModel(m, providers.get(m.provider_id))).sort(byIdThenProvider);

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

      if (harnessShown.length) {
        const byDevice = new Map();
        for (const m of harnessShown) {
          const devId = harnessDeviceId(m, providers.get(m.provider_id));
          if (!byDevice.has(devId)) byDevice.set(devId, []);
          byDevice.get(devId).push(m);
        }
        const deviceIds = [...byDevice.keys()].sort((a, b) => deviceName(a).localeCompare(deviceName(b)) || a.localeCompare(b));
        parts.push(
          h(
            'div',
            { class: 'mp-group', role: 'group', 'aria-label': 'Harnesses (subscriptions)' },
            h('div', { class: 'mp-section', 'aria-hidden': 'true' }, 'Harnesses (subscriptions)'),
            deviceIds.map((devId, g) => {
              const dev = devId ? findDevice(devId) : null;
              const label = devId ? deviceName(devId) : 'Unknown device';
              const models = byDevice.get(devId).sort((a, b) => {
                const ca = harnessCliName(a, providers.get(a.provider_id));
                const cb = harnessCliName(b, providers.get(b.provider_id));
                return ca.localeCompare(cb) || a.model.localeCompare(b.model) || a.id.localeCompare(b.id);
              });
              const headerId = `mp-hd-${g}`;
              return h(
                'div',
                { class: 'mp-group', role: 'group', 'aria-labelledby': headerId },
                h(
                  'div',
                  { class: 'mp-provider', id: headerId },
                  h('span', { class: 'mp-provider-id' }, label),
                  dev ? h('span', { class: ['badge', dev.online ? 'ok' : 'muted'] }, dev.online ? 'Online' : 'Offline') : null,
                ),
                models.map((m) => harnessOption(m, idx++, `harness:${devId}`, providers.get(m.provider_id))),
              );
            }),
          ),
        );
      }

      const byProvider = new Map();
      for (const m of apiShown) {
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
          const rowsId = `mp-rows-${g}`;
          const total = models.length;
          const expanded = filtering || isExpanded(pid, total);
          const capped = expanded && !filtering && total > SHOW_FIRST && !showAll.has(pid);
          const header = h(
            'button',
            {
              type: 'button',
              id: `mp-opt-${idx++}`,
              class: ['mp-provider', 'mp-nav', expanded && 'is-open'],
              'aria-expanded': expanded ? 'true' : 'false',
              'aria-controls': rowsId,
              tabindex: '-1',
              dataset: { key: `more:hdr:${pid}`, action: 'toggle', pid, count: String(total) },
              onclick: (e) => activateNav(e.currentTarget),
            },
            icon(expanded ? 'down' : 'chevron'),
            h('span', { class: 'mp-provider-id', id: labelId }, `${pid || 'Unknown provider'} · ${total}`),
            p?.kind ? h('span', { class: 'badge' }, p.kind) : null,
            p ? h('span', { class: ['badge', p.has_key ? 'ok' : 'muted'] }, p.has_key ? 'Key set' : 'No key') : null,
            p?.via_device ? h('span', { class: 'badge muted' }, `via ${deviceName(p.via_device)}`) : null,
            providersKnown && !p ? h('span', { class: 'badge warn' }, 'Unknown provider') : null,
          );
          if (!firstMoreKey) firstMoreKey = header.dataset.key;
          const rows = [];
          if (expanded) {
            for (const m of capped ? models.slice(0, SHOW_FIRST) : models) rows.push(modelOption(m, idx++, `more:${pid}`, p));
            if (capped) {
              rows.push(
                h(
                  'button',
                  {
                    type: 'button',
                    id: `mp-opt-${idx++}`,
                    class: 'mp-showall mp-nav',
                    tabindex: '-1',
                    dataset: { key: `more:all:${pid}`, action: 'showall', pid },
                    onclick: (e) => activateNav(e.currentTarget),
                  },
                  `Show all ${total}`,
                ),
              );
            }
          }
          parts.push(
            h(
              'div',
              { class: 'mp-group', role: 'group', 'aria-labelledby': labelId },
              header,
              h('div', { class: 'mp-rows', id: rowsId, role: 'none' }, rows),
            ),
          );
        });
      }

      setChildren(list, parts);
      options = [...list.querySelectorAll('[role="option"], .mp-nav')];

      if (!filtering && apiShown.length) {
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
          // Typing puts the first enabled match under the cursor.
          const firstEnabled = options.find((o, i) => i > 0 && !o.dataset.action && o.getAttribute('aria-disabled') !== 'true');
          nextKey = firstEnabled ? firstEnabled.dataset.key : options[0].dataset.key;
        } else {
          const match = options.find((o) => !o.dataset.action && o.dataset.value === current);
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
