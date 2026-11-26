// Long-term memory: search, add, edit, forget. Shared by every conversation.

import { h, icon, setChildren, debounce } from '../lib/dom.js';
import { get, post, put, del, qs, isAbort } from '../lib/api.js';
import { store, isAdmin } from '../lib/store.js';
import { bar, confirmAction, toast, toastError, skeletonBlocks, errorBanner, guardWrites } from '../lib/ui.js';
import { ago, date } from '../lib/format.js';

export function renderMemory(container) {
  const body = h('div', { class: 'view-body narrow' });
  container.append(h('section', { class: 'view' }, bar({ title: 'Memory', caption: 'What Grouper remembers across conversations' }), body));
  if (!isAdmin()) {
    body.append(h('div', { class: 'banner warn' }, icon('shield'), 'Memory is personal, so even reading it needs an admin token. Sign in with one in Settings.'));
    return null;
  }

  let query = '';
  let list = null;
  let error = null;
  let editing = null;
  // Leaving the view cancels the read in flight and a pending search.
  const aborter = new AbortController();
  let loading = null; // the AbortController of the newest read
  const onSearch = debounce((e) => {
    query = e.target.value.trim();
    load();
  }, 250);

  const search = h('input', {
    class: 'input',
    type: 'search',
    placeholder: 'Search memories',
    'aria-label': 'Search memories',
    oninput: onSearch,
  });

  const addContent = h('textarea', { class: 'textarea', name: 'content', required: true, rows: 2, placeholder: 'Something Grouper should remember, e.g. “I prefer British spelling.”' });
  const addTags = h('input', { class: 'input', name: 'tags', placeholder: 'tags, comma separated' });
  const addForm = h(
    'form',
    {
      class: 'card card-pad section',
      onsubmit: async (e) => {
        e.preventDefault();
        const content = addContent.value.trim();
        if (!content) return;
        try {
          await post('/v1/memories', { content, tags: tags(addTags.value) });
          addContent.value = '';
          addTags.value = '';
          toast('Remembered.');
          load();
        } catch (err) {
          toastError(err);
        }
      },
    },
    h('label', { class: 'field' }, h('span', null, 'Add a memory'), addContent),
    h('div', { class: 'row-form' }, h('label', { class: 'field' }, h('span', null, 'Tags'), addTags), h('button', { class: 'btn btn-primary', type: 'submit', 'data-write': true }, icon('plus'), 'Remember')),
  );

  const listSlot = h('div');
  body.append(h('div', { class: 'section' }, search), addForm, listSlot);

  function tags(text) {
    return text
      .split(',')
      .map((t) => t.trim())
      .filter(Boolean);
  }

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
      setChildren(listSlot, h('div', { class: 'empty' }, h('p', null, query ? 'No memories match.' : 'Nothing remembered yet. The orchestrator saves memories as you work, or add one above.')));
      return;
    }
    setChildren(
      listSlot,
      h('p', { class: 'hint' }, query ? `${list.length} matching` : 'Recently changed first'),
      h('ul', { class: 'list card' }, list.map((m) => h('li', null, editing === m.id ? editRow(m) : row(m)))),
    );
  }

  function row(m) {
    const conv = m.conversation_id ? store.conversations.find((c) => c.id === m.conversation_id) : null;
    return h(
      'div',
      { class: 'list-row' },
      h(
        'div',
        { class: 'grow' },
        h('div', { class: 'row-title' }, m.content),
        h(
          'div',
          { class: 'meta' },
          m.tags.map((t) => h('span', { class: 'badge' }, t)),
          h('span', null, `Updated ${ago(m.updated)}`),
          h('span', null, m.last_used ? `Last shown to a model ${ago(m.last_used)}` : 'Not shown to a model yet'),
          m.conversation_id ? h('a', { href: `#/chat/${encodeURIComponent(m.conversation_id)}` }, conv ? `From “${conv.title}”` : 'From a conversation') : h('span', null, `Added ${date(m.created)}`),
        ),
      ),
      h(
        'div',
        { class: 'bar-actions' },
        h(
          'button',
          {
            class: 'icon-btn',
            type: 'button',
            'data-write': true,
            'aria-label': 'Edit memory',
            onclick: () => {
              editing = m.id;
              render();
              listSlot.querySelector('textarea')?.focus();
            },
          },
          icon('edit'),
        ),
        h('button', { class: 'icon-btn', type: 'button', 'data-write': true, 'aria-label': 'Forget memory', onclick: () => forget(m) }, icon('trash')),
      ),
    );
  }

  function editRow(m) {
    const content = h('textarea', { class: 'textarea', value: m.content, 'aria-label': 'Memory text' });
    const tagInput = h('input', { class: 'input', value: m.tags.join(', '), 'aria-label': 'Tags' });
    return h(
      'form',
      {
        class: 'card-pad',
        onsubmit: async (e) => {
          e.preventDefault();
          try {
            await put(`/v1/memories/${m.id}`, { content: content.value.trim(), tags: tags(tagInput.value) });
            editing = null;
            toast('Saved.');
            load();
          } catch (err) {
            toastError(err);
          }
        },
      },
      h('div', { class: 'field' }, content),
      h(
        'div',
        { class: 'row-form' },
        h('label', { class: 'field' }, h('span', null, 'Tags'), tagInput),
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
        h('button', { class: 'btn btn-primary', type: 'submit', 'data-write': true }, 'Save'),
      ),
    );
  }

  async function forget(m) {
    const ok = await confirmAction({
      title: 'Forget this memory?',
      body: [h('blockquote', { class: 'callout' }, m.content), 'No conversation will see it again. This can’t be undone.'],
      confirmLabel: 'Forget',
      danger: true,
    });
    if (!ok) return;
    try {
      await del(`/v1/memories/${m.id}`);
      toast('Forgotten.');
      load();
    } catch (err) {
      toastError(err);
    }
  }

  async function load() {
    // A newer search replaces an older one still in flight.
    loading?.abort();
    const mine = new AbortController();
    loading = mine;
    try {
      list = (await get(`/v1/memories${qs({ q: query, limit: 200 })}`, { signal: mine.signal })) || [];
      error = null;
    } catch (err) {
      if (isAbort(err)) return;
      error = err;
    }
    if (aborter.signal.aborted) return;
    render();
  }

  const unguard = guardWrites(body);
  render();
  load();
  return () => {
    aborter.abort();
    loading?.abort();
    onSearch.cancel();
    unguard();
  };
}
