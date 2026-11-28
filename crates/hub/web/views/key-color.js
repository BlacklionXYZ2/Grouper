// API key colours (usage-tab-spec 4.4). The hub stores a colour as a name on the
// provider record ("purple"); every client maps the name to its own theme, so
// light, dark and increased-contrast values keep working. The ten names alias the
// family hues in css/usage.css (`--key-<name>`). Used on the Usage page, on
// provider headers in Models and on providers in Settings; thread rows and model
// pickers stay coloured by family.

import { h, icon } from '../lib/dom.js';
import { put, enc } from '../lib/api.js';

/** name -> label. Order is the picker's order. */
export const KEY_COLORS = [
  ['teal', 'Teal'],
  ['purple', 'Purple'],
  ['orange', 'Orange'],
  ['pink', 'Pink'],
  ['blue', 'Blue'],
  ['green', 'Green'],
  ['amber', 'Amber'],
  ['rose', 'Rose'],
  ['indigo', 'Indigo'],
  ['cyan', 'Cyan'],
];

const NAMES = new Set(KEY_COLORS.map(([n]) => n));

export const isKeyColor = (name) => NAMES.has(name);

/** Class that sets `--key-c` for a colour name; anything else is the neutral grey. */
export function keyClass(name) {
  return `key-c-${NAMES.has(name) ? name : 'unknown'}`;
}

export function keyColorLabel(name) {
  return KEY_COLORS.find(([n]) => n === name)?.[1] || 'No colour';
}

/**
 * A key's swatch: a 12px disc in its colour. `ring` draws an outline ring instead
 * (a key with nothing to show in the donut). Decorative: the name is always text.
 */
export function keySwatch(color, { ring = false } = {}) {
  return h('span', { class: ['key-swatch', keyClass(color), ring && 'is-ring'], 'aria-hidden': 'true' });
}

/** Save a colour (`null` re-runs the hub's auto-assignment). Resolves with the updated provider. */
export function setProviderColor(providerId, color) {
  return put(`/v1/providers/${enc(providerId)}/color`, { color });
}

/**
 * The picker: a radio group of the ten colours, each with its name as text, and under
 * a colour another key already uses that key's name. `others` maps colour name -> key ids
 * (without this key). `onPick(name)` saves at once; the caller redraws.
 */
export function colorPicker({ providerId, current, others = new Map(), onPick, disabled = false, idPrefix = 'kc' }) {
  const group = h('div', { class: 'key-picker', role: 'radiogroup', 'aria-label': `Colour for ${providerId}` });
  const buttons = KEY_COLORS.map(([name, label]) => {
    const usedBy = others.get(name) || [];
    const selected = current === name;
    const btn = h(
      'button',
      {
        class: ['key-pick', keyClass(name), selected && 'is-selected'],
        type: 'button',
        role: 'radio',
        'aria-checked': selected ? 'true' : 'false',
        tabindex: selected || (!current && name === 'teal') ? '0' : '-1',
        disabled,
        title: usedBy.length ? `${label} (also used by ${usedBy.join(', ')})` : label,
        id: `${idPrefix}-${name}`,
        'data-color': name,
        onclick: () => onPick(name),
      },
      h('span', { class: 'key-pick-disc', 'aria-hidden': 'true' }, selected ? icon('check') : null),
      h('span', { class: 'key-pick-name' }, label),
      usedBy.length ? h('span', { class: 'key-pick-used' }, usedBy.length === 1 ? usedBy[0] : `${usedBy.length} keys`) : null,
    );
    return btn;
  });
  // Arrow keys move between the swatches (a radio group's behaviour); Space/Enter pick.
  group.addEventListener('keydown', (e) => {
    const step = { ArrowRight: 1, ArrowDown: 1, ArrowLeft: -1, ArrowUp: -1 }[e.key];
    if (!step) return;
    const i = buttons.indexOf(document.activeElement);
    if (i < 0) return;
    e.preventDefault();
    const next = buttons[(i + step + buttons.length) % buttons.length];
    for (const b of buttons) b.tabIndex = b === next ? 0 : -1;
    next.focus();
  });
  group.append(...buttons);
  return group;
}

/** colour name -> ids of the keys using it (excluding `exceptId`). */
export function colorUse(providers, exceptId) {
  const map = new Map();
  for (const p of providers || []) {
    if (!p.color || p.id === exceptId) continue;
    if (!map.has(p.color)) map.set(p.color, []);
    map.get(p.color).push(p.id);
  }
  return map;
}
