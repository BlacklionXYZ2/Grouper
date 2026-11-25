// Small DOM helpers. Everything from the hub is set as text (textContent or
// attributes), never parsed as HTML.

const SVG_NS = 'http://www.w3.org/2000/svg';

/**
 * h('button', {class: 'x', onclick: fn, 'aria-label': 'Close'}, 'text', child)
 * Attributes starting with "on" become listeners; `class` accepts an array;
 * booleans toggle attributes; null/undefined/false are skipped.
 */
export function h(tag, attrs, ...children) {
  const el = document.createElement(tag);
  if (attrs) applyAttrs(el, attrs);
  append(el, children);
  return el;
}

function applyAttrs(el, attrs) {
  for (const [key, value] of Object.entries(attrs)) {
    if (value === null || value === undefined || value === false) continue;
    if (key.startsWith('on') && typeof value === 'function') {
      el.addEventListener(key.slice(2), value);
    } else if (key === 'class') {
      el.className = Array.isArray(value) ? value.filter(Boolean).join(' ') : value;
    } else if (key === 'dataset') {
      for (const [k, v] of Object.entries(value)) el.dataset[k] = v;
    } else if (key === 'value' && ('value' in el)) {
      el.value = value;
    } else if (key === 'checked' || key === 'disabled' || key === 'selected' || key === 'open' || key === 'hidden') {
      el[key] = Boolean(value);
    } else if (value === true) {
      el.setAttribute(key, '');
    } else {
      el.setAttribute(key, String(value));
    }
  }
}

export function append(el, children) {
  for (const child of children.flat(Infinity)) {
    if (child === null || child === undefined || child === false || child === true) continue;
    el.append(child instanceof Node ? child : document.createTextNode(String(child)));
  }
  return el;
}

/** Replace an element's children. */
export function setChildren(el, ...children) {
  el.replaceChildren();
  return append(el, children);
}

/** A small inline icon from the sprite of paths below. */
export function icon(name, label) {
  const svg = document.createElementNS(SVG_NS, 'svg');
  svg.setAttribute('viewBox', '0 0 24 24');
  svg.setAttribute('class', 'icon');
  if (label) {
    svg.setAttribute('role', 'img');
    svg.setAttribute('aria-label', label);
  } else {
    svg.setAttribute('aria-hidden', 'true');
  }
  for (const d of ICONS[name] || ICONS.dot) {
    const path = document.createElementNS(SVG_NS, 'path');
    path.setAttribute('d', d);
    svg.append(path);
  }
  return svg;
}

const ICONS = {
  chat: ['M4 5h16v11H8l-4 4z'],
  threads: ['M4 6h10', 'M4 12h16', 'M4 18h12', 'M18 4v4', 'M16 16v4'],
  devices: ['M3 5h14v10H3z', 'M7 19h6', 'M10 15v4', 'M19 9h2v10h-4'],
  more: ['M5 12h.01', 'M12 12h.01', 'M19 12h.01'],
  models: ['M12 3l8 4.5v9L12 21l-8-4.5v-9z', 'M12 12l8-4.5', 'M12 12v9', 'M12 12L4 7.5'],
  usage: ['M4 20V10', 'M10 20V4', 'M16 20v-7', 'M22 20H2'],
  memory: ['M6 3h12v18l-6-4-6 4z'],
  skills: ['M14 4l6 6-10 10H4v-6z', 'M12 6l6 6'],
  settings: ['M12 15a3 3 0 1 0 0-6 3 3 0 0 0 0 6z', 'M19 12h2', 'M3 12h2', 'M12 3v2', 'M12 19v2', 'M17 7l1.5-1.5', 'M5.5 18.5L7 17', 'M17 17l1.5 1.5', 'M5.5 5.5L7 7'],
  plus: ['M12 5v14', 'M5 12h14'],
  send: ['M12 19V5', 'M6 11l6-6 6 6'],
  close: ['M6 6l12 12', 'M18 6L6 18'],
  menu: ['M4 7h16', 'M4 12h16', 'M4 17h16'],
  back: ['M15 5l-7 7 7 7'],
  chevron: ['M9 6l6 6-6 6'],
  down: ['M6 9l6 6 6-6'],
  stop: ['M7 7h10v10H7z'],
  up: ['M7 11v9H4v-9z', 'M7 11l4-8c1.5 0 2.5 1 2.5 2.5V9H19a2 2 0 0 1 2 2.3l-1.2 7A2 2 0 0 1 17.8 20H7'],
  downvote: ['M17 13V4h3v9z', 'M17 13l-4 8c-1.5 0-2.5-1-2.5-2.5V15H5a2 2 0 0 1-2-2.3l1.2-7A2 2 0 0 1 6.2 4H17'],
  shield: ['M12 3l8 3v6c0 5-3.5 8-8 9-4.5-1-8-4-8-9V6z'],
  warning: ['M12 3l10 18H2z', 'M12 10v5', 'M12 18h.01'],
  flag: ['M5 21V4', 'M5 4h12l-2 4 2 4H5'],
  check: ['M5 12l5 5 9-10'],
  x: ['M6 6l12 12', 'M18 6L6 18'],
  file: ['M6 3h8l4 4v14H6z', 'M14 3v4h4'],
  download: ['M12 4v11', 'M7 10l5 5 5-5', 'M5 20h14'],
  search: ['M11 18a7 7 0 1 0 0-14 7 7 0 0 0 0 14z', 'M16 16l5 5'],
  refresh: ['M20 11a8 8 0 1 0-2.3 5.7', 'M20 5v6h-6'],
  dot: ['M12 12h.01'],
  hub: ['M4 6h16v5H4z', 'M4 13h16v5H4z', 'M7 8.5h.01', 'M7 15.5h.01'],
  offline: ['M3 3l18 18', 'M8.5 16.5a5 5 0 0 1 7 0', 'M5 12.5a10 10 0 0 1 4-2.3', 'M2 8.8a15 15 0 0 1 4.2-2.7', 'M12 20h.01'],
  tool: ['M14 6a4 4 0 0 0 5 5l-8 8a2 2 0 0 1-3-3z'],
  logout: ['M15 4h4v16h-4', 'M10 8l-4 4 4 4', 'M6 12h10'],
  edit: ['M4 20h4L19 9l-4-4L4 16z'],
  trash: ['M5 7h14', 'M9 7V4h6v3', 'M7 7l1 13h8l1-13'],
};

/** Visually hidden text for screen readers. */
export function srOnly(text) {
  return h('span', { class: 'visually-hidden' }, text);
}

/** Debounce. */
export function debounce(fn, ms) {
  let t;
  return (...args) => {
    clearTimeout(t);
    t = setTimeout(() => fn(...args), ms);
  };
}
