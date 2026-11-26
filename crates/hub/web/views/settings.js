// Settings: who is signed in, active browser sessions, the hub, provider keys
// (write-only), appearance, and signing out.

import { h, icon, setChildren } from '../lib/dom.js';
import { get, put, del, enc, describe, isAbort } from '../lib/api.js';
import { store, on, deviceName, isAdmin } from '../lib/store.js';
import { stopLive, loadDevices } from '../lib/live.js';
import { bar, sheet, confirmAction, toast, toastError, skeleton, errorBanner, guardWrites } from '../lib/ui.js';
import { ago, date, dateTime } from '../lib/format.js';

const THEME_KEY = 'grouper.theme';
const PROVIDER_KINDS = [
  ['anthropic', 'Anthropic'],
  ['openai', 'OpenAI-compatible'],
  ['scripted', 'Scripted (for tests)'],
];

export function renderSettings(container) {
  const admin = isAdmin();
  const body = h('div', { class: 'view-body narrow' });
  container.append(h('section', { class: 'view' }, bar({ title: 'Settings' }), body));

  const aborter = new AbortController();
  const { signal } = aborter;
  const unguard = guardWrites(body);

  const me = store.me;
  const account = h(
    'section',
    { class: 'section', 'aria-labelledby': 's-account' },
    h('div', { class: 'section-head' }, h('h2', { id: 's-account' }, 'This browser')),
    h(
      'div',
      { class: 'card card-pad' },
      h('div', { class: 'row-title' }, me.name || me.device_id),
      h('div', { class: 'meta' }, h('span', { class: 'mono' }, me.device_id), h('span', { class: ['badge', me.admin ? 'accent' : 'warn'] }, me.admin ? 'Admin token' : 'Device token: read only')),
      h('p', { class: 'hint' }, 'Signed in with a session cookie that lasts 30 days. The token itself isn’t stored in this browser; the hub accepts it until you revoke it there (grouper-hub device revoke).'),
      h('div', { class: 'section-actions' }, h('button', { class: 'btn', type: 'button', onclick: signOut }, icon('logout'), 'Sign out')),
    ),
  );

  const sessionsSlot = h('div', null, skeleton(2));
  const sessions = h(
    'section',
    { class: 'section', 'aria-labelledby': 's-sessions' },
    h('div', { class: 'section-head' }, h('h2', { id: 's-sessions' }, 'Browser sessions')),
    h('p', { class: 'section-note' }, 'Browsers currently signed in to this hub. Revoking one signs it out and closes its live connection immediately.'),
    sessionsSlot,
  );

  const hubSlot = h('div', { class: 'card card-pad' }, skeleton(2));
  const hub = h('section', { class: 'section', 'aria-labelledby': 's-hub' }, h('div', { class: 'section-head' }, h('h2', { id: 's-hub' }, 'Hub')), hubSlot);

  const providersSlot = h('div', null, skeleton(2));
  const addProviderBtn = admin
    ? h(
        'button',
        {
          class: 'btn btn-sm btn-primary',
          type: 'button',
          'data-write': true,
          onclick: () => openProviderSheet(null),
        },
        icon('plus'),
        'Add provider',
      )
    : null;
  const providers = h(
    'section',
    { class: 'section', 'aria-labelledby': 's-providers' },
    h('div', { class: 'section-head' }, h('h2', { id: 's-providers' }, 'Provider keys'), addProviderBtn),
    h('p', { class: 'section-note' }, 'Keys belong to the account, are encrypted on the hub and sync to your devices. Keys are write-only and never shown again.'),
    providersSlot,
  );

  let theme = 'system';
  try {
    theme = localStorage.getItem(THEME_KEY) || 'system';
  } catch {
    /* storage unavailable */
  }
  const appearance = h(
    'section',
    { class: 'section', 'aria-labelledby': 's-appearance' },
    h('div', { class: 'section-head' }, h('h2', { id: 's-appearance' }, 'Appearance')),
    h(
      'div',
      { class: 'card card-pad' },
      h(
        'div',
        { class: 'seg', role: 'radiogroup', 'aria-label': 'Theme' },
        [
          ['system', 'System'],
          ['light', 'Light'],
          ['dark', 'Dark'],
        ].map(([v, label]) =>
          h(
            'label',
            null,
            h('input', {
              type: 'radio',
              name: 'theme',
              value: v,
              checked: theme === v,
              onchange: () => {
                try {
                  if (v === 'system') localStorage.removeItem(THEME_KEY);
                  else localStorage.setItem(THEME_KEY, v);
                } catch {
                  /* storage unavailable */
                }
                if (v === 'system') document.documentElement.removeAttribute('data-theme');
                else document.documentElement.dataset.theme = v;
              },
            }),
            label,
          ),
        ),
      ),
      h('p', { class: 'hint' }, 'Reduced motion and reduced transparency follow your system settings.'),
    ),
  );

  body.append(account, sessions, providers, hub, appearance);

  // ---- hub info
  let hubInfo = store.hub;
  let health = 'checking';

  function renderHub() {
    if (!hubInfo) return;
    setChildren(
      hubSlot,
      h('div', { class: 'row-title' }, location.host),
      h(
        'div',
        { class: 'meta' },
        h('span', null, `Version ${hubInfo.hub_version}`),
        h('span', null, `Protocol ${hubInfo.protocol_version}`),
        h('span', null, health === 'healthy' ? 'Healthy' : `Health check: ${health}`),
        h('span', null, store.online ? 'Live events connected' : 'Live events offline'),
      ),
      h('p', { class: 'hint mono' }, `Hub id ${hubInfo.hub_id}`),
    );
  }

  async function loadHub() {
    health = 'unreachable';
    try {
      const r = await fetch('/healthz', { cache: 'no-store', signal });
      health = r.ok ? 'healthy' : `HTTP ${r.status}`;
    } catch (err) {
      if (isAbort(err)) return;
    }
    try {
      hubInfo = store.hub || (await get('/v1/hub', { signal }));
      if (!signal.aborted) renderHub();
    } catch (err) {
      if (isAbort(err)) return;
      setChildren(hubSlot, errorBanner(err, loadHub));
    }
  }

  // ---- browser sessions
  async function loadSessions() {
    if (!admin) {
      setChildren(sessionsSlot, h('p', { class: 'hint' }, 'Browser sessions need an admin token.'));
      return;
    }
    setChildren(sessionsSlot, skeleton(2));
    try {
      const list = (await get('/v1/sessions', { signal })) || [];
      if (signal.aborted) return;
      if (!list.length) {
        setChildren(sessionsSlot, h('div', { class: 'card card-pad' }, h('p', { class: 'hint' }, 'No active browser sessions.')));
        return;
      }
      setChildren(
        sessionsSlot,
        h(
          'ul',
          { class: 'list card' },
          list.map((s) =>
            h(
              'li',
              { class: 'list-row' },
              h(
                'div',
                { class: 'grow' },
                h(
                  'div',
                  { class: 'row-title row-badges' },
                  s.device_name || s.device_id,
                  s.current ? h('span', { class: 'badge accent' }, 'This browser') : null,
                  h('span', { class: ['badge', s.admin ? null : 'muted'] }, s.admin ? 'Admin' : 'Device'),
                ),
                h(
                  'div',
                  { class: 'meta' },
                  h('span', null, `Created ${dateTime(s.created)}`),
                  h('span', null, `Last used ${ago(s.last_used)}`),
                  h('span', null, `Expires ${date(s.expires)}`),
                ),
              ),
              h(
                'button',
                {
                  class: 'btn btn-sm',
                  type: 'button',
                  'data-write': true,
                  'aria-label': s.current ? `Revoke current session for ${s.device_name || s.device_id}` : `Revoke session for ${s.device_name || s.device_id}`,
                  onclick: () => revokeSession(s),
                },
                'Revoke',
              ),
            ),
          ),
        ),
      );
    } catch (err) {
      if (isAbort(err)) return;
      setChildren(sessionsSlot, errorBanner(err, loadSessions));
    }
  }

  async function revokeSession(s) {
    const ok = await confirmAction({
      title: s.current ? 'Revoke this browser’s session?' : `Revoke session for ${s.device_name || s.device_id}?`,
      body: s.current
        ? 'You’ll be signed out of this browser immediately and need a token to sign in again.'
        : 'That browser will be signed out and its live connection closed immediately. Its device token stays valid.',
      confirmLabel: 'Revoke',
      danger: true,
    });
    if (!ok) return;
    try {
      await del(`/v1/sessions/${enc(s.id)}`);
    } catch (err) {
      toastError(err);
      return;
    }
    if (s.current) {
      finishSignOut();
      return;
    }
    toast('Session revoked.');
    loadSessions();
  }

  // ---- providers
  let providerList = null;

  async function loadProviders() {
    if (!admin) {
      setChildren(providersSlot, h('p', { class: 'hint' }, 'Provider keys need an admin token.'));
      return;
    }
    setChildren(providersSlot, skeleton(2));
    try {
      if (!store.devicesAt) await loadDevices().catch(() => {});
      providerList = (await get('/v1/providers', { signal })) || [];
      if (signal.aborted) return;
      renderProviders();
    } catch (err) {
      if (isAbort(err)) return;
      setChildren(providersSlot, errorBanner(err, loadProviders));
    }
  }

  function resolveViaDeviceLabel(via) {
    if (!via) return null;
    const match = store.devices.find((d) => d.id === via || d.name.toLowerCase() === via.toLowerCase());
    return match ? `${match.name} (${match.online ? 'online' : 'offline'})` : deviceName(via);
  }

  function renderProviders() {
    if (!providerList) return;
    if (!providerList.length) {
      setChildren(providersSlot, h('div', { class: 'card card-pad' }, h('p', { class: 'hint' }, 'No providers yet. Add a provider above to configure API keys or a device-hosted model server.')));
      return;
    }
    setChildren(
      providersSlot,
      h(
        'ul',
        { class: 'list card' },
        providerList.map((p) => {
          const viaLabel = resolveViaDeviceLabel(p.via_device);
          return h(
            'li',
            { class: 'list-row' },
            h(
              'div',
              { class: 'grow' },
              h(
                'div',
                { class: 'row-title row-badges' },
                p.id,
                h('span', { class: ['badge', p.has_key ? 'accent' : 'muted'] }, p.has_key ? 'Key stored' : 'No key'),
              ),
              h(
                'div',
                { class: 'meta' },
                h('span', null, p.kind === 'scripted' ? 'scripted (for tests)' : p.kind),
                p.base_url ? h('span', { class: 'mono' }, p.base_url) : null,
                viaLabel ? h('span', null, `via ${viaLabel}`) : null,
              ),
            ),
            h(
              'button',
              {
                class: 'btn btn-sm',
                type: 'button',
                'data-write': true,
                'aria-label': `Edit provider ${p.id}`,
                onclick: () => openProviderSheet(p),
              },
              icon('edit'),
              'Edit',
            ),
          );
        }),
      ),
    );
  }

  function openProviderSheet(existing) {
    const isEdit = Boolean(existing);
    const devices = store.devices.filter((d) => d.id !== 'hub');
    const matchedDevice = existing?.via_device
      ? devices.find((d) => d.id === existing.via_device || d.name.toLowerCase() === existing.via_device.toLowerCase())
      : null;
    const initialVia = matchedDevice ? matchedDevice.id : existing?.via_device || '';

    // Strip `via_device` from the JSON box since the picker controls it.
    let initialOptions = '';
    if (existing?.options && typeof existing.options === 'object' && !Array.isArray(existing.options)) {
      const rest = { ...existing.options };
      delete rest.via_device;
      if (Object.keys(rest).length) initialOptions = JSON.stringify(rest, null, 2);
    }

    sheet((close) => {
      const errBox = h('div', { class: 'banner error', role: 'alert', hidden: true });
      const idInput = h('input', {
        class: 'input mono',
        name: 'id',
        required: true,
        readonly: isEdit,
        placeholder: 'openrouter',
        value: existing?.id || '',
      });
      const kindSelect = h(
        'select',
        { class: 'select', name: 'kind', required: true },
        PROVIDER_KINDS.map(([val, label]) => h('option', { value: val, selected: (existing?.kind || 'openai') === val }, label)),
      );
      const baseUrlInput = h('input', {
        class: 'input mono',
        name: 'base_url',
        type: 'url',
        placeholder: 'https://openrouter.ai/api/v1',
        value: existing?.base_url || '',
      });
      const keyInput = h('input', {
        class: 'input mono',
        name: 'api_key',
        type: 'password',
        autocomplete: 'new-password',
        placeholder: existing?.has_key ? 'Stored — leave blank to keep current key' : 'Enter API key (optional for local / scripted)',
        value: '',
      });
      const viaOptions = [
        h('option', { value: '' }, 'None (hub / direct)'),
        ...devices.map((d) => h('option', { value: d.id, selected: initialVia === d.id }, `${d.name} (${d.online ? 'online' : 'offline'})`)),
      ];
      if (initialVia && !matchedDevice) {
        viaOptions.push(h('option', { value: initialVia, selected: true }, initialVia));
      }
      const viaSelect = h('select', { class: 'select', name: 'via_device' }, viaOptions);
      const optionsInput = h('textarea', {
        class: 'textarea mono',
        name: 'options',
        rows: 4,
        placeholder: '{}',
        value: initialOptions,
      });
      const saveBtn = h('button', { class: 'btn btn-primary', type: 'submit', form: 'provider-form' }, isEdit ? 'Save provider' : 'Add provider');

      const form = h(
        'form',
        {
          id: 'provider-form',
          class: 'stack',
          onsubmit: async (e) => {
            e.preventDefault();
            errBox.hidden = true;
            const id = isEdit ? existing.id : idInput.value.trim();
            if (!id || /[\s/]/.test(id)) {
              errBox.textContent = 'Provider id must be non-empty with no spaces or slashes.';
              errBox.hidden = false;
              idInput.focus();
              return;
            }
            let opts = {};
            const rawOpts = optionsInput.value.trim();
            if (rawOpts) {
              try {
                const parsed = JSON.parse(rawOpts);
                if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) {
                  throw new Error('Options must be a JSON object.');
                }
                opts = parsed;
              } catch (parseErr) {
                errBox.textContent = `Options JSON: ${parseErr.message}`;
                errBox.hidden = false;
                optionsInput.focus();
                return;
              }
            }
            const via = viaSelect.value.trim();
            if (via) opts.via_device = via;
            else delete opts.via_device;

            const baseUrl = baseUrlInput.value.trim() || null;
            const apiKey = keyInput.value.trim() || null;
            saveBtn.disabled = true;
            try {
              await put(`/v1/providers/${enc(id)}`, {
                id,
                kind: kindSelect.value,
                base_url: baseUrl,
                api_key: apiKey,
                options: opts,
              });
              keyInput.value = '';
              toast(isEdit ? `Updated provider ${id}.` : `Added provider ${id}.`);
              close(true);
              loadProviders();
            } catch (err) {
              errBox.textContent = describe(err);
              errBox.hidden = false;
              saveBtn.disabled = false;
            }
          },
        },
        errBox,
        h(
          'div',
          { class: 'form-grid' },
          h('label', { class: 'field' }, h('span', null, 'Provider id'), idInput),
          h('label', { class: 'field' }, h('span', null, 'Kind'), kindSelect),
        ),
        h('label', { class: 'field' }, h('span', null, 'Base URL (optional)'), baseUrlInput, h('p', { class: 'hint' }, 'For OpenAI-compatible endpoints (e.g. https://openrouter.ai/api/v1) or a loopback URL on a hosting device.')),
        h('label', { class: 'field' }, h('span', null, 'API key'), keyInput, h('p', { class: 'hint' }, existing?.has_key ? 'Write-only: leave blank to keep the stored key, or enter a new key to replace it.' : 'Write-only: encrypted at rest on the hub and never displayed again.')),
        h('label', { class: 'field' }, h('span', null, 'Host via device (optional)'), viaSelect, h('p', { class: 'hint' }, 'Relay calls through one of your devices so a local server (e.g. LM Studio, Ollama) stays private.')),
        h('label', { class: 'field' }, h('span', null, 'Options JSON (optional)'), optionsInput, h('p', { class: 'hint' }, 'Optional JSON object, e.g. {"max_tokens_field": "max_completion_tokens"}.')),
      );

      return {
        title: isEdit ? `Edit provider ${existing.id}` : 'Add provider',
        caption: isEdit ? 'Update connection details or rotate the stored key' : 'Configure an AI provider for this hub',
        body: [form],
        foot: [
          h('button', { class: 'btn', type: 'button', onclick: () => close(false) }, 'Cancel'),
          saveBtn,
        ],
        onOpen: () => idInput.focus(),
      };
    });
  }

  function finishSignOut() {
    stopLive();
    try {
      localStorage.removeItem('grouper.lastConversation');
    } catch {
      /* storage unavailable */
    }
    location.hash = '';
    location.reload();
  }

  async function signOut() {
    if (!(await confirmAction({ title: 'Sign out of this browser?', body: 'You’ll need a hub token to sign in again.', confirmLabel: 'Sign out' }))) return;
    try {
      await del('/v1/session');
    } catch (err) {
      toastError(err);
      return;
    }
    finishSignOut();
  }

  const offs = [
    on('online', () => {
      renderHub();
    }),
    on('devices', () => {
      renderProviders();
    }),
  ];

  loadHub();
  loadSessions();
  loadProviders();

  return () => {
    aborter.abort();
    unguard();
    offs.forEach((off) => off());
  };
}
