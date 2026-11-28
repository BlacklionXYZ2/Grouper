// Shared helpers of the Usage pages (summary, breakdown, limits): the last-good-copy
// loader, money and token formatting, the provider-reported rows and the meter.
// Moved out of the old single-file usage.js without changing behaviour.

import { h, icon } from '../lib/dom.js';
import { get, describe, isAbort } from '../lib/api.js';
import { store } from '../lib/store.js';
import { errorBanner } from '../lib/ui.js';
import { num, usd, ago, dateTime } from '../lib/format.js';

// A snapshot older than this is labelled stale, with its time.
export const STALE_SECS = 3600;

// ---- last good copy of each read, so an offline visit still shows something

export const cache = new Map(); // `${hub}|${path}` -> {data, at}

export async function load(path, signal) {
  const key = `${store.hub?.hub_id || ''}|${path}`;
  try {
    const data = await get(path, { signal });
    const at = Date.now();
    cache.set(key, { data, at });
    return { data, at, error: null, cached: false };
  } catch (error) {
    if (isAbort(error)) throw error;
    const hit = cache.get(key);
    if (hit) return { data: hit.data, at: hit.at, error, cached: true };
    return { data: null, at: 0, error, cached: false };
  }
}

// ---- small formatting helpers

export const nowSecs = () => Date.now() / 1000;

/** The "estimated" tag that goes beside every Grouper cost figure. */
export function est() {
  return h('span', { class: 'badge u-est' }, 'estimated');
}

/** An amount in its own unit: USD as dollars, anything else with its code. */
export function money(amount, currency) {
  if (amount === null || amount === undefined || Number.isNaN(amount)) return '—';
  const c = String(currency || '').trim();
  if (!c) return num(Math.round(amount * 100) / 100);
  if (c.toUpperCase() === 'USD') return usd(amount);
  if (c.toLowerCase() === 'credits') return `${num(Math.round(amount * 100) / 100)} credits`;
  return `${amount.toFixed(2)} ${c}`;
}

export function sumRows(rows) {
  return rows.reduce(
    (t, r) => ({
      calls: t.calls + r.calls,
      input_tokens: t.input_tokens + r.input_tokens,
      output_tokens: t.output_tokens + r.output_tokens,
      cache_read_tokens: t.cache_read_tokens + r.cache_read_tokens,
      cache_write_tokens: t.cache_write_tokens + r.cache_write_tokens,
      cost: t.cost + r.cost,
    }),
    { calls: 0, input_tokens: 0, output_tokens: 0, cache_read_tokens: 0, cache_write_tokens: 0, cost: 0 },
  );
}

export const tokensOf = (r) => r.input_tokens + r.output_tokens + r.cache_read_tokens + r.cache_write_tokens;

/** "in 2h 14m" style, for reset times. */
export function inDuration(secs) {
  const s = Math.round(secs);
  if (s <= 0) return 'now';
  if (s < 60) return 'in under a minute';
  if (s < 3600) return `in ${Math.round(s / 60)} min`;
  if (s < 86400) return `in ${Math.floor(s / 3600)}h ${String(Math.floor((s % 3600) / 60)).padStart(2, '0')}m`;
  return `in ${Math.floor(s / 86400)}d ${Math.floor((s % 86400) / 3600)}h`;
}

export function slug(s) {
  return String(s).replace(/[^a-zA-Z0-9_-]+/g, '-');
}

export function hostOf(provider) {
  try {
    return new URL(provider.base_url).host.toLowerCase();
  } catch {
    return '';
  }
}

/** Which official balance API the hub reads for this provider (see usage.rs `collector`). */
export function balanceApi(provider) {
  if (!provider || provider.kind !== 'openai') return null;
  // Includes "openrouter" with no base_url, which defaults to OpenRouter.
  if (isOpenRouter(provider)) return { name: 'OpenRouter', source: 'OpenRouter key and credits API' };
  if (hostOf(provider) === 'api.deepseek.com') return { name: 'DeepSeek', source: 'DeepSeek balance API' };
  return null;
}

