// #/link?code=ABCD-EFGH: approve or deny a device that asked to sign in (`grouper login`).
// GET /v1/auth/link/{code} shows the request; POST .../approve or .../deny answers it
// (admin). The device's token goes straight to the device, never to this page.
//
// Signed out, app.js remembers the code here (sessionStorage) and shows sign-in;
// after Google sign-in comes back to #/, it reopens this page.

import { h, icon, setChildren } from '../lib/dom.js';
import { get, post, enc, isAbort, describe } from '../lib/api.js';
import { isAdmin } from '../lib/store.js';
import { bar, skeletonBlocks, errorBanner, toastError, announce } from '../lib/ui.js';
import { ago } from '../lib/format.js';

const PENDING = 'grouper.pendingLink';
// A remembered request older than a code's life (10 minutes) isn't worth reopening.
const PENDING_MAX_AGE_MS = 15 * 60 * 1000;
const CODE_RE = /^[A-Za-z0-9 -]{4,16}$/;

/** Remember the request a signed-out visitor opened, to come back to after sign-in. */
export function rememberPendingLink(code) {
  if (!code || !CODE_RE.test(code)) return;
  try {
    sessionStorage.setItem(PENDING, JSON.stringify({ code, at: Date.now() }));
  } catch {
    /* storage unavailable: the person opens the link again */
  }
}

/** The hash of a remembered request (once), or null. */
export function takePendingLink() {
  try {
    const raw = sessionStorage.getItem(PENDING);
    sessionStorage.removeItem(PENDING);
    const { code, at } = JSON.parse(raw || 'null') || {};
    if (typeof code === 'string' && CODE_RE.test(code) && Date.now() - at < PENDING_MAX_AGE_MS) {
      return `#/link?code=${encodeURIComponent(code)}`;
    }
  } catch {
    /* storage unavailable or garbled */
  }
  return null;
}

const PLATFORMS = { windows: 'Windows', linux: 'Linux', macos: 'macOS', ios: 'iOS', android: 'Android' };

function platformName(p) {
  return PLATFORMS[p] || p || 'Unknown';
}

export function renderLink(container, { code }) {
  const body = h('div', { class: 'view-body narrow' });
  container.append(h('section', { class: 'view' }, bar({ title: 'Approve a device', caption: 'Sign-in request' }), body));
  const aborter = new AbortController();
  let busy = false;

  function message(kind, title, text, extra = null) {
    setChildren(body, h('div', { class: ['card', 'card-pad', 'link-card', `link-${kind}`], role: kind === 'error' ? 'alert' : 'status' }, h('h2', null, title), h('p', null, text), extra));
  }

  function showMissing() {
    message('gone', 'No device is waiting with this code', 'The code may have expired (they last 10 minutes) or been used already. Run grouper login on the device again for a new one.');
  }

  function showResult(req, status, deviceName) {
    if (status === 'approved') {
      message('ok', `${deviceName || req?.name || 'The device'} is signed in`, 'It finishes on its own in a few seconds. You can close this page.', h('p', null, h('a', { href: '#/devices' }, 'Go to Devices')));
    } else {
      message('denied', 'Denied', "The device wasn't signed in. If that was a mistake, run grouper login on it again.");
    }
  }

  function showRequest(req) {
    const approveBtn = h('button', { class: 'btn btn-primary', type: 'button', onclick: () => answer(req, 'approve') }, 'Approve');
    const denyBtn = h('button', { class: 'btn', type: 'button', onclick: () => answer(req, 'deny') }, 'Deny');
    const row = (label, value, cls) => h('div', { class: 'link-row' }, h('dt', null, label), h('dd', { class: cls }, value));
    const minutesLeft = Math.max(0, Math.ceil((req.expires - Date.now() / 1000) / 60));
    setChildren(
      body,
      h(
        'div',
        { class: 'card card-pad link-card' },
        h('h2', null, 'A device wants to sign in to your Grouper'),
        h('p', { class: 'hint' }, 'Approve only if you started this on that device. It gets its own device token (not an admin one), which you can revoke in Devices.'),
        h(
          'dl',
          { class: 'link-details' },
          row('Device', req.name),
          row('Platform', platformName(req.platform)),
          row('Asked', ago(req.requested_at)),
          row('Code', req.code, 'link-code mono'),
        ),
        h('p', { class: 'hint' }, `Check that the device shows the same code. This request ends in ${minutesLeft === 1 ? '1 minute' : `${minutesLeft} minutes`}.`),
        req.replaces
          ? h('div', { class: 'banner info' }, icon('devices'), h('div', { class: 'grow' }, `This signs “${req.replaces}” in again: its old sign-in stops working.`))
          : null,
        h('div', { class: 'link-actions' }, denyBtn, approveBtn),
      ),
    );
    requestAnimationFrame(() => approveBtn.focus());
  }

  async function answer(req, action) {
    if (busy) return;
    busy = true;
    for (const b of body.querySelectorAll('.link-actions button')) b.disabled = true;
    try {
      const res = await post(`/v1/auth/link/${enc(req.code)}/${action}`, {});
      announce(action === 'approve' ? 'Approved.' : 'Denied.');
      showResult(req, res?.status, res?.device?.name);
    } catch (err) {
      if (err.status === 404) showMissing();
      else if (err.status === 409) load();
      else {
        toastError(err);
        for (const b of body.querySelectorAll('.link-actions button')) b.disabled = false;
      }
    } finally {
      busy = false;
    }
  }

  async function load() {
    if (!code) {
      message('gone', 'This link has no code', 'Open the link that grouper login printed on the device.');
      return;
    }
    if (!isAdmin()) {
      message('error', 'This session can’t approve devices', 'Sign in with your Google account (or an admin token) to approve it.');
      return;
    }
    setChildren(body, skeletonBlocks(2));
    try {
      const req = await get(`/v1/auth/link/${enc(code)}`, { signal: aborter.signal });
      if (req.status === 'pending') showRequest(req);
      else showResult(req, req.status);
    } catch (err) {
      if (isAbort(err)) return;
      if (err.status === 404) showMissing();
      else if (err.status === 403) message('error', 'This session can’t approve devices', describe(err));
      else setChildren(body, errorBanner(err, load));
    }
  }

  load();
  return () => aborter.abort();
}
