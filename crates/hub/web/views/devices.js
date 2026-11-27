// Devices: Hub card, Change hub… picker, move confirmation & live progress,
// replaced/stepped-down states, device presence, scoped folders, and each
// device's tool registry with Manual / Auto / Bypass.
//
// Presence and move progress update live from the hub's event stream (live.js).

import { h, icon, setChildren, keepFocus } from '../lib/dom.js';
import { get, post, put, del, qs, enc, describe, isAbort } from '../lib/api.js';
import { store, on, isAdmin } from '../lib/store.js';
import { loadDevices } from '../lib/live.js';
import { bar, sheet, confirmAction, toast, toastError, announce, skeleton, skeletonBlocks, errorBanner } from '../lib/ui.js';
import { ago, date, plural } from '../lib/format.js';
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

const KNOWN_HUB_KEY = `grouper.knownHub.${location.host}`;
const PENDING_MOVE_KEY = 'grouper.pendingHubMove';
const PENDING_MOVE_MAX_AGE_MS = 5 * 60 * 1000;
const MOVE_POLL_MS = 1500;

function platformLabel(platform) {
  switch ((platform || '').toLowerCase()) {
    case 'windows':
      return 'Windows';
    case 'macos':
      return 'macOS';
    case 'linux':
      return 'Linux';
    case 'ios':
      return 'iPhone';
    case 'web':
      return 'Browser';
    case 'cli':
      return 'CLI';
    default:
      return platform || 'Unknown';
  }
}

function formatBytes(bytes) {
  if (bytes === null || bytes === undefined || Number.isNaN(bytes)) return '—';
  if (bytes < 1024) return `${bytes} B`;
  const units = ['KB', 'MB', 'GB', 'TB'];
  let val = bytes / 1024;
  let idx = 0;
  while (val >= 1024 && idx < units.length - 1) {
    val /= 1024;
    idx++;
  }
  return `${val >= 10 ? Math.round(val) : val.toFixed(1)} ${units[idx]}`;
}

function estimateMoveTime(bytes) {
  if (!bytes || bytes < 10 * 1024 * 1024) return 'Usually under 30 seconds';
  if (bytes < 100 * 1024 * 1024) return 'Usually under a minute';
  return 'A few minutes';
}

function readJsonStorage(storage, key) {
  try {
    const raw = storage.getItem(key);
    return raw ? JSON.parse(raw) : null;
  } catch {
    return null;
  }
}

function writeJsonStorage(storage, key, value) {
  try {
    if (value === null) storage.removeItem(key);
    else storage.setItem(key, JSON.stringify(value));
  } catch {
    /* storage unavailable */
  }
}

function readPendingMove() {
  const pending = readJsonStorage(sessionStorage, PENDING_MOVE_KEY);
  if (!pending || !pending.ts || Date.now() - pending.ts > PENDING_MOVE_MAX_AGE_MS) {
    writeJsonStorage(sessionStorage, PENDING_MOVE_KEY, null);
    return null;
  }
  return pending;
}

