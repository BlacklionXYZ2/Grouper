// Formatting for times, money, counts and thread status.

const nf = new Intl.NumberFormat();

export function num(n) {
  return n === null || n === undefined ? '—' : nf.format(n);
}

/** USD with enough digits for small amounts. */
export function usd(n) {
  if (n === null || n === undefined || Number.isNaN(n)) return '—';
  const abs = Math.abs(n);
  const digits = abs === 0 ? 2 : abs < 0.01 ? 4 : 2;
  return `$${n.toFixed(digits)}`;
}

export function ago(unix) {
  if (!unix) return 'never';
  const s = Math.max(0, Math.round(Date.now() / 1000 - unix));
  if (s < 45) return 'just now';
  if (s < 3600) return `${Math.round(s / 60)}m ago`;
  if (s < 86400) return `${Math.round(s / 3600)}h ago`;
  if (s < 86400 * 30) return `${Math.round(s / 86400)}d ago`;
  return date(unix);
}

export function date(unix) {
  return new Date(unix * 1000).toLocaleDateString(undefined, { year: 'numeric', month: 'short', day: 'numeric' });
}

export function dateTime(unix) {
  return new Date(unix * 1000).toLocaleString(undefined, { month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit' });
}

export function time(unix) {
  return new Date(unix * 1000).toLocaleTimeString(undefined, { hour: '2-digit', minute: '2-digit' });
}

export function isoDay(d) {
  return d.toISOString().slice(0, 10);
}

export function duration(secs) {
  if (secs === null || secs === undefined) return '—';
  if (secs < 60) return `${Math.round(secs)}s`;
  if (secs < 3600) return `${Math.round(secs / 60)}m`;
  return `${(secs / 3600).toFixed(1)}h`;
}

/**
 * Elapsed time with seconds while it's short: "42s", "4m 05s", "1h 12m", "2d 3h".
 * For live timers (the threads panel and the thread view tick it every second).
 */
export function elapsed(secs) {
  if (secs === null || secs === undefined || Number.isNaN(secs)) return '—';
  const s = Math.max(0, Math.floor(secs));
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m ${String(s % 60).padStart(2, '0')}s`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ${String(Math.floor((s % 3600) / 60)).padStart(2, '0')}m`;
  return `${Math.floor(s / 86400)}d ${Math.floor((s % 86400) / 3600)}h`;
}

/** The same span as an ISO 8601 duration, for <time datetime>. */
export function isoDuration(secs) {
  return `PT${Math.max(0, Math.floor(secs || 0))}S`;
}

/**
 * How long a thread has run: live (now - created) while it runs or waits, the
 * total (finished - created) once it's over. `live` says which.
 */
export function threadElapsed(t, now = Date.now() / 1000) {
  if (!t?.created) return { secs: null, live: false };
  if (isActive(t.status)) return { secs: Math.max(0, now - t.created), live: true };
  if (t.finished) return { secs: Math.max(0, t.finished - t.created), live: false };
  return { secs: null, live: false };
}

export function pct(x) {
  return x === null || x === undefined ? '—' : `${Math.round(x * 100)}%`;
}

/** Status word, icon and tone for a thread. */
export function threadStatus(status, { checkpoint = false, approval = false } = {}) {
  if (approval) return { word: 'Needs approval', icon: 'shield', tone: 'warn' };
  switch (status) {
    case 'running':
      return { word: 'Running', icon: 'dot', tone: 'accent' };
    case 'waiting_device':
      return checkpoint
        ? { word: 'Checkpoint', icon: 'flag', tone: 'accent' }
        : { word: 'Waiting', icon: 'offline', tone: 'warn' };
    case 'done':
      return { word: 'Done', icon: 'check', tone: 'muted' };
    case 'failed':
      return { word: 'Failed', icon: 'x', tone: 'danger' };
    case 'stopped':
      return { word: 'Stopped', icon: 'stop', tone: 'muted' };
    default:
      return { word: status || 'Unknown', icon: 'dot', tone: 'muted' };
  }
}

export function isActive(status) {
  return status === 'running' || status === 'waiting_device';
}

/** Pretty JSON for tool inputs. */
export function pretty(value) {
  if (typeof value === 'string') {
    try {
      return JSON.stringify(JSON.parse(value), null, 2);
    } catch {
      return value;
    }
  }
  return JSON.stringify(value, null, 2);
}

export function plural(n, one, many = `${one}s`) {
  return `${n} ${n === 1 ? one : many}`;
}

/** 412K, 1.84M: three significant figures (the exact figure goes in a tooltip). */
export function abbr(n) {
  const v = Number(n) || 0;
  const trim = (x) => String(Number(x.toPrecision(3)));
  if (v >= 1e9) return `${trim(v / 1e9)}B`;
  if (v >= 1e6) return `${trim(v / 1e6)}M`;
  if (v >= 1e3) return `${trim(v / 1e3)}K`;
  return String(Math.round(v));
}

/** Tokens in = input + cache read + cache write; tokens out = output (usage-tab-spec 4.3). */
export const tokensIn = (r) => (r?.input_tokens || 0) + (r?.cache_read_tokens || 0) + (r?.cache_write_tokens || 0);
export const tokensOut = (r) => r?.output_tokens || 0;
