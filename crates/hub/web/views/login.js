// Sign in: Google (POST /v1/auth/google/start -> Google -> /v1/auth/google/return)
// or hub token (POST /v1/session). Tokens and setup codes go to the hub once
// and are never kept by the browser.

import { h, icon, srOnly, setChildren } from '../lib/dom.js';
import { get, post, describe } from '../lib/api.js';

const SVG_NS = 'http://www.w3.org/2000/svg';
const GOOGLE_G_PATHS = [
  ['#EA4335', 'M24 9.5c3.54 0 6.71 1.22 9.21 3.6l6.85-6.85C35.9 2.38 30.47 0 24 0 14.62 0 6.51 5.38 2.56 13.22l7.98 6.19C12.43 13.72 17.74 9.5 24 9.5z'],
  ['#4285F4', 'M46.98 24.55c0-1.57-.15-3.09-.38-4.55H24v9.02h12.94c-.58 2.96-2.26 5.48-4.78 7.18l7.73 6c4.51-4.18 7.09-10.36 7.09-17.65z'],
  ['#FBBC05', 'M10.53 28.59c-.48-1.45-.76-2.99-.76-4.59s.27-3.14.76-4.59l-7.98-6.19C.92 16.46 0 20.12 0 24c0 3.88.92 7.54 2.56 10.78l7.97-6.19z'],
  ['#34A853', 'M24 48c6.48 0 11.93-2.13 15.89-5.81l-7.73-6c-2.15 1.45-4.92 2.3-8.16 2.3-6.26 0-11.57-4.22-13.47-9.91l-7.98 6.19C6.51 42.62 14.62 48 24 48z'],
];

function googleMark() {
  const svg = document.createElementNS(SVG_NS, 'svg');
  svg.setAttribute('viewBox', '0 0 48 48');
  svg.setAttribute('class', 'google-g');
  svg.setAttribute('aria-hidden', 'true');
  for (const [fill, d] of GOOGLE_G_PATHS) {
    const path = document.createElementNS(SVG_NS, 'path');
    path.setAttribute('fill', fill);
    path.setAttribute('d', d);
    svg.append(path);
  }
  return svg;
}

function hashWithoutError(keepClaim, code) {
  if (!keepClaim && !code) return '#/';
  if (code) return `#/claim?code=${encodeURIComponent(code)}`;
  return '#/claim';
}

