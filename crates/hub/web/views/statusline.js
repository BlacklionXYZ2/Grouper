// Desktop status line (design system 3.8): a 26px strip under the main column.
// Left: connection and "N of M devices". Right: up to four subscription meters,
// closest to their limit first, each a link to Usage. Hidden under 1100px, when the
// owner turned it off in Settings, and when no subscription is being read.
//
// The meters come from the same read the Usage tab makes (`/v1/usage/snapshots`,
// through usage-common's `load`, so the last good copy is shared) and the same
// naming helpers. It refreshes every five minutes and pauses while the tab is hidden.

import { h, setChildren, setText, setAttr } from '../lib/dom.js';
import { store, on } from '../lib/store.js';
import { load, nowSecs, STALE_SECS } from './usage-common.js';
import { PLAN_PROVIDERS, planTitle, parseWindow, spanLabel, resetWords } from './usage.js';

const PREF_KEY = 'grouper.statusLine';
const PREF_EVENT = 'grouper-statusline-pref';
const WIDE = '(min-width: 1100px)';
const REFRESH_SECS = 300;
const MAX_ITEMS = 4;

/** The Settings switch: on unless the owner turned it off on this browser. */
export function statuslineOn() {
  try {
    return localStorage.getItem(PREF_KEY) !== 'off';
  } catch {
    return true;
  }
}

export function setStatuslineOn(on_) {
  try {
    if (on_) localStorage.removeItem(PREF_KEY);
    else localStorage.setItem(PREF_KEY, 'off');
  } catch {
    /* storage unavailable: the switch still works until reload */
  }
  window.dispatchEvent(new CustomEvent(PREF_EVENT, { detail: Boolean(on_) }));
}

