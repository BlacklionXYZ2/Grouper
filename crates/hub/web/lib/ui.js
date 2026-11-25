// Shared UI pieces: navigation, toasts, confirmations, status pills, skeletons,
// screen-reader announcements.

import { h, icon } from './dom.js';
import { threadStatus } from './format.js';
import { describe } from './api.js';

export function navigate(hash) {
  if (location.hash === hash) window.dispatchEvent(new HashChangeEvent('hashchange'));
  else location.hash = hash;
}

export function toast(message, kind = 'info') {
  let host = document.querySelector('.toasts');
  if (!host) {
    host = h('div', { class: 'toasts' });
    document.body.append(host);
  }
  const el = h(
    'div',
    { class: ['toast', 'glass', kind], role: kind === 'error' ? 'alert' : 'status' },
    icon(kind === 'error' ? 'warning' : 'check'),
    h('span', null, message),
  );
  host.append(el);
  setTimeout(() => el.remove(), kind === 'error' ? 6000 : 3200);
}

export function toastError(error) {
  toast(describe(error), 'error');
}

/**
 * A modal sheet built on <dialog>, so focus is trapped and Escape closes it.
 * `build(close)` returns {title, caption, body: Node[], foot: Node[]}.
 * Resolves with whatever `close(value)` was called with (undefined on Escape).
 */
export function sheet(build, { label } = {}) {
  return new Promise((resolve) => {
    const dialog = h('dialog', { class: 'sheet-dialog', 'aria-label': label || null });
    let done = false;
    const close = (value) => {
      if (done) return;
      done = true;
      dialog.close();
      dialog.remove();
      resolve(value);
    };
    const parts = build(close);
    const titleId = `dlg-${Math.random().toString(36).slice(2)}`;
    if (!label) dialog.setAttribute('aria-labelledby', titleId);
    dialog.append(
      h('div', { class: 'dialog-head' }, h('h2', { id: titleId }, parts.title), parts.caption ? h('div', { class: 'caption' }, parts.caption) : null),
      h('div', { class: 'dialog-body' }, parts.body || []),
      parts.foot ? h('div', { class: 'dialog-foot' }, parts.foot) : null,
    );
    dialog.addEventListener('cancel', (e) => {
      e.preventDefault();
      close(undefined);
    });
    dialog.addEventListener('click', (e) => {
      if (e.target === dialog) close(undefined);
    });
    document.body.append(dialog);
    dialog.showModal();
    parts.onOpen?.(dialog);
  });
}

/** Ask before something that can't be undone. Resolves true to go ahead. */
export function confirmAction({ title, body, confirmLabel = 'Confirm', danger = false, cancelLabel = 'Cancel' }) {
  return sheet((close) => {
    const ok = h('button', { class: ['btn', danger ? 'btn-danger' : 'btn-primary'], onclick: () => close(true) }, confirmLabel);
    return {
      title,
      body: (Array.isArray(body) ? body : [body]).map((b) => (typeof b === 'string' ? h('p', { class: 'dialog-text' }, b) : b)),
      foot: [h('button', { class: 'btn', onclick: () => close(false), autofocus: true }, cancelLabel), ok],
    };
  }).then(Boolean);
}

export function statusPill(status, opts) {
  const s = threadStatus(status, opts);
  return h(
    'span',
    { class: ['status', s.tone] },
    status === 'running' && !opts?.approval ? h('span', { class: 'live-dot', 'aria-hidden': 'true' }) : icon(s.icon),
    s.word,
  );
}

export function skeleton(lines = 3) {
  const widths = ['w80', 'w60', 'w40'];
  return h('div', { 'aria-hidden': 'true' }, Array.from({ length: lines }, (_, i) => h('div', { class: ['skeleton', widths[i % 3]] })));
}

export function skeletonBlocks(n = 3) {
  return h('div', { 'aria-busy': 'true', 'aria-label': 'Loading' }, Array.from({ length: n }, () => h('div', { class: 'skeleton block' })));
}

export function errorBanner(error, retry) {
  return h(
    'div',
    { class: 'banner error', role: 'alert' },
    icon('warning'),
    h('div', { class: 'grow' }, describe(error)),
    retry ? h('button', { class: 'btn btn-sm', onclick: retry }, 'Try again') : null,
  );
}

let lastAnnounce = 0;
let pendingAnnounce = null;
/** Speak through the polite live region, at most every few seconds unless forced. */
export function announce(text, { force = false } = {}) {
  const region = document.getElementById('live-region');
  if (!region) return;
  const now = Date.now();
  clearTimeout(pendingAnnounce);
  if (force || now - lastAnnounce > 8000) {
    lastAnnounce = now;
    region.textContent = '';
    // A fresh text node makes screen readers read it again.
    setTimeout(() => {
      region.textContent = text;
    }, 30);
  } else {
    pendingAnnounce = setTimeout(() => announce(text, { force: true }), 8000 - (now - lastAnnounce));
  }
}

export function openDrawer() {
  document.querySelector('.shell')?.classList.add('drawer-open');
  document.querySelector('.drawer-scrim')?.removeAttribute('hidden');
  document.querySelector('.sidebar a, .sidebar button')?.focus();
}

export function closeDrawer() {
  document.querySelector('.shell')?.classList.remove('drawer-open');
  document.querySelector('.drawer-scrim')?.setAttribute('hidden', '');
}

/** The bar at the top of a view (glass). */
export function bar({ title, caption, actions = [], back = null, menu = true }) {
  return h(
    'header',
    { class: 'bar glass' },
    back
      ? h('a', { class: 'icon-btn', href: back, 'aria-label': 'Back' }, icon('back'))
      : menu
        ? h('button', { class: 'icon-btn only-mobile', type: 'button', 'aria-label': 'Open menu', onclick: openDrawer }, icon('menu'))
        : null,
    h('div', { class: 'bar-titles' }, h('h1', null, title), caption ? h('div', { class: 'caption' }, caption) : null),
    h('div', { class: 'bar-actions' }, actions),
  );
}

/** Auto-grow a textarea where `field-sizing: content` isn't supported. */
export function autoGrow(textarea, max = 168) {
  if (CSS.supports('field-sizing', 'content')) return;
  const fit = () => {
    textarea.style.height = 'auto';
    textarea.style.height = `${Math.min(max, textarea.scrollHeight)}px`;
  };
  textarea.addEventListener('input', fit);
  requestAnimationFrame(fit);
}
