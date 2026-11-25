// Skills: procedures the orchestrator saved after tasks that worked. Search,
// read, edit, forget. There is no "new" here: the orchestrator saves them.

import { h, icon, setChildren, debounce } from '../lib/dom.js';
import { get, put, del, qs } from '../lib/api.js';
import { isAdmin } from '../lib/store.js';
import { renderMarkdown } from '../lib/md.js';
import { bar, confirmAction, toast, toastError, skeletonBlocks, errorBanner } from '../lib/ui.js';
import { ago } from '../lib/format.js';

export function renderSkills(container) {
  const body = h('div', { class: 'view-body narrow' });
  container.append(h('section', { class: 'view' }, bar({ title: 'Skills', caption: 'Procedures saved from tasks that worked' }), body));
  if (!isAdmin()) {
    body.append(h('div', { class: 'banner warn' }, icon('shield'), 'Skills need an admin token. Sign in with one in Settings.'));
    return null;
  }
  let query = '';
  let list = null;
  let error = null;
  const open = new Set();
  let editing = null;

  const search = h('input', {
    class: 'input',
    type: 'search',
    placeholder: 'Search skills',
    'aria-label': 'Search skills',
    oninput: debounce((e) => {
      query = e.target.value.trim();
      load();
    }, 250),
  });
  const listSlot = h('div');
  body.append(h('div', { class: 'section' }, search), listSlot);

  const csv = (text) =>
    text
      .split(',')
      .map((t) => t.trim())
      .filter(Boolean);

  function render() {
    if (error) {
      setChildren(listSlot, errorBanner(error, load));
      return;
    }
    if (!list) {
      setChildren(listSlot, skeletonBlocks(3));
      return;
    }
    if (!list.length) {
      setChildren(
        listSlot,
        h('div', { class: 'empty' }, h('h2', null, query ? 'No skills match' : 'No skills yet'), h('p', null, 'The orchestrator saves a skill after a task goes well, and offers it on similar tasks.')),
      );
      return;
    }
    setChildren(listSlot, h('ul', { class: 'list card' }, list.map((s) => h('li', null, editing === s.id ? editForm(s) : item(s)))));
  }

  function item(s) {
    const details = h(
      'details',
      {
        open: open.has(s.id),
        ontoggle: () => (details.open ? open.add(s.id) : open.delete(s.id)),
      },
      h(
        'summary',
        { class: 'list-row' },
        h(
          'div',
          { class: 'grow' },
          h('div', { class: 'row-title' }, s.name),
          h('div', { class: 'row-sub' }, s.description),
          h('div', { class: 'meta' }, h('span', null, `Worked ${s.success_count}×`), h('span', null, s.last_used ? `Last used ${ago(s.last_used)}` : 'Not used yet')),
        ),
      ),
      h(
        'div',
        { class: 'card-pad' },
        h('div', { class: 'md' }, renderMarkdown(s.steps)),
        list_('Triggers', s.triggers),
        list_('Tools', s.tools),
        list_('Models', s.models),
        list_('Devices', s.devices),
        h(
          'div',
          { class: 'bar-actions' },
          h(
            'button',
            {
              class: 'btn btn-sm',
              type: 'button',
              onclick: () => {
                editing = s.id;
                render();
              },
            },
            icon('edit'),
            'Edit',
          ),
          h('button', { class: 'btn btn-sm', type: 'button', onclick: () => forget(s) }, icon('trash'), 'Forget'),
        ),
      ),
    );
    return details;
  }

  function list_(label, values) {
    if (!values?.length) return null;
    return h('div', { class: 'meta' }, h('strong', null, `${label}:`), values.map((v) => h('span', { class: 'badge' }, v)));
  }

  function editForm(s) {
    const field = (name, label, value, multiline = false) =>
      h('label', { class: 'field' }, h('span', null, label), multiline ? h('textarea', { class: 'textarea mono', name, value, rows: 8 }) : h('input', { class: 'input', name, value }));
    const form = h(
      'form',
      {
        class: 'card-pad',
        onsubmit: async (e) => {
          e.preventDefault();
          const f = new FormData(form);
          try {
            await put(`/v1/skills/${s.id}`, {
              name: String(f.get('name')).trim(),
              description: String(f.get('description')).trim(),
              steps: String(f.get('steps')),
              triggers: csv(String(f.get('triggers'))),
            });
            editing = null;
            toast('Skill saved.');
            load();
          } catch (err) {
            toastError(err);
          }
        },
      },
      field('name', 'Name', s.name),
      field('description', 'Description', s.description),
      field('triggers', 'Triggers (comma separated)', s.triggers.join(', ')),
      field('steps', 'Steps (Markdown)', s.steps, true),
      h(
        'div',
        { class: 'bar-actions' },
        h(
          'button',
          {
            class: 'btn',
            type: 'button',
            onclick: () => {
              editing = null;
              render();
            },
          },
          'Cancel',
        ),
        h('button', { class: 'btn btn-primary', type: 'submit' }, 'Save'),
      ),
    );
    return form;
  }

  async function forget(s) {
    if (!(await confirmAction({ title: `Forget “${s.name}”?`, body: 'The orchestrator won’t offer it again. This can’t be undone.', confirmLabel: 'Forget', danger: true }))) return;
    try {
      await del(`/v1/skills/${s.id}`);
      toast('Forgotten.');
      load();
    } catch (err) {
      toastError(err);
    }
  }

  async function load() {
    try {
      list = (await get(`/v1/skills${qs({ q: query })}`)) || [];
      error = null;
    } catch (err) {
      error = err;
    }
    render();
  }

  render();
  load();
  return null;
}
