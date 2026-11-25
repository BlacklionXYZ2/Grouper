// Shared UI pieces: navigation, toasts, confirmations, status pills, skeletons,
// screen-reader announcements, the mobile drawer and the offline write guard.

import { h, icon } from './dom.js';
import { threadStatus } from './format.js';
import { describe } from './api.js';
import { store, on } from './store.js';

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

/**
 * Ask before something that can't be undone. Resolves true to go ahead.
 * `callout` is a warning shown above the text (danger-toned when `danger`);
 * `typeToConfirm` makes the person type that word before the button works.
 */
export function confirmAction({ title, body, confirmLabel = 'Confirm', danger = false, cancelLabel = 'Cancel', callout = null, typeToConfirm = null }) {
  return sheet((close) => {
    const ok = h('button', { class: ['btn', danger ? 'btn-danger' : 'btn-primary'], type: 'button', onclick: () => close(true) }, confirmLabel);
    const parts = [];
    if (callout) {
      parts.push(h('div', { class: ['callout', 'callout-icon', danger ? 'danger' : 'warn'] }, icon('warning'), h('div', null, callout)));
    }
    parts.push(...(Array.isArray(body) ? body : [body]).map((b) => (typeof b === 'string' ? h('p', { class: 'dialog-text' }, b) : b)));
    let input = null;
    if (typeToConfirm) {
      const id = `confirm-${Math.random().toString(36).slice(2)}`;
      ok.disabled = true;
      input = h('input', {
        class: 'input mono',
        id,
        type: 'text',
        autocomplete: 'off',
        spellcheck: 'false',
        autocapitalize: 'off',
        'aria-describedby': `${id}-help`,
        oninput: () => {
          ok.disabled = input.value.trim() !== typeToConfirm;
        },
        onkeydown: (e) => {
          if (e.key === 'Enter' && !ok.disabled) {
            e.preventDefault();
            close(true);
          }
        },
      });
      parts.push(
        h(
          'div',
          { class: 'field' },
          h('label', { for: id }, h('span', { class: 'field-label' }, 'Type ', h('code', null, typeToConfirm), ' to confirm')),
          input,
          h('p', { class: 'hint', id: `${id}-help` }, 'The button turns on when the name matches.'),
        ),
      );
    }
    return {
      title,
      body: parts,
      foot: [h('button', { class: 'btn', type: 'button', onclick: () => close(false), autofocus: !typeToConfirm }, cancelLabel), ok],
      onOpen: () => input?.focus(),
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

/**
 * Speak a streaming text by sentences through `announce` (which throttles), never
 * per delta. Call `speak(text)` with the whole text so far; `reset()` when it lands.
 */
export function sentenceSpeaker() {
  let spokenTo = 0;
  return {
    speak(text) {
      const all = text || '';
      if (all.length < spokenTo) spokenTo = 0;
      const m = all.slice(spokenTo).match(/^[\s\S]*[.!?](\s|$)/);
      if (m) {
        spokenTo += m[0].length;
        announce(m[0].trim());
      }
    },
    reset() {
      spokenTo = 0;
    },
  };
}

// ---- mobile drawer

const narrow = typeof matchMedia === 'function' ? matchMedia('(max-width: 760px)') : null;
let drawerOpener = null;

export function isNarrow() {
  return Boolean(narrow?.matches);
}

function drawerIsOpen() {
  return Boolean(document.querySelector('.shell')?.classList.contains('drawer-open'));
}

/**
 * Make the background match the layer on top: on narrow screens the closed drawer
 * is off-screen, so it's inert; an open drawer makes everything else inert.
 * `extra` are more layers that are modal right now (the overlay inspector).
 */
export function syncInert() {
  const shell = document.querySelector('.shell');
  if (!shell) return;
  const sidebar = shell.querySelector('.sidebar');
  const others = [shell.querySelector('.main'), shell.querySelector('.tabbar'), shell.querySelector('.skip-link')];
  const inspector = shell.querySelector('.inspector');
  const open = drawerIsOpen();
  const overlayInspector = inspector && !inspector.hidden && matchMedia('(max-width: 1180px)').matches;
  if (sidebar) sidebar.inert = (isNarrow() && !open) || overlayInspector;
  for (const el of others) if (el) el.inert = open || overlayInspector;
  if (inspector) inspector.inert = open;
  for (const btn of document.querySelectorAll('.menu-btn')) btn.setAttribute('aria-expanded', open ? 'true' : 'false');
}

narrow?.addEventListener('change', () => {
  if (!isNarrow()) closeDrawer();
  syncInert();
});
matchMedia?.('(max-width: 1180px)').addEventListener?.('change', syncInert);

export function openDrawer() {
  const shell = document.querySelector('.shell');
  if (!shell) return;
  drawerOpener = document.activeElement;
  shell.classList.add('drawer-open');
  document.querySelector('.drawer-scrim')?.removeAttribute('hidden');
  syncInert();
  (shell.querySelector('.sidebar [aria-current="page"]') || shell.querySelector('.sidebar a, .sidebar button'))?.focus();
}

/** Close the drawer; `restoreFocus` returns focus to the button that opened it. */
export function closeDrawer({ restoreFocus = false } = {}) {
  const shell = document.querySelector('.shell');
  const wasOpen = drawerIsOpen();
  shell?.classList.remove('drawer-open');
  document.querySelector('.drawer-scrim')?.setAttribute('hidden', '');
  syncInert();
  if (wasOpen && restoreFocus) {
    const target = drawerOpener?.isConnected ? drawerOpener : document.querySelector('.main .menu-btn');
    target?.focus();
  }
  if (wasOpen) drawerOpener = null;
}

/** The bar at the top of a view (glass). */
export function bar({ title, caption, actions = [], back = null, menu = true }) {
  return h(
    'header',
    { class: 'bar glass' },
    back
      ? h('a', { class: 'icon-btn', href: back, 'aria-label': 'Back' }, icon('back'))
      : menu
        ? h(
            'button',
            {
              class: 'icon-btn only-mobile menu-btn',
              type: 'button',
              'aria-label': 'Open menu',
              'aria-controls': 'sidebar',
              'aria-expanded': drawerIsOpen() ? 'true' : 'false',
              onclick: openDrawer,
            },
            icon('menu'),
          )
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

// ---- offline write guard

/** True while the live connection is down after having been up. */
export function isOffline() {
  return !store.online && store.everOnline;
}

/**
 * While the hub is unreachable, disable every control under `root` marked
 * `data-write` (now and as views re-render) and show a note at the top of `root`.
 * Returns a cleanup function.
 */
export function guardWrites(root) {
  const note = h('div', { class: 'banner warn', role: 'status', hidden: true }, icon('offline'), h('span', { class: 'grow' }, 'Offline. Changes are paused until the hub reconnects.'));
  root.prepend(note);
  const apply = () => {
    const off = isOffline();
    note.hidden = !off;
    for (const el of root.querySelectorAll('[data-write]')) {
      if (off) {
        if (!el.disabled) {
          el.disabled = true;
          el.dataset.offlineDisabled = '';
        }
      } else if ('offlineDisabled' in el.dataset) {
        el.disabled = false;
        delete el.dataset.offlineDisabled;
      }
    }
  };
  const observer = new MutationObserver(() => {
    if (isOffline()) apply();
  });
  observer.observe(root, { childList: true, subtree: true });
  const off = on('online', apply);
  apply();
  return () => {
    observer.disconnect();
    off();
  };
}