/** Whether a provider talks to OpenRouter (see usage.rs `is_openrouter`). */
export function isOpenRouter(provider) {
  if (!provider || provider.kind !== 'openai') return false;
  return provider.base_url ? hostOf(provider) === 'openrouter.ai' : provider.id === 'openrouter';
}

// OpenRouter's daily free-model allowance, as the hub records it from a 429.
export const FREE_KIND = 'requests';
export const FREE_WINDOW = 'free requests/day';
export const isFreeAllowance = (s) => s.kind === FREE_KIND && s.window === FREE_WINDOW;

/** Local "14:05" for a Unix time. */
export function clock(secs) {
  return new Date(secs * 1000).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
}

export function isLocalHost(host) {
  return /^(localhost|127\.|\[::1\]|0\.0\.0\.0)/.test(host) || host.endsWith('.local') || host.endsWith('.localhost');
}

/** A bar plus the same facts in words; the bar alone never carries the meaning. */
export function meter(pct, words) {
  const p = Math.max(0, Math.min(100, pct));
  const tone = pct >= 100 ? 'danger' : pct >= 80 ? 'warn' : 'ok';
  const fill = h('span', { class: 'u-meter-fill' });
  fill.style.width = `${p}%`;
  return h(
    'div',
    { class: ['u-meter-wrap', tone] },
    h('div', { class: 'u-meter', 'aria-hidden': 'true' }, fill),
    h('div', { class: 'u-meter-text' }, words, pct >= 100 ? h('span', { class: 'badge danger' }, icon('warning'), 'Used up') : pct >= 80 ? h('span', { class: 'badge warn' }, icon('warning'), 'Nearly used') : null),
  );
}

/** "Fetched 5m ago", plus a stale label with the exact time once it's over an hour old. */
export function ageLine(fetchedAt, source = '') {
  const stale = nowSecs() - fetchedAt > STALE_SECS;
  return h(
    'div',
    { class: 'u-meta' },
    icon('clock'),
    h('span', null, `Fetched ${ago(fetchedAt)}${source ? ` · ${source}` : ''}`),
    stale ? h('span', { class: 'badge warn' }, `Stale · ${dateTime(fetchedAt)}`) : null,
  );
}

/** End a message with a full stop unless it already has one. */
export function sentence(text) {
  const t = String(text || '').trim();
  return /[.!?]$/.test(t) ? t : `${t}.`;
}

/** What a read that failed (or fell back to the cached copy) looks like inline. */
export function sourceProblem(res, what, retry) {
  if (!res?.error) return null;
  if (res.cached) {
    return h(
      'div',
      { class: 'banner warn u-banner', role: 'status' },
      icon('offline'),
      h('div', { class: 'grow' }, `Couldn’t load ${what}: ${sentence(describe(res.error))} Showing the copy from ${ago(res.at / 1000)}.`),
      retry ? h('button', { class: 'btn btn-sm', type: 'button', onclick: retry }, 'Try again') : null,
    );
  }
  const banner = errorBanner(res.error, retry);
  banner.classList.add('u-banner');
  banner.setAttribute('role', 'status'); // a background read, not an interrupted action
  banner.querySelector('.grow')?.prepend(`Couldn’t load ${what}. `);
  return banner;
}

/**
 * OpenRouter's daily free-model allowance: what its last 429 said (never a
 * built-in number), then Grouper's own count of free-model calls today.
 */
