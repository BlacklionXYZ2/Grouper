// "More" on narrow screens: the sections that don't fit the tab bar.

import { h, icon } from '../lib/dom.js';
import { store } from '../lib/store.js';
import { bar } from '../lib/ui.js';

export function renderMore(container) {
  const item = (href, label, iconName, sub, badge) =>
    h(
      'li',
      null,
      h(
        'a',
        { class: 'list-row', href },
        icon(iconName),
        h('div', { class: 'grow' }, h('div', { class: 'row-title' }, label), h('div', { class: 'row-sub' }, sub)),
        badge ? h('span', { class: 'count', 'aria-label': `${badge} pending` }, String(badge)) : null,
        icon('chevron'),
      ),
    );
  container.append(
    h(
      'section',
      { class: 'view' },
      bar({ title: 'More' }),
      h(
        'div',
        { class: 'view-body narrow' },
        h(
          'ul',
          { class: 'list card' },
          item('#/models', 'Models', 'models', 'Registry, stats, proposals, rules', store.pendingProposals),
          item('#/usage', 'Usage & limits', 'usage', 'Spend, balances, budgets'),
          item('#/memory', 'Memory', 'memory', 'What Grouper remembers'),
          item('#/skills', 'Skills', 'skills', 'Saved procedures'),
          item('#/settings', 'Settings', 'settings', 'This browser, the hub, appearance'),
        ),
      ),
    ),
  );
  return null;
}
