// Sign in: trade a hub token for a session cookie (POST /v1/session).
// The token goes to the hub once and is never kept by the browser.

import { h, icon, srOnly } from '../lib/dom.js';
import { post, describe } from '../lib/api.js';

export function renderLogin(onSignedIn) {
  const input = h('input', {
    class: 'input mono',
    type: 'password',
    id: 'token',
    name: 'token',
    autocomplete: 'current-password',
    spellcheck: 'false',
    autocapitalize: 'off',
    placeholder: 'device-id.secret',
    required: true,
    'aria-describedby': 'token-help',
  });
  // Tokens are long and easy to mistype: let people check what they pasted.
  const revealWord = h('span', null, 'Show');
  const reveal = h(
    'button',
    {
      class: 'btn btn-ghost reveal-btn',
      type: 'button',
      'aria-controls': 'token',
      onclick: () => {
        const show = input.type === 'password';
        input.type = show ? 'text' : 'password';
        revealWord.textContent = show ? 'Hide' : 'Show';
      },
    },
    revealWord,
    srOnly(' token'),
  );
  const error = h('div', { class: 'banner error', role: 'alert', hidden: true });
  const button = h('button', { class: 'btn btn-primary btn-lg', type: 'submit' }, 'Sign in');
  const form = h(
    'form',
    {
      class: 'login-card',
      onsubmit: async (e) => {
        e.preventDefault();
        const token = input.value.trim();
        if (!token) return;
        button.disabled = true;
        button.textContent = 'Signing in…';
        error.hidden = true;
        try {
          const me = await post('/v1/session', { token });
          input.value = '';
          onSignedIn(me);
        } catch (err) {
          error.replaceChildren(icon('warning'), h('span', null, err.status === 401 ? "That token isn't valid on this hub." : describe(err)));
          error.hidden = false;
          input.focus();
          input.select();
        } finally {
          button.disabled = false;
          button.textContent = 'Sign in';
        }
      },
    },
    h('span', { class: 'brand-mark', 'aria-hidden': 'true' }),
    h('div', null, h('h1', null, 'Sign in to Grouper'), h('p', null, 'Your hub, your devices, one conversation.')),
    error,
    // Lets password managers file the token under a name.
    h('input', { type: 'text', name: 'username', autocomplete: 'username', value: 'grouper', hidden: true, 'aria-hidden': 'true', tabindex: '-1' }),
    h('div', { class: 'field' }, h('label', { for: 'token' }, h('span', { class: 'field-label' }, 'Hub token')), h('div', { class: 'reveal-field' }, input, reveal)),
    h(
      'p',
      { class: 'hint', id: 'token-help' },
      'Use an admin token to chat, approve tool calls and change settings; a device token can only read. Make one on the hub with ',
      h('code', { class: 'nowrap' }, 'grouper-hub device add Browser --admin'),
      '. The browser keeps a session cookie for 30 days, never the token.',
    ),
    button,
  );
  requestAnimationFrame(() => input.focus());
  return h('div', { class: 'login' }, form);
}