function isActiveMoveState(state) {
  return state === 'preparing' || state === 'copying' || state === 'switching';
}

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
        caption: 'Your hub, presence, and tool permissions',
        actions: [h('button', { class: 'icon-btn', type: 'button', 'aria-label': 'Refresh', onclick: () => refresh() }, icon('refresh'))],
      }),
      body,
    ),
  );

  let error = null;
  // The Google account every device here belongs to (admin sessions can read it).
  let owner = null;
  let hubInfo = store.hub || null;
  let installsData = null;
  let moveStatus = null;
  let tunnelInfo = null;
  let replacedNotice = null;
  let dismissedMoveId = null;
  let movePollError = false;
  let movePollTimer = null;
  let cancelBusy = false;
  let lastAnnouncedPhase = null;
  let pendingMove = readPendingMove();

  const aborter = new AbortController();
  const { signal } = aborter;

  function clearMovePoll() {
    if (movePollTimer) {
      clearTimeout(movePollTimer);
      movePollTimer = null;
    }
  }

  function scheduleMovePoll() {
    clearMovePoll();
    if (signal.aborted) return;
    if (!isActiveMoveState(moveStatus?.state)) return;
    movePollTimer = setTimeout(pollMoveState, MOVE_POLL_MS);
  }

  function updateHubIdentityCache(info, installs) {
    if (!info?.hub_id) return;
    const prev = readJsonStorage(localStorage, KNOWN_HUB_KEY);
    const hubInst = installs?.installs?.find((i) => i.is_hub);
    const currentRecord = {
      hub_id: info.hub_id,
      hub_device_id: info.hub_device_id || null,
      epoch: info.epoch || 1,
      name: hubInst?.name || null,
      seen_at: Math.floor(Date.now() / 1000),
    };
    if (prev?.hub_id && prev.hub_id !== info.hub_id) {
      replacedNotice = {
        old_hub_id: prev.hub_id,
        old_name: prev.name,
        replaced_at: currentRecord.seen_at,
      };
      return;
    }
    if (prev && (prev.hub_device_id !== currentRecord.hub_device_id || (currentRecord.epoch || 0) > (prev.epoch || 0))) {
      // Hub moved cleanly to another device under the same hub_id.
      if (prev.hub_device_id && currentRecord.hub_device_id && prev.hub_device_id !== currentRecord.hub_device_id) {
        const label = currentRecord.name || 'the new device';
        toast(`Hub moved to ${label}.`);
        announce(`Hub moved to ${label}.`, { force: true });
      }
    }
    writeJsonStorage(localStorage, KNOWN_HUB_KEY, currentRecord);
  }

  async function pollMoveState() {
    if (signal.aborted) return;
    try {
      const [mv, hub] = await Promise.all([
        get('/v1/hub/move', { signal }),
        get('/v1/hub', { signal }).catch(() => null),
      ]);
      if (signal.aborted) return;
      movePollError = false;
      if (hub) {
        hubInfo = hub;
        store.hub = hub;
      }
      applyMoveStatus(mv);
      if (hub && !isActiveMoveState(mv?.state)) {
        await Promise.allSettled([loadInstalls(), loadDevices()]);
        updateHubIdentityCache(hubInfo, installsData);
      }
    } catch (err) {
      if (isAbort(err)) return;
      // During Switching, the old hub stops and the new hub starts up: keep polling.
      movePollError = true;
    }
    if (!signal.aborted) {
      render();
      if (isActiveMoveState(moveStatus?.state)) scheduleMovePoll();
    }
  }

  function applyMoveStatus(next) {
    if (!next) return;
    const prevState = moveStatus?.state;
    moveStatus = next;
    if (next.state && next.state !== prevState) {
      const phaseKey = `${next.move_id || ''}:${next.state}`;
      if (phaseKey !== lastAnnouncedPhase) {
        lastAnnouncedPhase = phaseKey;
        if (next.state === 'preparing') announce(`Preparing hub move to ${next.target_name || 'target device'}.`);
        else if (next.state === 'copying') announce(`Copying hub data to ${next.target_name || 'target device'}.`);
        else if (next.state === 'switching') announce(`Switching hub to ${next.target_name || 'target device'}.`);
        else if (next.state === 'done') announce(next.message || 'Hub move complete.', { force: true });
        else if (next.state === 'rolled_back') announce(next.error || next.message || 'Hub move rolled back.', { force: true });
      }
    }
    if (isActiveMoveState(next.state)) {
      scheduleMovePoll();
    } else {
      clearMovePoll();
      if (next.state === 'done' || next.state === 'rolled_back') {
        loadHubAndInstalls();
        loadDevices().catch(() => {});
      }
    }
  }

  async function loadInstalls() {
    installsData = await get('/v1/devices/installs', { signal });
    return installsData;
  }

  async function loadHubAndInstalls() {
    const [hubRes, installsRes, moveRes, tunnelRes] = await Promise.allSettled([
      get('/v1/hub', { signal }),
      loadInstalls(),
      get('/v1/hub/move', { signal }),
      isAdmin() ? get('/v1/tunnel', { signal }) : Promise.resolve(null),
    ]);
    if (signal.aborted) return;
    if (hubRes.status === 'fulfilled' && hubRes.value) {
      hubInfo = hubRes.value;
      store.hub = hubRes.value;
    }
    if (installsRes.status === 'fulfilled') {
      installsData = installsRes.value;
    }
    if (moveRes.status === 'fulfilled' && moveRes.value) {
      applyMoveStatus(moveRes.value);
    }
    if (tunnelRes.status === 'fulfilled') {
      tunnelInfo = tunnelRes.value;
    }
    updateHubIdentityCache(hubInfo, installsData);
  }

  if (isAdmin()) {
    get('/v1/auth/google/link', { signal })
      .then((res) => {
        owner = res?.owner || null;
        if (owner && !signal.aborted) render();
      })
      .catch(() => {
        /* no owner line; the list still works */
      });
  }

  async function cancelActiveMove() {
    if (cancelBusy) return;
    cancelBusy = true;
    render();
    try {
      const res = await del('/v1/hub/move');
      if (signal.aborted) return;
      toast('Cancelling hub move…');
      if (res) applyMoveStatus(res);
      await pollMoveState();
    } catch (err) {
      toastError(err);
    } finally {
      cancelBusy = false;
      if (!signal.aborted) render();
    }
  }

  async function signInAgainForReplacedHub() {
    try {
      await del('/v1/session');
    } catch {
      /* session may already be gone on the new hub */
    }
    location.hash = '';
    location.reload();
  }

  function dismissReplacedNotice() {
    if (hubInfo?.hub_id) {
      const hubInst = installsData?.installs?.find((i) => i.is_hub);
      writeJsonStorage(localStorage, KNOWN_HUB_KEY, {
        hub_id: hubInfo.hub_id,
        hub_device_id: hubInfo.hub_device_id || null,
        epoch: hubInfo.epoch || 1,
        name: hubInst?.name || null,
        seen_at: Math.floor(Date.now() / 1000),
      });
    }
    replacedNotice = null;
    render();
  }

  function renderHubCard() {
    const admin = isAdmin();
    const state = moveStatus?.state;

    // 1. Replaced hub_id at this address (section 4a Recovery)
    if (replacedNotice) {
      return h(
        'section',
        { class: 'hub-card is-replaced', role: 'alert', 'aria-labelledby': 'hub-card-title' },
        h(
          'div',
          { class: 'hub-card-top' },
          h(
            'div',
            { class: 'hub-card-main' },
            h('span', { class: 'hub-card-ico' }, icon('warning')),
            h(
              'div',
              { class: 'hub-card-body' },
              h('h2', { class: 'hub-card-title', id: 'hub-card-title' }, `Your hub was replaced on ${date(replacedNotice.replaced_at)}. Sign in again.`, h('span', { class: 'badge warn' }, 'Replaced')),
              h('p', { class: 'row-sub' }, 'This address now serves a different hub. Cached chats remain readable, marked as from the old hub.'),
              hubInfo?.hub_id ? h('div', { class: 'meta' }, h('span', { class: 'mono' }, `New hub ${hubInfo.hub_id}`), hubInfo.epoch ? h('span', null, `Epoch ${hubInfo.epoch}`) : null) : null,
            ),
          ),
          h(
            'div',
            { class: 'hub-card-actions' },
            h('button', { id: 'hub-replaced-signin-btn', class: 'btn btn-primary', type: 'button', onclick: signInAgainForReplacedHub }, 'Sign in again'),
            h('button', { id: 'hub-replaced-dismiss-btn', class: 'btn', type: 'button', onclick: dismissReplacedNotice }, 'Dismiss'),
          ),
        ),
      );
    }

    // 2. Active hub move (preparing / copying / switching)
    if (isActiveMoveState(state)) {
      const targetName = moveStatus.target_name || moveStatus.target_device_id || 'target computer';
      const pct = typeof moveStatus.progress_pct === 'number' ? moveStatus.progress_pct : null;
      const phases = [
        ['preparing', 'Preparing'],
        ['copying', 'Copying'],
        ['switching', 'Switching'],
      ];
      const activeIdx = phases.findIndex(([k]) => k === state);
      const canCancel = admin && (state === 'preparing' || state === 'copying');
      const progressAttrs = {
        class: 'hub-progress',
        max: 100,
        'aria-label': `Moving hub to ${targetName}`,
      };
      if (state !== 'preparing' && pct !== null) progressAttrs.value = pct;

      return h(
        'section',
        { class: 'hub-card is-moving', 'aria-labelledby': 'hub-card-title' },
        h(
          'div',
          { class: 'hub-card-top' },
          h(
            'div',
            { class: 'hub-card-main' },
            h('span', { class: 'hub-card-ico' }, icon('hub')),
            h(
              'div',
              { class: 'hub-card-body' },
              h(
                'h2',
                { class: 'hub-card-title', id: 'hub-card-title' },
                moveStatus.message || `Moving your hub to ${targetName}…`,
                h('span', { class: 'badge hub-badge' }, 'Moving hub'),
              ),
              h('p', { class: 'row-sub' }, 'Grouper pauses chats, threads, and settings writes for everyone during the move.'),
            ),
          ),
          admin
            ? h(
                'div',
                { class: 'hub-card-actions' },
                h(
                  'button',
                  {
                    id: 'cancel-hub-move-btn',
                    class: 'btn',
                    type: 'button',
                    disabled: !canCancel || cancelBusy,
                    onclick: cancelActiveMove,
                  },
                  cancelBusy ? 'Cancelling…' : 'Cancel move',
                ),
              )
            : null,
        ),
        h(
          'ol',
          { class: 'hub-move-steps', 'aria-label': 'Move stages' },
          phases.map(([key, label], idx) => {
            const done = idx < activeIdx;
            const active = idx === activeIdx;
            return h(
              'li',
              {
                class: ['hub-move-step', done && 'is-done', active && 'is-active'],
                'aria-current': active ? 'step' : null,
              },
              icon(done ? 'check' : active ? 'clock' : 'dot'),
              label,
            );
          }),
        ),
        h(
          'div',
          { class: 'hub-progress-wrap' },
          h('progress', progressAttrs),
          h(
            'div',
            { class: 'hub-progress-row' },
            h(
              'span',
              null,
              movePollError && state === 'switching'
                ? 'Waiting for the new hub to answer at this address…'
                : state === 'switching'
                  ? 'Cannot cancel once switching has started.'
                  : moveStatus.snapshot_bytes
                    ? `Snapshot ${formatBytes(moveStatus.snapshot_bytes)}`
                    : 'Preparing snapshot…',
            ),
            pct !== null ? h('span', { class: 'hub-progress-pct' }, `${pct}%`) : null,
          ),
        ),
      );
    }

    // 3. Stepped-down / moved away from this hub (MovePhase::Done or tunnel stepdown)
    if (state === 'done' || tunnelInfo?.stepped_down) {
      const sd = tunnelInfo?.stepdown;
      const targetName = moveStatus?.target_name || sd?.peer_hub_device_id || 'another device';
      const newEpoch = moveStatus?.new_epoch || sd?.peer_epoch;
      return h(
        'section',
        { class: 'hub-card is-replaced', 'aria-labelledby': 'hub-card-title' },
        h(
          'div',
          { class: 'hub-card-top' },
          h(
            'div',
            { class: 'hub-card-main' },
            h('span', { class: 'hub-card-ico' }, icon('hub')),
            h(
              'div',
              { class: 'hub-card-body' },
              h(
                'h2',
                { class: 'hub-card-title', id: 'hub-card-title' },
                moveStatus?.message || `Hub moved to ${targetName}`,
                h('span', { class: 'badge warn' }, 'Stepped down'),
              ),
              h(
                'p',
                { class: 'row-sub' },
                'This machine is no longer the active hub and now runs as an ordinary device.',
              ),
              h(
                'div',
                { class: 'meta' },
                newEpoch ? h('span', null, `Epoch ${newEpoch}`) : null,
                moveStatus?.pre_move_backup ? h('span', { class: 'mono' }, `Old copy retained as ${moveStatus.pre_move_backup} (7 days)`) : null,
                sd?.address ? h('span', { class: 'mono' }, sd.address) : null,
              ),
            ),
          ),
          h(
            'div',
            { class: 'hub-card-actions' },
            h('button', { class: 'btn btn-primary', type: 'button', onclick: () => location.reload() }, icon('refresh'), 'Reload'),
          ),
        ),
      );
    }

    // 4. Normal Hub card (plus optional rolled-back banner if recent move failed)
    const hubInstall = installsData?.installs?.find((i) => i.is_hub);
    const hubDevice = store.devices.find((d) => d.id === hubInfo?.hub_device_id || d.id === 'hub');
    const hubName = hubInstall?.name || hubDevice?.name || 'Hub';
    const hubPlatform = hubInstall?.platform || hubDevice?.platform;
    const hubOnline = hubInstall ? hubInstall.online : store.online;
    const hubSince = installsData?.hub_since;
    const epoch = installsData?.epoch || hubInfo?.epoch;
    const hubVer = installsData?.hub_version || hubInfo?.hub_version;
    const showRolledBack = state === 'rolled_back' && moveStatus?.move_id !== dismissedMoveId;

    return h(
      'div',
      null,
      showRolledBack
        ? h(
            'div',
            { class: 'banner error', role: 'alert' },
            icon('warning'),
            h(
              'div',
              { class: 'grow' },
              h('strong', null, moveStatus.message || 'Hub move rolled back.'),
              moveStatus.error ? ` ${moveStatus.error}` : '',
            ),
            h(
              'button',
              {
                class: 'btn btn-sm',
                type: 'button',
                onclick: () => {
                  dismissedMoveId = moveStatus.move_id;
                  render();
                },
              },
              'Dismiss',
            ),
          )
        : null,
      h(
        'section',
        { class: 'hub-card', 'aria-labelledby': 'hub-card-title' },
        h(
          'div',
          { class: 'hub-card-top' },
          h(
            'div',
            { class: 'hub-card-main' },
            h('span', { class: 'hub-card-ico' }, icon('hub')),
            h(
              'div',
              { class: 'hub-card-body' },
              h(
                'h2',
                { class: 'hub-card-title', id: 'hub-card-title' },
                hubName,
                h('span', { class: 'badge hub-badge' }, 'Hub'),
                h(
                  'span',
                  { class: ['badge', hubOnline ? 'ok' : 'warn'] },
                  icon(hubOnline ? 'check' : 'offline'),
                  hubOnline ? 'Online' : 'Offline',
                ),
              ),
              h(
                'div',
                { class: 'meta' },
                hubPlatform ? h('span', null, platformLabel(hubPlatform)) : null,
                hubSince ? h('span', null, `Hub since ${date(hubSince)}`) : null,
                epoch ? h('span', null, `Epoch ${epoch}`) : null,
                hubVer ? h('span', null, `v${hubVer}`) : null,
              ),
            ),
          ),
          admin
            ? h(
                'div',
                { class: 'hub-card-actions' },
                h(
                  'button',
                  {
                    id: 'change-hub-btn',
                    class: 'btn btn-primary',
                    type: 'button',
                    disabled: !store.online && store.everOnline,
                    onclick: () => openChangeHubSheet(),
                  },
                  'Change hub…',
                ),
              )
            : null,
        ),
      ),
    );
  }

  function renderPendingResumeBanner() {
    if (!pendingMove || !isAdmin() || isActiveMoveState(moveStatus?.state)) return null;
    return h(
      'div',
      { class: 'banner info', role: 'status' },
      icon('shield'),
      h(
        'div',
        { class: 'grow' },
        'Google sign-in refreshed. Ready to move your hub to ',
        h('strong', null, pendingMove.target_name || pendingMove.target_device),
        '.',
      ),
      h(
        'button',
        {
          class: 'btn btn-sm btn-primary',
          type: 'button',
          onclick: () => resumePendingMove(),
        },
        'Continue…',
      ),
      h(
        'button',
        {
          class: 'btn btn-sm',
          type: 'button',
          onclick: () => {
            writeJsonStorage(sessionStorage, PENDING_MOVE_KEY, null);
            pendingMove = null;
            render();
          },
        },
        'Dismiss',
      ),
    );
  }

  async function resumePendingMove() {
    if (!pendingMove) return;
    try {
      const data = installsData || (await loadInstalls());
      const target = data?.installs?.find(
        (i) => i.device_id === pendingMove.target_device || i.node_device_id === pendingMove.target_device,
      );
      if (!target || !target.selectable) {
        toast('That device is no longer selectable as the hub.', 'error');
        writeJsonStorage(sessionStorage, PENDING_MOVE_KEY, null);
        pendingMove = null;
        render();
        return;
      }
      openMoveConfirmSheet(target, data, { initialThreads: pendingMove.threads || 'wait', reauthed: true });
    } catch (err) {
      toastError(err);
    }
  }

  async function openChangeHubSheet() {
    const picked = await sheet((close) => {
      const listBody = h('div', { class: 'stack' }, skeleton(4));

      async function fetchAndRender() {
        setChildren(listBody, skeleton(4));
        try {
          const data = await get('/v1/devices/installs', { signal });
          installsData = data;
          const installs = data?.installs || [];
          const eligible = installs.filter((i) => i.is_hub || i.eligible);
          const ineligible = installs.filter((i) => !i.is_hub && !i.eligible);

          setChildren(
            listBody,
            eligible.length
              ? h(
                  'ul',
                  { class: 'list card hub-pick-list' },
                  eligible.map((item) => {
                    if (item.is_hub) {
                      return h(
                        'li',
                        { class: 'list-row hub-pick-row is-current' },
                        h(
                          'div',
                          { class: 'grow' },
                          h('div', { class: 'row-title' }, `${item.name} · ${platformLabel(item.platform)} · Hub`),
                          h('div', { class: 'meta' }, h('span', null, item.online ? 'Online' : 'Offline')),
                        ),
                        h('span', { class: 'hub-pick-check' }, icon('check', 'Current hub')),
                        h('span', { class: 'badge accent' }, 'Current hub'),
                      );
                    }
                    if (item.selectable) {
                      return h(
                        'li',
                        null,
                        h(
                          'button',
                          {
                            type: 'button',
                            class: 'list-row hub-pick-row is-selectable',
                            onclick: () => close({ target: item, data }),
                          },
                          h(
                            'div',
                            { class: 'grow' },
                            h('div', { class: 'row-title row-badges' }, h('span', { class: 'dot on', 'aria-hidden': 'true' }), item.name, h('span', { class: 'badge ok' }, 'Online')),
                            h(
                              'div',
                              { class: 'meta' },
                              h('span', null, platformLabel(item.platform)),
                              item.host?.app_version ? h('span', null, `v${item.host.app_version}`) : null,
                            ),
                            item.warnings?.length
                              ? item.warnings.map((w) => h('div', { class: 'hub-pick-warn' }, icon('warning'), h('span', null, w)))
                              : null,
                          ),
                          icon('chevron'),
                        ),
                      );
                    }
                    const disabledNote =
                      item.reason === 'older_version'
                        ? `Update Grouper on this device first${item.host?.app_version ? ` (running v${item.host.app_version}, hub is v${data.hub_version})` : ''}.`
                        : `Offline · ${item.last_seen ? `last seen ${ago(item.last_seen)}. ` : ''}Turn it on to move the hub there.`;
                    return h(
                      'li',
                      { class: 'list-row hub-pick-row is-disabled', 'aria-disabled': 'true' },
                      h(
                        'div',
                        { class: 'grow' },
                        h('div', { class: 'row-title row-badges' }, h('span', { class: 'dot', 'aria-hidden': 'true' }), item.name, h('span', { class: 'badge muted' }, item.reason === 'older_version' ? 'Update needed' : 'Offline')),
                        h('div', { class: 'meta' }, h('span', null, platformLabel(item.platform))),
                        h('p', { class: 'row-sub' }, disabledNote),
                      ),
                    );
                  }),
                )
              : h('p', { class: 'hint' }, 'No eligible computers found.'),
            ineligible.length
              ? h(
                  'div',
                  { class: 'stack' },
                  h('h3', { class: 'sub-title hub-ineligible-title' }, 'Can’t be the hub'),
                  h(
                    'ul',
                    { class: 'list card hub-ineligible-list' },
                    ineligible.map((item) =>
                      h(
                        'li',
                        { class: 'list-row hub-ineligible-row' },
                        h(
                          'div',
                          { class: 'grow' },
                          h('div', { class: 'row-title' }, item.name),
                          h('div', { class: 'meta' }, h('span', null, platformLabel(item.platform)), h('span', null, item.online ? 'Online' : 'Offline')),
                        ),
                        h('span', { class: 'badge muted' }, 'Can’t host'),
                      ),
                    ),
                  ),
                )
              : null,
          );
        } catch (err) {
          if (isAbort(err)) return;
          setChildren(listBody, errorBanner(err, fetchAndRender));
        }
      }

      fetchAndRender();
      return {
        title: 'Change hub',
        caption: 'Pick an online computer running Grouper to become your hub.',
        body: [listBody],
        foot: [h('button', { class: 'btn', type: 'button', onclick: () => close(null) }, 'Close')],
      };
    });

    if (picked?.target) {
      openMoveConfirmSheet(picked.target, picked.data);
    }
  }

  async function openMoveConfirmSheet(target, data, { initialThreads = 'wait', reauthed = false } = {}) {
    let authStatus = { google_web: false };
    try {
      authStatus = (await get('/v1/auth/status', { signal })) || authStatus;
    } catch {
      /* fallback when auth status check fails */
    }
    if (signal.aborted) return;

    const runningHubThreads = [...store.threads.values()].filter((t) => t.device_id === 'hub' && t.status === 'running');
    let selectedThreads = initialThreads === 'stop' ? 'stop' : 'wait';
    const snapshotBytes = data?.estimated_snapshot_bytes ?? 0;
    const isFreshGoogle = reauthed || (pendingMove?.target_device === target.device_id && pendingMove?.reauthed);

    const result = await sheet((close) => {
      const errBox = h('div', { class: 'banner error', role: 'alert', hidden: true });
      const moveBtn = h(
        'button',
        { class: 'btn btn-primary', type: 'button' },
        `Move hub to ${target.name}`,
      );
      const googleReauthBtn = h(
        'button',
        { class: 'btn', type: 'button' },
        icon('shield'),
        isFreshGoogle ? 'Sign in with Google again' : 'Sign in with Google again to confirm',
      );
      let busy = false;

      function showErr(message) {
        errBox.replaceChildren(icon('warning'), h('div', { class: 'grow' }, message));
        errBox.hidden = false;
      }

      googleReauthBtn.addEventListener('click', async () => {
        if (busy) return;
        busy = true;
        googleReauthBtn.disabled = true;
        moveBtn.disabled = true;
        errBox.hidden = true;
        writeJsonStorage(sessionStorage, PENDING_MOVE_KEY, {
          target_device: target.device_id,
          target_name: target.name,
          threads: selectedThreads,
          ts: Date.now(),
          reauthed: true,
        });
        try {
          const res = await post('/v1/auth/google/start', {});
          if (!res?.auth_url) throw new Error('The hub did not return a Google sign-in address.');
          location.assign(res.auth_url);
        } catch (err) {
          writeJsonStorage(sessionStorage, PENDING_MOVE_KEY, null);
          showErr(describe(err));
          busy = false;
          googleReauthBtn.disabled = false;
          moveBtn.disabled = false;
        }
      });

      moveBtn.addEventListener('click', async () => {
        if (busy) return;
        busy = true;
        moveBtn.disabled = true;
        googleReauthBtn.disabled = true;
        errBox.hidden = true;
        try {
          const payload = {
            target_device: target.device_id,
            threads: selectedThreads,
          };
          const status = await post('/v1/hub/move', payload);
          writeJsonStorage(sessionStorage, PENDING_MOVE_KEY, null);
          pendingMove = null;
          close({ started: status });
        } catch (err) {
          showErr(describe(err));
          busy = false;
          moveBtn.disabled = false;
          googleReauthBtn.disabled = false;
        }
      });

      const bodyParts = [
        errBox,
        h(
          'div',
          { class: 'callout stack' },
          h('div', null, h('strong', null, 'What moves: '), 'Conversations, threads, workspaces, memories, skills, device list, provider keys, and remote-access settings.'),
          h(
            'div',
            { class: 'meta' },
            h('span', null, `Estimated snapshot: ${formatBytes(snapshotBytes)}`),
            h('span', null, estimateMoveTime(snapshotBytes)),
          ),
        ),
        h(
          'div',
          { class: 'callout callout-icon warn' },
          icon('warning'),
          h('div', null, 'Grouper pauses for everyone during the move. New chats, threads, and settings changes wait until the new hub takes over.'),
        ),
      ];

      if (target.warnings?.length) {
        for (const w of target.warnings) {
          bodyParts.push(h('div', { class: 'callout callout-icon warn' }, icon('warning'), h('div', null, w)));
        }
      }

      if (runningHubThreads.length) {
        const waitId = `move-threads-wait-${Math.random().toString(36).slice(2, 7)}`;
        const stopId = `move-threads-stop-${Math.random().toString(36).slice(2, 7)}`;
        bodyParts.push(
          h(
            'fieldset',
            { class: 'hub-threads-choice' },
            h('legend', null, `${plural(runningHubThreads.length, 'hub thread')} currently running`),
            h(
              'label',
              { class: 'hub-radio-opt', for: waitId },
              h('input', {
                id: waitId,
                type: 'radio',
                name: 'move_threads',
                value: 'wait',
                checked: selectedThreads === 'wait',
                onchange: () => {
                  selectedThreads = 'wait';
                },
              }),
              h('span', null, h('span', { class: 'hub-radio-title' }, 'Wait for them'), h('span', { class: 'hint' }, 'Finish running hub threads before copying data.')),
            ),
            h(
              'label',
              { class: 'hub-radio-opt', for: stopId },
              h('input', {
                id: stopId,
                type: 'radio',
                name: 'move_threads',
                value: 'stop',
                checked: selectedThreads === 'stop',
                onchange: () => {
                  selectedThreads = 'stop';
                },
              }),
              h('span', null, h('span', { class: 'hub-radio-title' }, 'Stop them and move now'), h('span', { class: 'hint' }, 'Stop running hub threads immediately (“Stopped: the hub moved”).')),
            ),
          ),
        );
      } else {
        bodyParts.push(h('p', { class: 'hint' }, 'No hub threads are running right now. Device threads keep running and reconnect to the new hub.'));
      }

      if (authStatus?.google_web) {
        bodyParts.push(
          h(
            'div',
            { class: 'hub-reauth-box' },
            h(
              'div',
              { class: 'hub-reauth-head' },
              h('span', null, 'Fresh Google sign-in check'),
              isFreshGoogle ? h('span', { class: 'badge ok' }, icon('check'), 'Google sign-in refreshed') : null,
            ),
            h('p', { class: 'hint' }, 'Moving the hub requires a Google sign-in within the last 5 minutes so an unlocked session can’t reassign it.'),
            h('div', null, googleReauthBtn),
          ),
        );
      }

      return {
        title: `Move hub to ${target.name}?`,
        caption: `${platformLabel(target.platform)} · Online`,
        body: bodyParts,
        foot: [
          h('button', { class: 'btn', type: 'button', onclick: () => close({ back: true }) }, 'Back'),
          h('button', { class: 'btn', type: 'button', onclick: () => close(null) }, 'Cancel'),
          moveBtn,
        ],
      };
    });

    if (result?.back) {
      openChangeHubSheet();
      return;
    }
    if (result?.started) {
      applyMoveStatus(result.started);
      render();
    }
  }

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
    keepFocus(body, () =>
      setChildren(
        body,
        owner?.email ? h('p', { class: 'devices-owner' }, icon('shield'), h('span', null, 'Linked to ', h('strong', null, owner.email))) : null,
        !store.online && store.everOnline ? h('div', { class: 'banner warn' }, icon('offline'), `Offline. This list is from ${ago(store.devicesAt / 1000)}.`) : null,
        renderPendingResumeBanner(),
        renderHubCard(),
        !others.length
          ? h('div', { class: 'banner info' }, icon('devices'), h('div', { class: 'grow' }, 'No devices yet. On a computer, run ', h('code', null, 'grouper login'), ' and then ', h('code', null, 'grouper connect'), ' so threads can run there.'))
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
        h('p', { class: 'hint' }, `Updated ${ago(store.devicesAt / 1000)}. Live presence updates automatically.`),
      ),
    );
  }

  async function refresh() {
    try {
      await Promise.all([loadDevices(), loadHubAndInstalls()]);
      error = null;
    } catch (e) {
      if (isAbort(e)) return;
      error = e;
    }
    if (!signal.aborted) render();
  }

  const offs = [
    on('devices', render),
    on('online', (online) => {
      render();
      if (online) loadHubAndInstalls().then(() => { if (!signal.aborted) render(); }).catch(() => {});
    }),
    on('event', (e) => {
      if (e?.kind === 'hub_move_progress') {
        applyMoveStatus({
          move_id: e.move_id,
          state: e.state,
          target_device_id: e.target_device_id,
          target_name: e.target_name,
          progress_pct: e.progress_pct,
          message: e.message,
          error: e.error,
          snapshot_bytes: moveStatus?.snapshot_bytes ?? installsData?.estimated_snapshot_bytes ?? null,
        });
        render();
      }
    }),
  ];
  render();
  refresh();
  return () => {
    offs.forEach((off) => off());
    clearMovePoll();
    aborter.abort();
  };
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
        value === 'bypass' && bypass ? icon('warning') : null,
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
  const offs = [
    on('devices', render),
    on('device_presence', (e) => {
      if (e.device_id === id && store.online) refresh();
    }),
    on('online', (online) => {
      render();
      if (online) refresh();
    }),
  ];
  render();
  refresh({ withDevices: !store.devicesAt });
  return () => {
    offs.forEach((off) => off());
    aborter.abort();
  };
}
