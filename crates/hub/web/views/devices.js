// Devices: presence, scoped folders, and each device's tool registry with
// Manual / Auto / Bypass. Bypass needs a second confirmation; for a shell or a
// CLI agent's chat tool the person types the tool's name. A tool on Bypass is
// marked in red with a warning icon so it can't be missed.
//
// Presence comes from the app's one device poller (live.js), which these views
// switch to its 15-second cadence while they're open.

import { h, icon, setChildren, keepFocus } from '../lib/dom.js';
import { get, put, qs, enc, isAbort } from '../lib/api.js';
import { store, on, isAdmin } from '../lib/store.js';
import { loadDevices, wantFastDevicePolling } from '../lib/live.js';
import { bar, confirmAction, toast, toastError, skeletonBlocks, errorBanner } from '../lib/ui.js';
import { ago, plural } from '../lib/format.js';
import { renderMarkdown } from '../lib/md.js';

const CONFINED = ['claude-code', 'codex', 'cursor', 'antigravity', 'pi'];
const UNCONFINED = ['hermes', 'grok', 'opencode'];
const SAFETY = {
  codex: 'workspace-write',
  'claude-code': 'acceptEdits',
  antigravity: 'accept-edits',
  cursor: 'its sandbox',
  pi: 'read-only tools',
};

export function renderDevices(container, { id }) {
  return id ? renderDetail(container, id) : renderList(container);
}

function renderList(container) {
  const body = h('div', { class: 'view-body' });
  container.append(
    h(
      'section',
      { class: 'view' },
      bar({
        title: 'Devices',
        caption: 'Presence and tool permissions',
        actions: [h('button', { class: 'icon-btn', type: 'button', 'aria-label': 'Refresh', onclick: () => refresh() }, icon('refresh'))],
      }),
      body,
    ),
  );
  let error = null;

  function render() {
    if (error && !store.devices.length) {
      setChildren(body, errorBanner(error, refresh));
      return;
    }
    if (!store.devicesAt) {
      setChildren(body, skeletonBlocks(3));
      return;
    }
    const devices = store.devices;
    const others = devices.filter((d) => d.id !== 'hub');
    setChildren(
      body,
      !store.online && store.everOnline ? h('div', { class: 'banner warn' }, icon('offline'), `Offline. This list is from ${ago(store.devicesAt / 1000)}.`) : null,
      !others.length
        ? h('div', { class: 'banner info' }, icon('devices'), h('div', { class: 'grow' }, 'No devices yet. On a computer, run ', h('code', null, 'grouper connect'), ' with a device token so threads can run there.'))
        : null,
      h(
        'div',
        { class: 'device-grid' },
        devices.map((d) =>
          h(
            'a',
            { class: 'card device-card', href: `#/devices/${enc(d.id)}` },
            h('div', { class: 'name' }, h('span', { class: ['dot', d.online && 'on'], 'aria-hidden': 'true' }), d.name),
            h('div', { class: 'meta' }, h('span', null, d.online ? 'Online' : `Offline · ${d.last_seen ? `seen ${ago(d.last_seen)}` : 'never seen'}`), h('span', null, d.platform)),
            h('div', { class: 'row-sub' }, d.id === 'hub' ? 'Hub threads have file tools only.' : d.online ? plural(d.capabilities.length, 'tool') : 'Tools show when it connects'),
            d.folders?.length ? h('div', { class: 'folders' }, d.folders.map((f) => h('span', { class: 'badge' }, `@${f.name} · ${f.mode === 'read_write' ? 'read/write' : 'read'}`))) : null,
          ),
        ),
      ),
      h('p', { class: 'hint' }, `Updated ${ago(store.devicesAt / 1000)}. Presence refreshes every 15 seconds here.`),
    );
  }

  async function refresh() {
    try {
      await loadDevices();
      error = null;
    } catch (e) {
      error = e;
    }
    render();
  }

  const offs = [on('devices', render), on('online', render), wantFastDevicePolling()];
  render();
  refresh();
  return () => offs.forEach((off) => off());
}

/** Tools whose Bypass hands a model a whole shell or another agent: type the name to confirm. */
function needsTypedConfirm(tool) {
  return /(^|__)run_shell$/.test(tool) || /_chat$/.test(tool);
}

