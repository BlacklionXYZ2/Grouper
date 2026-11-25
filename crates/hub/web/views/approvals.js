// The approval sheet: device, tool, the arguments that will run and, most
// prominently, the hub's reason. Approve / Deny post to /v1/approvals/{id}.
// "Change" denies, then opens the thread's step-in field with a draft.

import { h, icon } from '../lib/dom.js';
import { post, enc, describe } from '../lib/api.js';
import { store, on, emit, deviceName, isAdmin } from '../lib/store.js';
import { sheet, toast, navigate } from '../lib/ui.js';
import { pretty, ago } from '../lib/format.js';
import { setStepInDraft } from './thread.js';

let openId = null;

export function openApproval(approvalId) {
  const a = store.approvals.get(approvalId);
  if (!a || openId) return;
  openId = approvalId;
  const thread = store.threads.get(a.thread_id);
  const device = deviceName(a.device_id);
  let off = null;

  sheet((close) => {
    const status = h('div', { role: 'status', class: 'hint' });
    const approve = h(
      'button',
      { class: 'btn btn-primary btn-lg', type: 'button', 'aria-label': `Approve ${a.tool} on ${device}`, onclick: () => answer(true) },
      icon('check'),
      'Approve',
    );
    const deny = h(
      'button',
      { class: 'btn btn-lg', type: 'button', 'aria-label': `Deny ${a.tool} on ${device}`, onclick: () => answer(false) },
      icon('x'),
      'Deny',
    );
    const change = h(
      'button',
      { class: 'link-btn', type: 'button', onclick: () => answer(false, true) },
      'Change it: deny and tell the thread what to do instead',
    );
    const actions = h('div', { class: 'approval-actions' }, deny, approve);
    if (!isAdmin()) {
      approve.disabled = deny.disabled = change.disabled = true;
      status.textContent = 'This session uses a device token, which can’t answer approvals.';
    }

    function answered(text) {
      approve.disabled = deny.disabled = change.disabled = true;
      actions.replaceChildren(h('div', { class: 'banner info' }, icon('check'), h('span', { class: 'grow' }, text)));
      change.hidden = true;
      setTimeout(() => close(), 1400);
    }

    async function answer(approved, thenChange = false) {
      approve.disabled = deny.disabled = change.disabled = true;
      status.textContent = approved ? 'Approving…' : 'Denying…';
      try {
        await post(`/v1/approvals/${enc(a.approval_id)}`, { approve: approved });
        store.answered.add(a.approval_id);
        store.approvals.delete(a.approval_id);
        emit('approvals');
        if (thenChange) {
          setStepInDraft(a.thread_id, 'Don’t run that. Instead: ');
          close();
          navigate(`#/chat/${enc(a.conversation_id)}/thread/${enc(a.thread_id)}`);
          return;
        }
        toast(approved ? `Approved ${a.tool} on ${device}.` : `Denied ${a.tool} on ${device}.`);
        close();
      } catch (err) {
        if (err.status === 404) {
          store.answered.add(a.approval_id);
          store.approvals.delete(a.approval_id);
          emit('approvals');
          answered('Answered on another device.');
        } else {
          status.textContent = describe(err);
          approve.disabled = deny.disabled = change.disabled = false;
        }
      }
    }

    const offResolved = on('approval_resolved', (e) => {
      if (e.approval_id === a.approval_id && !approve.disabled) answered('Answered on another device.');
    });
    // A stopped or finished thread drops its requests without an answer event.
    const offGone = on('approvals', () => {
      if (!store.approvals.has(a.approval_id) && !approve.disabled) {
        approve.disabled = deny.disabled = change.disabled = true;
        answered(store.answered.has(a.approval_id) ? 'Answered on another device.' : 'This request is no longer waiting: its thread ended.');
      }
    });
    off = () => {
      offResolved();
      offGone();
    };

    return {
      title: 'Run this tool?',
      caption: `${a.tool} · ${device}${thread ? ` · ${thread.title}` : ''}`,
      body: [
        h('div', { class: 'reason' }, icon('warning'), h('div', null, a.reason || 'This tool is set to Manual, so every call waits for you.')),
        h(
          'div',
          null,
          h('div', { class: 'field-label' }, 'What will run'),
          h('pre', { class: 'code', tabindex: '0', 'aria-label': `Arguments for ${a.tool}` }, pretty(a.input)),
        ),
        h(
          'p',
          { class: 'hint' },
          `Requested ${ago(a.ts)}. The call waits until someone answers or the thread is stopped. `,
          h(
            'a',
            {
              href: `#/chat/${enc(a.conversation_id)}/thread/${enc(a.thread_id)}`,
              onclick: () => close(),
            },
            'Open the thread',
          ),
        ),
        status,
      ],
      foot: [h('div', { class: 'approval-foot' }, actions, change)],
    };
  }).finally(() => {
    off?.();
    openId = null;
  });
}