/** "38m", "2h 14m", "3d 4h": the reset time, short. */
export function shortReset(secs) {
  const s = Math.round(secs);
  if (s < 60) return '<1m';
  if (s < 3600) return `${Math.round(s / 60)}m`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ${String(Math.floor((s % 3600) / 60)).padStart(2, '0')}m`;
  return `${Math.floor(s / 86400)}d ${Math.floor((s % 86400) / 3600)}h`;
}

/**
 * One item per subscription account (provider plus account key, or plus device when
 * the reading has no account), showing its fullest window. Readings that have reset
 * or aren't in use are left out; they carry no number worth a glance.
 */
export function meterItems(snapshots, now = nowSecs()) {
  const rows = (Array.isArray(snapshots) ? snapshots : []).filter((s) => s.source === 'probe' && s.kind === 'window');
  const accounts = new Map();
  for (const s of rows) {
    const pct = s.used_pct;
    if (pct === null || pct === undefined || Number.isNaN(pct)) continue;
    if (s.resets_at && s.resets_at <= now) continue;
    if (!s.resets_at && !(pct > 0)) continue;
    const key = `${s.provider}|${s.account_key ? `k:${s.account_key}` : `d:${s.device_id || ''}`}`;
    let a = accounts.get(key);
    if (!a) {
      a = { key, provider: s.provider, plan: planTitle(s), newest: 0, best: null };
      accounts.set(key, a);
    }
    if (s.fetched_at > a.newest) {
      a.newest = s.fetched_at;
      a.plan = planTitle(s);
    }
    if (!a.best || pct > a.best.used_pct) a.best = s;
  }
  return [...accounts.values()]
    .map((a) => {
      const used = Math.max(0, Math.min(100, a.best.used_pct));
      return {
        key: a.key,
        provider: a.provider,
        fam: PLAN_PROVIDERS[a.provider]?.fam || 'unknown',
        plan: a.plan,
        span: spanLabel(parseWindow(a.best.window)),
        used,
        resetsAt: a.best.resets_at || null,
        stale: now - a.newest > STALE_SECS,
      };
    })
    .sort((x, y) => y.used - x.used || x.plan.localeCompare(y.plan))
    .slice(0, MAX_ITEMS);
}

function connectionState() {
  const text = store.online ? 'Connected' : store.everOnline ? 'Offline, reconnecting' : 'Connecting';
  // Only devices that run threads count; browsers and phones never connect as nodes.
  const devices = (store.devices || []).filter((d) => d.platform !== 'web' && d.platform !== 'ios');
  const up = devices.filter((d) => d.online).length;
  return { text, online: store.online, devices: devices.length ? `${up} of ${devices.length} ${devices.length === 1 ? 'device' : 'devices'}` : '' };
}

function itemEl(it, now) {
  const shown = it.used >= 99 && it.used < 100 ? Math.floor(it.used * 10) / 10 : Math.round(it.used);
  const state = it.used >= 100 ? 'danger' : it.used >= 80 ? 'warn' : 'ok';
  const word = state === 'danger' ? ', used up' : state === 'warn' ? ', nearly used' : '';
  const reset = it.resetsAt ? shortReset(it.resetsAt - now) : '';
  const full = `${it.plan}, ${it.span}: ${shown}% used${word}${it.resetsAt ? `, ${resetWords(it.resetsAt, { long: true })}` : ''}${it.stale ? ', reading is stale' : ''}`;
  const fill = h('span', { class: 'sl-fill' });
  fill.style.width = `${it.used}%`;
  return h(
    'li',
    null,
    h(
      'a',
      { class: ['sl-item', `fam-${it.fam}`, `is-${state}`, it.stale && 'is-stale'], href: '#/usage', title: full },
      h('span', { class: 'visually-hidden' }, `${it.plan}, ${it.span}: `),
      h('span', { class: 'sl-mark', 'aria-hidden': 'true' }),
      h('span', { class: 'sl-meter', 'aria-hidden': 'true' }, fill),
      h('span', { class: 'sl-pct' }, `${shown}%`, h('span', { class: 'visually-hidden' }, ` used${word}`)),
      reset ? h('span', { class: 'sl-reset' }, h('span', { class: 'visually-hidden' }, 'resets in '), reset) : null,
    ),
  );
}

/** Add the strip to the shell grid; it follows the preference, the width and the data. */
export function mountStatusline(shell, signal) {
  const conn = h('span', { class: 'sl-conn' });
  const dev = h('span', { class: 'sl-dev' });
  const list = h('ul', { class: 'sl-list' });
  const nav = h('nav', { class: 'sl-meters', 'aria-label': 'Subscription usage' }, list);
  const el = h('footer', { class: 'statusline', hidden: true }, h('div', { class: 'sl-left' }, conn, dev), nav);
  shell.append(el);

  const wide = typeof matchMedia === 'function' ? matchMedia(WIDE) : null;
  let items = [];
  let fetchedAt = 0;
  let loading = false;

  const active = () => statuslineOn() && (!wide || wide.matches);

  function draw() {
    const show = active() && items.length > 0;
    el.hidden = !show;
    shell.classList.toggle('has-statusline', show);
    if (!show) return;
    const now = nowSecs();
    const c = connectionState();
    setText(conn, c.text);
    setAttr(conn, 'data-online', c.online ? 'true' : 'false');
    setText(dev, c.devices);
    dev.hidden = !c.devices;
    setChildren(list, items.map((it) => itemEl(it, now)));
  }

  async function refresh() {
    if (loading || signal.aborted || !active() || document.hidden || (!store.online && store.everOnline)) return;
    loading = true;
    try {
      const res = await load('/v1/usage/snapshots', signal);
      if (signal.aborted) return;
      fetchedAt = Date.now() / 1000;
      items = res.data ? meterItems(res.data) : [];
    } catch {
      // Aborted or failed with no copy: keep what is drawn.
    } finally {
      loading = false;
    }
    draw();
  }

  const tick = () => {
    if (document.hidden || signal.aborted) return;
    if (nowSecs() - fetchedAt >= REFRESH_SECS) refresh();
    else draw(); // reset countdowns move on their own
  };
  const timer = setInterval(tick, 60000);
  const offs = ['online', 'devices', 'device_presence'].map((topic) => on(topic, draw));
  const onPref = () => {
    if (active() && !fetchedAt) refresh();
    else draw();
  };
  const onVisible = () => {
    if (!document.hidden) tick();
  };
  window.addEventListener(PREF_EVENT, onPref, { signal });
  document.addEventListener('visibilitychange', onVisible, { signal });
  wide?.addEventListener?.('change', onPref);
  signal.addEventListener('abort', () => {
    clearInterval(timer);
    offs.forEach((off) => off());
    wide?.removeEventListener?.('change', onPref);
    shell.classList.remove('has-statusline');
    el.remove();
  });

  refresh();
}