function policyLine(tool, permission) {
  const server = tool.includes('__') ? tool.split('__')[0] : null;
  if (permission === 'manual') return { text: 'Every call waits for you.', tone: '' };
  if (permission === 'bypass') return { text: 'Every call runs, including deletes and commands outside the workspace.', tone: 'danger' };
  if (server && CONFINED.includes(server)) {
    return {
      text: `Runs unless a risk rule asks. Auto runs ${server} in the thread's workspace with ${SAFETY[server]}; leaving the workspace, asking for a bypass, or a clearly destructive instruction still asks.`,
      tone: '',
    };
  }
  if (server && UNCONFINED.includes(server)) {
    return { text: `Auto can't confine ${server} to the workspace: it has no safety mode, so each chat call asks you first.`, tone: 'warn' };
  }
  return { text: 'Runs unless a risk rule asks.', tone: '' };
}

function renderDetail(container, id) {
  const body = h('div', { class: 'view-body narrow' });
  const barSlot = h('div', { class: 'bar-slot' });
  container.append(h('section', { class: 'view' }, barSlot, body));
  let tools = null;
  let error = null;
  let showUnavailable = false;
  const busy = new Set();

  const device = () => store.devices.find((d) => d.id === id);

  function renderBar() {
    const d = device();
    setChildren(
      barSlot,
      bar({
        title: d?.name || id,
        caption: d ? `${d.online ? 'Online' : 'Offline'} · ${d.platform}` : 'Device',
        back: '#/devices',
      }),
    );
  }

  function render() {
    renderBar();
    const d = device();
    if (error && !tools) {
      setChildren(body, errorBanner(error, refresh));
      return;
    }
    if (!tools) {
      setChildren(body, skeletonBlocks(4));
      return;
    }
    const available = tools.filter((t) => t.available);
    const unavailable = tools.filter((t) => !t.available);
    const admin = isAdmin();
    const writable = admin && store.online;
    const bypassed = available.filter((t) => t.permission === 'bypass');
    keepFocus(body, () => setChildren(
      body,
      bypassed.length
        ? h('div', { class: 'banner bypass-banner', role: 'note' }, icon('warning'), h('div', { class: 'grow' }, h('strong', null, `Bypass is on for ${plural(bypassed.length, 'tool')}: `), bypassed.map((t) => t.name).join(', '), '. Those calls run without asking.'))
        : null,
      !store.online && store.everOnline ? h('div', { class: 'banner warn' }, icon('offline'), 'Offline. Permissions can be changed once the hub reconnects.') : null,
      h(
        'div',
        { class: 'card card-pad section stack' },
        h('div', { class: 'meta' }, d ? h('span', null, d.online ? 'Online now' : d.last_seen ? `Last seen ${ago(d.last_seen)}` : 'Never connected') : null, h('span', { class: 'mono' }, id)),
        id === 'hub' ? h('p', { class: 'hint' }, 'Hub threads have file tools only, until they run in sandbox containers.') : null,
        d?.folders?.length
          ? h('div', null, h('div', { class: 'field-label' }, 'Scoped folders'), h('div', { class: 'folders' }, d.folders.map((f) => h('span', { class: 'badge' }, `@${f.name} · ${f.mode === 'read_write' ? 'read/write' : 'read only'}`))))
          : null,
        h('p', { class: 'hint' }, 'Workspace file tools and the known agent tools start on Auto; everything else starts on Manual. Only you can change these; no model has a tool for it.'),
        !admin ? h('div', { class: 'banner warn' }, icon('shield'), 'This session uses a device token, so permissions are read-only here.') : null,
      ),
      available.length ? h('ul', { class: 'list card' }, available.map((t) => h('li', null, toolItem(t, writable)))) : h('p', { class: 'hint' }, d?.online ? 'This device offers no tools.' : 'No tools known yet. They appear when the device connects.'),
      unavailable.length
        ? h(
            'div',
            { class: 'section' },
            h(
              'label',
              { class: 'switch' },
              h('input', {
                type: 'checkbox',
                checked: showUnavailable,
                onchange: (e) => {
                  showUnavailable = e.target.checked;
                  render();
                },
              }),
              `Show ${plural(unavailable.length, 'unavailable tool')}`,
            ),
            showUnavailable ? h('ul', { class: 'list card' }, unavailable.map((t) => h('li', null, toolItem(t, writable, true)))) : null,
          )
        : null,
    ));
  }

  function toolItem(t, writable, unavailable = false) {
    const policy = policyLine(t.name, t.permission);
    const name = `perm-${t.name}`;
    const bypass = t.permission === 'bypass';
    const option = (value, label) =>
      h(
        'label',
        { class: value === 'bypass' ? 'bypass' : null },
        h('input', {
          type: 'radio',
          name,
          value,
          checked: t.permission === value,
          // Not disabled while a write is in flight: a disabled radio drops focus.
          disabled: !writable,
          'aria-describedby': `${name}-policy`,
          onchange: () => change(t, value),
        }),
        label,
      );
    return h(
      'div',
      { class: ['tool-item', bypass && 'bypass-on'] },
      h(
        'div',
        { class: 'tool-top' },
        h(
          'div',
          null,
          h('div', { class: 'tool-title' }, bypass ? icon('warning', 'Bypass on:') : null, t.name),
          bypass ? h('span', { class: 'badge danger' }, 'Bypass: runs without asking') : null,
          unavailable ? h('span', { class: 'badge muted' }, 'Unavailable') : null,
        ),
        h('div', { class: 'seg', role: 'radiogroup', 'aria-label': `Permission for ${t.name}` }, option('manual', 'Manual'), option('auto', 'Auto'), option('bypass', 'Bypass')),
      ),
      t.description ? h('div', { class: 'row-sub md' }, renderMarkdown(t.description)) : null,
      h('p', { class: ['policy', policy.tone], id: `${name}-policy` }, policy.text),
    );
  }

  /** Put a tool's radios back to its saved level, in place (no rebuild, focus stays). */
  function showLevel(t) {
    for (const r of body.querySelectorAll(`input[name="${CSS.escape(`perm-${t.name}`)}"]`)) r.checked = r.value === t.permission;
  }

  async function change(t, value) {
    const previous = t.permission;
    if (busy.has(t.name)) {
      showLevel(t);
      return;
    }
    if (value === 'bypass') {
      // Until it's confirmed, the row keeps showing the level it has.
      showLevel(t);
      const typed = needsTypedConfirm(t.name);
      const where = device()?.name || id;
      const ok = await confirmAction({
        title: `Always run ${t.name}?`,
        callout: /_chat$/.test(t.name)
          ? `${t.name} hands work to another agent with its own tools. On Bypass that agent runs on ${where} in its own bypass mode, and nothing asks you first.`
          : typed
            ? `${t.name} runs any command on ${where}. On Bypass, a model can run commands there with nothing asking you first.`
            : `Bypass turns off every check for ${t.name} on ${where}.`,
        body: [
          `Every call to ${t.name} on ${device()?.name || id} runs without asking, including deletes and commands outside the workspace.`,
          'Only you can put it back.',
        ],
        confirmLabel: 'Set to Bypass',
        danger: true,
        typeToConfirm: typed ? t.name : null,
      });
      if (!ok) return;
    }
    busy.add(t.name);
    t.permission = value;
    render();
    try {
      await put(`/v1/devices/${enc(id)}/tools/${enc(t.name)}`, { permission: value });
      toast(`${t.name} is now ${value[0].toUpperCase()}${value.slice(1)}.`);
    } catch (err) {
      t.permission = previous;
      toastError(err);
    } finally {
      busy.delete(t.name);
      render();
    }
  }

  // Presence comes from the shared poller; this view re-reads only the tools.
  async function refresh({ withDevices = false } = {}) {
    try {
      const [list] = await Promise.all([get(`/v1/tools${qs({ device: id, all: 'true' })}`, { signal: aborter.signal }), withDevices ? loadDevices() : null]);
      // Keep a tool's optimistic level while its write is in flight.
      tools = (list || []).map((t) => (busy.has(t.name) ? tools?.find((x) => x.name === t.name) || t : t)).sort((a, b) => a.name.localeCompare(b.name));
      error = null;
    } catch (e) {
      if (isAbort(e)) return;
      error = e;
    }
    render();
  }

  const aborter = new AbortController();
  const offs = [on('devices', render), on('online', render), wantFastDevicePolling()];
  const timer = setInterval(() => {
    if (!document.hidden && store.online) refresh();
  }, 15000);
  render();
  refresh({ withDevices: !store.devicesAt });
  return () => {
    offs.forEach((off) => off());
    clearInterval(timer);
    aborter.abort();
  };
}