export function freeAllowance(s, free) {
  const parts = [];
  if (s) {
    const limit = s.limit_amount ?? null;
    const used = s.used_amount ?? null;
    const left = limit !== null && used !== null ? Math.max(0, limit - used) : s.used_pct >= 100 ? 0 : null;
    const reset = s.resets_at || null;
    // With no reset time, a reading is only trusted for the UTC day it was taken in
    // (OpenRouter's free allowance is daily).
    const passed = reset !== null ? reset <= nowSecs() : Math.floor(s.fetched_at / 86400) < Math.floor(nowSecs() / 86400);
    if (!passed) {
      const value = [
        left !== null ? (limit !== null ? `${num(left)} left of ${num(limit)}` : `${num(left)} left`) : limit !== null ? `limit ${num(limit)}` : 'limit reached',
        reset ? `resets ${clock(reset)}` : null,
      ]
        .filter(Boolean)
        .join(' · ');
      parts.push(
        h('div', { class: 'u-rep' }, h('div', { class: 'u-rep-line' }, h('span', { class: 'u-rep-k' }, 'Free requests'), h('span', { class: 'u-rep-v' }, value))),
        s.used_pct !== null && s.used_pct !== undefined ? meter(s.used_pct, `${Math.round(s.used_pct)}% of today’s free requests used`) : null,
        reset ? h('div', { class: 'u-meta' }, `Resets ${inDuration(reset - nowSecs())} (${dateTime(reset)})`) : h('div', { class: 'u-meta' }, 'Reset time not reported'),
      );
    } else {
      parts.push(
        h(
          'div',
          { class: 'u-rep' },
          h('div', { class: 'u-rep-line' }, h('span', { class: 'u-rep-k' }, 'Free requests'), h('span', { class: 'u-rep-v' }, limit !== null ? `${num(limit)} a day` : 'Limit not reported')),
          h(
            'div',
            { class: 'u-meta' },
            reset !== null
              ? `Ran out ${ago(s.fetched_at)}; it reset at ${clock(reset)} (${dateTime(reset)}), so what’s left now isn’t known.`
              : `Ran out ${ago(s.fetched_at)}, before today (UTC), so what’s left now isn’t known.`,
          ),
        ),
      );
    }
    parts.push(h('div', { class: 'u-meta' }, icon('clock'), h('span', null, `Seen ${ago(s.fetched_at)} · ${s.source}`)));
  } else {
    parts.push(h('p', { class: 'hint' }, 'Allowance unavailable: OpenRouter reports its free-request limit only when a call runs into it.'));
  }
  const count = free.known ? num(free.sent) : 'unavailable';
  parts.push(
    h(
      'div',
      { class: 'u-rep u-free-sent' },
      h('div', { class: 'u-rep-line' }, h('span', { class: 'u-rep-k' }, 'Sent by Grouper today'), h('span', { class: 'u-rep-v' }, count)),
      h('div', { class: 'u-meta' }, 'Grouper’s own count of answered calls to :free models since 00:00 UTC. Not the allowance: other apps on the account aren’t seen.'),
    ),
  );
  return parts;
}

export function reportedRow(s) {
  const windowWords = { monthly: 'this month', weekly: 'this week', daily: 'today', total: 'all time', current: 'now' }[s.window] || s.window;
  let label;
  let value;
  let pct = s.used_pct;
  if (s.kind === 'spend') {
    label = `Key spend, ${windowWords}`;
    value = s.limit_amount !== null && s.limit_amount !== undefined ? `${money(s.used_amount, s.currency)} of ${money(s.limit_amount, s.currency)} limit` : `${money(s.used_amount, s.currency)} (no limit on the key)`;
  } else if (s.kind === 'balance' && s.limit_amount !== null && s.limit_amount !== undefined) {
    label = 'Credits';
    const left = s.limit_amount - (s.used_amount || 0);
    value = `${money(left, s.currency)} left of ${money(s.limit_amount, s.currency)}`;
  } else if (s.kind === 'balance') {
    label = 'Balance';
    value = money(s.used_amount, s.currency);
    pct = null;
  } else {
    label = `${s.kind}${s.window ? `, ${windowWords}` : ''}`;
    value =
      s.limit_amount !== null && s.limit_amount !== undefined
        ? `${money(s.used_amount, s.currency)} used of ${money(s.limit_amount, s.currency)}`
        : s.used_amount !== null && s.used_amount !== undefined
          ? money(s.used_amount, s.currency)
          : '';
  }
  const resets = s.resets_at ? h('div', { class: 'u-meta' }, `Resets ${inDuration(s.resets_at - nowSecs())} (${dateTime(s.resets_at)})`) : null;
  return h(
    'div',
    { class: 'u-rep' },
    h('div', { class: 'u-rep-line' }, h('span', { class: 'u-rep-k' }, label), h('span', { class: 'u-rep-v' }, value || '—')),
    pct !== null && pct !== undefined ? meter(pct, `${Math.round(pct)}% used`) : null,
    resets,
  );
}