export function renderLogin(onSignedIn, opts = {}) {
  const initialCode = opts.code || '';
  const initialError = opts.error || '';
  const fromClaim = Boolean(opts.claim);

  const card = h(
    'div',
    { class: 'login-card', 'aria-busy': 'true' },
    h('span', { class: 'brand-mark', 'aria-hidden': 'true' }),
    h('div', null, h('h1', null, fromClaim ? 'Claim this hub' : 'Sign in to Grouper'), h('p', null, 'Checking sign-in options…')),
  );
  const wrap = h('div', { class: 'login' }, card);

  function buildBody(status) {
    card.removeAttribute('aria-busy');
    const googleWeb = Boolean(status?.google_web);
    const claimed = status?.claimed ?? true;
    const claimMode = googleWeb && !claimed;

    const errorBox = h('div', { class: 'banner error', role: 'alert', hidden: true });
    let setupCodeInput = null;
    let googleBtn = null;
    let tokenInput = null;

    function clearError() {
      errorBox.hidden = true;
      errorBox.replaceChildren();
    }

    function dismissInitialError() {
      clearError();
      if (location.hash.includes('error=')) {
        const nextCode = setupCodeInput ? setupCodeInput.value.trim() : initialCode;
        history.replaceState(null, '', hashWithoutError(fromClaim, nextCode));
      }
      if (setupCodeInput && !setupCodeInput.value.trim()) setupCodeInput.focus();
      else if (googleBtn) googleBtn.focus();
      else tokenInput?.focus();
    }

    function showError(message, { retry = false } = {}) {
      setChildren(
        errorBox,
        icon('warning'),
        h('div', { class: 'grow' }, message),
        retry ? h('button', { class: 'btn btn-sm', type: 'button', onclick: dismissInitialError }, 'Try again') : null,
      );
      errorBox.hidden = false;
    }

    function buildTokenForm() {
      tokenInput = h('input', {
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
            const show = tokenInput.type === 'password';
            tokenInput.type = show ? 'text' : 'password';
            revealWord.textContent = show ? 'Hide' : 'Show';
          },
        },
        revealWord,
        srOnly(' token'),
      );
      const submitBtn = h('button', { class: 'btn btn-primary btn-lg', type: 'submit' }, 'Sign in');
      return h(
        'form',
        {
          class: 'login-form',
          onsubmit: async (e) => {
            e.preventDefault();
            const token = tokenInput.value.trim();
            if (!token) return;
            submitBtn.disabled = true;
            submitBtn.textContent = 'Signing in…';
            clearError();
            try {
              const me = await post('/v1/session', { token });
              tokenInput.value = '';
              onSignedIn(me);
            } catch (err) {
              showError(err.status === 401 ? "That token isn't valid on this hub." : describe(err));
              tokenInput.focus();
              tokenInput.select();
            } finally {
              submitBtn.disabled = false;
              submitBtn.textContent = 'Sign in';
            }
          },
        },
        // Lets password managers file the token under a name.
        h('input', { type: 'text', name: 'username', autocomplete: 'username', value: 'grouper', hidden: true, 'aria-hidden': 'true', tabindex: '-1' }),
        h('div', { class: 'field' }, h('label', { for: 'token' }, h('span', { class: 'field-label' }, 'Hub token')), h('div', { class: 'reveal-field' }, tokenInput, reveal)),
        h(
          'p',
          { class: 'hint', id: 'token-help' },
          'Use an admin token to chat, approve tool calls and change settings; a device token can only read. Make one on the hub with ',
          h('code', { class: 'nowrap' }, 'grouper-hub device add Browser --admin'),
          '. The browser keeps a session cookie for 30 days, never the token.',
        ),
        submitBtn,
      );
    }

    const tokenForm = buildTokenForm();
    const heading = h(
      'div',
      null,
      h('h1', null, claimMode ? 'Claim this hub' : 'Sign in to Grouper'),
      h('p', null, claimMode ? 'Claim this hub: sign in with the Google account that will own it.' : 'Your hub, your devices, one conversation.'),
    );

    if (!googleWeb) {
      const note =
        fromClaim && initialCode
          ? h('div', { class: 'banner info' }, icon('warning'), h('div', { class: 'grow' }, 'Browser Google sign-in isn’t turned on for this hub. Use a token below instead.'))
          : null;
      setChildren(card, h('span', { class: 'brand-mark', 'aria-hidden': 'true' }), heading, errorBox, note, tokenForm);
      if (initialError) showError(initialError, { retry: true });
      requestAnimationFrame(() => {
        if (initialError) errorBox.querySelector('button')?.focus();
        else tokenInput.focus();
      });
      return;
    }

    if (!claimed) {
      setupCodeInput = h('input', {
        class: 'input mono',
        type: 'text',
        id: 'setup-code',
        name: 'setup_code',
        value: initialCode,
        autocomplete: 'one-time-code',
        spellcheck: 'false',
        autocapitalize: 'characters',
        placeholder: 'XXXX-XXXX-XXXX',
        'aria-describedby': 'setup-code-help',
      });
    }

    const googleLabel = h('span', null, 'Sign in with Google');
    googleBtn = h(
      'button',
      {
        class: 'btn btn-lg google-btn',
        type: claimed ? 'button' : 'submit',
        onclick: claimed ? startGoogle : null,
      },
      googleMark(),
      googleLabel,
    );

    async function startGoogle() {
      const body = {};
      if (!claimed) {
        const code = setupCodeInput.value.trim();
        if (!code) {
          showError('Enter the setup code from the hub’s log (or run grouper-hub setup-code on the hub).');
          setupCodeInput.focus();
          return;
        }
        body.setup_code = code;
      }
      googleBtn.disabled = true;
      googleLabel.textContent = 'Redirecting to Google…';
      clearError();
      try {
        const res = await post('/v1/auth/google/start', body);
        if (!res?.auth_url) throw new Error("The hub didn't return a Google sign-in address.");
        location.assign(res.auth_url);
      } catch (err) {
        showError(describe(err));
        googleBtn.disabled = false;
        googleLabel.textContent = 'Sign in with Google';
      }
    }

    const googleSection = !claimed
      ? h(
          'form',
          {
            class: 'login-form',
            onsubmit: (e) => {
              e.preventDefault();
              startGoogle();
            },
          },
          h(
            'div',
            { class: 'field' },
            h('label', { for: 'setup-code' }, h('span', { class: 'field-label' }, 'Setup code')),
            setupCodeInput,
            h(
              'p',
              { class: 'hint', id: 'setup-code-help' },
              'Format XXXX-XXXX-XXXX, from the hub’s startup log or ',
              h('code', { class: 'nowrap' }, 'grouper-hub setup-code'),
              '.',
            ),
          ),
          googleBtn,
        )
      : googleBtn;

    const alreadyClaimedNote =
      fromClaim && initialCode
        ? h('div', { class: 'banner info' }, h('div', { class: 'grow' }, 'This hub already has an owner. Sign in with that Google account.'))
        : null;

    const disclosure = h(
      'details',
      {
        class: 'login-disclosure',
        ontoggle: () => {
          if (disclosure.open && !disclosure.contains(document.activeElement)) tokenInput.focus();
        },
      },
      h('summary', null, icon('chevron'), 'Use a token instead'),
      tokenForm,
    );

    setChildren(card, h('span', { class: 'brand-mark', 'aria-hidden': 'true' }), heading, errorBox, alreadyClaimedNote, googleSection, disclosure);
    if (initialError) showError(initialError, { retry: true });

    requestAnimationFrame(() => {
      if (initialError) errorBox.querySelector('button')?.focus();
      else if (setupCodeInput && !setupCodeInput.value.trim()) setupCodeInput.focus();
      else googleBtn.focus();
    });
  }

  get('/v1/auth/status')
    .then((status) => {
      if (wrap.isConnected) buildBody(status);
    })
    .catch(() => {
      if (wrap.isConnected) buildBody({ claimed: true, google: false, google_web: false });
    });

  return wrap;
}
