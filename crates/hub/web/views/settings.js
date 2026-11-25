// Settings: who is signed in, the hub, provider keys (status only), appearance,
// and signing out.

import { h, icon, setChildren } from '../lib/dom.js';
import { get, del } from '../lib/api.js';
import { store, isAdmin } from '../lib/store.js';
import { stopLive } from '../lib/live.js';
import { bar, confirmAction, toastError, skeleton } from '../lib/ui.js';

const THEME_KEY = 'grouper.theme';

export function renderSettings(container) {
  const body = h('div', { class: 'view-body narrow' });
  container.append(h('section', { class: 'view' }, bar({ title: 'Settings' }), body));

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
      h('div', null, h('button', { class: 'btn', type: 'button', onclick: signOut }, icon('logout'), 'Sign out')),
    ),
  );

  const hubSlot = h('div', { class: 'card card-pad' }, skeleton(2));
  const hub = h('section', { class: 'section', 'aria-labelledby': 's-hub' }, h('div', { class: 'section-head' }, h('h2', { id: 's-hub' }, 'Hub')), hubSlot);

  const providersSlot = h('div', null, skeleton(2));
  const providers = h(
    'section',
    { class: 'section', 'aria-labelledby': 's-providers' },
    h('div', { class: 'section-head' }, h('h2', { id: 's-providers' }, 'Provider keys')),
    h('p', { class: 'section-note' }, 'Keys belong to the account, are encrypted on the hub and sync to your devices. Set them with grouper keys set <provider>.'),
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

  body.append(account, hub, providers, appearance);

  (async () => {
    let health = 'unreachable';
    try {
      const r = await fetch('/healthz', { cache: 'no-store' });
      health = r.ok ? 'healthy' : `HTTP ${r.status}`;
    } catch {
      /* unreachable */
    }
    try {
      const info = store.hub || (await get('/v1/hub'));
      setChildren(
        hubSlot,
        h('div', { class: 'row-title' }, location.host),
        h('div', { class: 'meta' }, h('span', null, `Version ${info.hub_version}`), h('span', null, `Protocol ${info.protocol_version}`), h('span', null, health === 'healthy' ? 'Healthy' : `Health check: ${health}`), h('span', null, store.online ? 'Live events connected' : 'Live events offline')),
        h('p', { class: 'hint mono' }, `Hub id ${info.hub_id}`),
      );
    } catch (err) {
      setChildren(hubSlot, h('p', null, `Can't read the hub's details: ${err.message}`));
    }
  })();

  (async () => {
    if (!isAdmin()) {
      setChildren(providersSlot, h('p', { class: 'hint' }, 'Provider keys need an admin token.'));
      return;
    }
    try {
      const list = (await get('/v1/providers')) || [];
      setChildren(
        providersSlot,
        list.length
          ? h(
              'ul',
              { class: 'list card' },
              list.map((p) =>
                h(
                  'li',
                  { class: 'list-row' },
                  h('div', { class: 'grow' }, h('div', { class: 'row-title' }, p.id), h('div', { class: 'meta' }, h('span', null, p.kind === 'scripted' ? 'scripted (for tests)' : p.kind), p.base_url ? h('span', { class: 'mono' }, p.base_url) : null)),
                  h('span', { class: ['badge', p.has_key ? 'accent' : 'muted'] }, p.has_key ? 'Stored' : 'Not set'),
                ),
              ),
            )
          : h('p', { class: 'hint' }, 'No providers yet.'),
      );
    } catch (err) {
      setChildren(providersSlot, h('p', { class: 'hint' }, err.message));
    }
  })();

  async function signOut() {
    if (!(await confirmAction({ title: 'Sign out of this browser?', body: 'You’ll need a hub token to sign in again.', confirmLabel: 'Sign out' }))) return;
    try {
      await del('/v1/session');
    } catch (err) {
      toastError(err);
      return;
    }
    stopLive();
    try {
      localStorage.removeItem('grouper.lastConversation');
    } catch {
      /* storage unavailable */
    }
    location.hash = '';
    location.reload();
  }

  return null;
}
