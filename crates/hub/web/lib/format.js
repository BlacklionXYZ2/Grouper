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
