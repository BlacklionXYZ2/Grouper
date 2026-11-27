// "More" on narrow screens: the sections that don't fit the tab bar (Chat / Devices /
// More). Needs you comes first, with a live count. Rows use the filled nav icons.

import { h, icon, setText, setAttr } from '../lib/dom.js';
import { store, on, needsYouCount } from '../lib/store.js';
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

  // Needs you keeps its count current while this page is open.
  const needsBadge = h('span', { class: 'count', hidden: true });
  const needsRow = h(
    'li',
    null,
    h(
      'a',
      { class: 'list-row', href: '#/inbox' },
      icon('inbox-fill'),
      h('div', { class: 'grow' }, h('div', { class: 'row-title' }, 'Needs you'), h('div', { class: 'row-sub' }, 'Approvals and threads waiting for a device')),
      needsBadge,
      icon('chevron'),
    ),
  );
  const syncNeeds = () => {
    const n = needsYouCount();
    needsBadge.hidden = !n;
    setText(needsBadge, n ? String(n) : '');
    setAttr(needsBadge, 'aria-label', n ? `${n} need you` : null);
  };
  syncNeeds();

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
          needsRow,
          item('#/models', 'Models', 'models-fill', 'Registry, stats, proposals, rules', store.pendingProposals),
          item('#/usage', 'Usage & limits', 'usage-fill', 'Spend, balances, budgets'),
          item('#/memory', 'Memory', 'memory-fill', 'What Grouper remembers'),
          item('#/skills', 'Skills', 'skills-fill', 'Saved procedures'),
          item('#/settings', 'Settings', 'settings-fill', 'This browser, the hub, appearance'),
        ),
      ),
    ),
  );
  const offs = [on('approvals', syncNeeds), on('threads', syncNeeds)];
  return () => offs.forEach((off) => off());
}
