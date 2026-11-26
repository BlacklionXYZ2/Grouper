// Settings: who is signed in, active browser sessions, the hub, provider keys
// (write-only), appearance, approval notifications for this browser, and signing
// out. Signing out (or revoking this browser's session) first removes this
// browser's push subscription from the hub and unsubscribes, best effort.

import { h, icon, setChildren } from '../lib/dom.js';
import { get, put, del, enc, describe, isAbort } from '../lib/api.js';
import { store, on, deviceName, isAdmin } from '../lib/store.js';
import { stopLive, loadDevices } from '../lib/live.js';
import { bar, sheet, confirmAction, toast, toastError, skeleton, errorBanner, guardWrites } from '../lib/ui.js';
import { ago, date, dateTime } from '../lib/format.js';
import {
  support as notifySupport,
  permission as notifyPermission,
  optedIn,
  currentSubscription,
  enableNotifications,
  disableNotifications,
  forgetNotifications,
  listSubscriptions,
  removeSubscription,
  sendTest,
  showLocalTest,
  storedSubscriptionId,
  setHasPush,
} from '../lib/push.js';

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

  const notifySlot = h('div', { class: 'card card-pad notify-card' }, skeleton(2));
  const notifyListSlot = h('div');
  const notifications = h(
    'section',
    { class: 'section', 'aria-labelledby': 's-notify' },
    h('div', { class: 'section-head' }, h('h2', { id: 's-notify' }, 'Notifications')),
    h(
      'p',
      { class: 'section-note' },
      'Grouper can notify this browser when a tool needs your review. Notifications show tool/device/chat names, never the command or its contents. You must open Grouper to inspect and approve.',
    ),
    notifySlot,
    notifyListSlot,
  );

  body.append(account, sessions, providers, hub, appearance, notifications);

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

  // ---- approval notifications (this browser's opt-in; spec 5.11, 5.14)
  let notifyMessage = ''; // the outcome of the last action, in words
  let notifyBusy = false;
  let hubPush = 'unknown'; // 'ok' | 'missing' (no push API on this hub) | 'forbidden'

  async function renderNotify() {
    const kind = notifySupport();
    const perm = notifyPermission();
    const sub = kind === 'push' ? await currentSubscription() : null;
    if (signal.aborted) return;
    const on = optedIn() && perm === 'granted' && (kind === 'page-only' || Boolean(sub));
    let state;
    let detail = null;
    const buttons = [];
    const btn = (label, iconName, onclick, primary = false) =>
      h('button', { class: ['btn', primary && 'btn-primary'], type: 'button', 'data-write': true, disabled: notifyBusy, onclick }, iconName ? icon(iconName) : null, label);

    if (!admin) {
      state = 'Not available in this session';
      detail = 'This session uses a device token, which can’t answer approvals, so it can’t receive their notifications. Sign in with an admin token to turn them on.';
    } else if (kind === 'ios-home-screen') {
      state = 'Add Grouper to your Home Screen to enable web notifications';
      detail = 'In Safari, tap Share, then Add to Home Screen, and open Grouper from the new icon. Settings there will offer notifications.';
    } else if (kind === 'insecure') {
      state = 'Needs the hub’s HTTPS address';
      detail = 'Browsers only allow notifications on secure pages. Open Grouper over https:// (or on localhost) to turn them on.';
    } else if (kind === 'unsupported') {
      state = 'Not supported in this browser';
      detail = 'This browser can’t show web notifications. You can still find requests in Needs you and in each chat.';
    } else if (hubPush === 'missing' && kind === 'push') {
      state = 'Not available on this hub';
      detail = 'This hub doesn’t offer web notifications yet. Update the hub to turn them on.';
    } else if (hubPush === 'forbidden') {
      state = 'Not available in this session';
      detail = 'The hub refused: this session can’t manage notifications. Sign in with an admin token.';
    } else if (perm === 'denied') {
      state = 'Blocked in this browser';
      detail = `Notifications for ${location.host} are blocked in your browser’s settings, so Grouper won’t ask again. To allow them, open this site’s settings (usually the icon beside the address), allow notifications, then reload this page.`;
      if (sub || optedIn()) buttons.push(btn('Turn off for this browser', null, () => runNotify(turnOff)));
    } else if (on) {
      state = kind === 'page-only' ? 'On while Grouper is open in this browser' : 'On for this browser';
      detail =
        kind === 'page-only'
          ? 'This browser has no background push, so alerts show only while a Grouper tab is open.'
          : 'Each browser opts in on its own. If Grouper is open on the request’s chat, the dock shows it instead.';
      buttons.push(btn('Send a test notification', 'play', () => runNotify(test)), btn('Turn off for this browser', null, () => runNotify(turnOff)));
    } else {
      state = 'Off for this browser';
      detail = perm === 'granted' ? 'The browser already allows notifications; turning them on won’t ask again.' : 'Your browser will ask for permission once, when you press the button.';
      buttons.push(btn('Enable approval notifications', 'shield', () => runNotify(turnOn), true));
    }

    setChildren(
      notifySlot,
      h('div', { class: 'row-title row-badges' }, state, admin ? h('span', { class: ['badge', on ? 'accent' : 'muted'] }, on ? 'On' : 'Off') : null),
      detail ? h('p', { class: 'hint' }, detail) : null,
      admin && kind !== 'unsupported' && kind !== 'ios-home-screen' ? h('p', { class: 'hint' }, `Browser permission: ${perm === 'granted' ? 'allowed' : perm === 'denied' ? 'blocked' : 'not asked yet'}.`) : null,
      buttons.length ? h('div', { class: 'section-actions' }, buttons) : null,
      h('p', { class: 'hint notify-result', role: 'status' }, notifyMessage),
    );
  }

  async function runNotify(action) {
    if (notifyBusy) return;
    notifyBusy = true;
    notifyMessage = '';
    // No re-render before the action: the browser's permission prompt must come
    // straight from the click (Safari refuses it after an await).
    try {
      notifyMessage = (await action()) || '';
    } catch (err) {
      if (err?.status === 404) hubPush = 'missing';
      if (err?.status === 403) hubPush = 'forbidden';
      notifyMessage = describe(err);
    }
    notifyBusy = false;
    if (signal.aborted) return;
    await renderNotify();
    loadNotifyList();
  }

  async function turnOn() {
    const r = await enableNotifications();
    if (r.permission === 'denied') return 'The browser blocked notifications. Grouper won’t ask again.';
    if (r.permission !== 'granted') return 'Notifications stay off: the browser didn’t get permission.';
    setHasPush(r.mode === 'push');
    hubPush = 'ok';
    return r.mode === 'page' ? 'On. Alerts show while Grouper is open here.' : 'On. This browser will be notified when a tool needs your review.';
  }

  async function turnOff() {
    try {
      await disableNotifications();
    } finally {
      setHasPush(false);
    }
    return 'Off for this browser.';
  }

  async function test() {
    if (notifySupport() === 'page-only') {
      await showLocalTest();
      return 'Test shown.';
    }
    const results = await sendTest(storedSubscriptionId());
    const list = Array.isArray(results) ? results : Array.isArray(results?.results) ? results.results : null;
    if (!list) return 'Test sent. It should arrive in a few seconds.';
    if (!list.length) return 'The hub has no subscription for this browser. Turn notifications off and on again.';
    const failed = list.filter((x) => x && x.ok === false);
    if (!failed.length) return 'Test sent. It should arrive in a few seconds.';
    return `The push service refused the test: ${failed.map((x) => x.error || x.status || 'failed').join('; ')}`;
  }

  // Every browser with notifications on (no keys or endpoints are shown).
  async function loadNotifyList() {
    if (!admin || hubPush === 'missing' || hubPush === 'forbidden') {
      notifyListSlot.replaceChildren();
      return;
    }
    let rows;
    try {
      rows = (await listSubscriptions({ signal })) || [];
      hubPush = 'ok';
    } catch (err) {
      if (isAbort(err)) return;
      if (err.status === 404) hubPush = 'missing';
      else if (err.status === 403) hubPush = 'forbidden';
      notifyListSlot.replaceChildren();
      if (err.status === 404 || err.status === 403) renderNotify();
      return;
    }
    if (signal.aborted) return;
    if (!rows.length) {
      notifyListSlot.replaceChildren();
      return;
    }
    const mine = storedSubscriptionId();
    setChildren(
      notifyListSlot,
      h('h3', { class: 'sub-title notify-list-title' }, 'Browsers with notifications on'),
      h(
        'ul',
        { class: 'list card' },
        rows.map((row) =>
          h(
            'li',
            { class: 'list-row' },
            h(
              'div',
              { class: 'grow' },
              h('div', { class: 'row-title row-badges' }, row.label || 'Browser', String(row.id) === mine ? h('span', { class: 'badge accent' }, 'This browser') : null),
              h('div', { class: 'meta' }, row.created ? h('span', null, `Added ${dateTime(row.created)}`) : null, h('span', null, row.last_ok ? `Last delivered ${ago(row.last_ok)}` : 'Nothing delivered yet')),
            ),
            h(
              'button',
              {
                class: 'btn btn-sm',
                type: 'button',
                'data-write': true,
                'aria-label': `Remove notifications for ${row.label || 'this browser'}`,
                onclick: () => removeRow(row, String(row.id) === mine),
              },
              'Remove',
            ),
          ),
        ),
      ),
    );
  }

  async function removeRow(row, isMine) {
    if (isMine) {
      runNotify(turnOff);
      return;
    }
    try {
      await removeSubscription(row.id);
      toast(`Notifications removed for ${row.label || 'that browser'}.`);
    } catch (err) {
      toastError(err);
    }
    loadNotifyList();
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
    // This browser's notifications end with its session (while the session can still delete them).
    if (s.current) await forgetNotifications();
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
    // Remove this browser's push subscription first, while the session can still do it.
    await forgetNotifications();
    try {
      await del('/v1/session');
    } catch (err) {
      toastError(err);
      return;
    }
    finishSignOut();
  }

  // The permission can change in the browser's own settings while this page is open.
  const onVisible = () => {
    if (document.visibilityState === 'visible' && !notifyBusy) renderNotify();
  };
  document.addEventListener('visibilitychange', onVisible);

  const offs = [
    () => document.removeEventListener('visibilitychange', onVisible),
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
  renderNotify();
  loadNotifyList();

  return () => {
    aborter.abort();
    unguard();
    offs.forEach((off) => off());
  };
}
