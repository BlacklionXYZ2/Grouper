// "More" on narrow screens: the sections that don't fit the tab bar. Needs you
// comes first, with a live count, until the phone tabs are decided.

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
      icon('inbox'),
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
          item('#/models', 'Models', 'models', 'Registry, stats, proposals, rules', store.pendingProposals),
          item('#/usage', 'Usage & limits', 'usage', 'Spend, balances, budgets'),
          item('#/memory', 'Memory', 'memory', 'What Grouper remembers'),
          item('#/skills', 'Skills', 'skills', 'Saved procedures'),
          item('#/settings', 'Settings', 'settings', 'This browser, the hub, appearance'),
        ),
      ),
    ),
  );
  const offs = [on('approvals', syncNeeds), on('threads', syncNeeds)];
  return () => offs.forEach((off) => off());
}
