// The approval sheet: device, tool, the arguments that will run and, most
// prominently, the hub's reason. Approve / Deny post to /v1/approvals/{id}.
// "Change" denies, then opens the thread's step-in field with a draft; "Stop
// thread" ends the thread (which drops the request).
//
// A tool on Manual asks every time, so Approve is the expected answer and gets the
// accent. When a risk rule asked (the call looked dangerous), Approve and Deny look
// the same: the sheet doesn't nudge towards either.

import { h, icon } from '../lib/dom.js';
import { get, post, enc, describe } from '../lib/api.js';
import { store, on, emit, deviceName, isAdmin } from '../lib/store.js';
import { sheet, toast, navigate } from '../lib/ui.js';
import { pretty, ago, duration } from '../lib/format.js';
import { setStepInDraft, stopThread } from './thread.js';

let openId = null;

/** The hub's reason for a Manual tool ends like this (core-agent permission.rs). */
function isManualReason(reason) {
  return !reason || / is set to Manual$/.test(reason);
}

export function openApproval(approvalId, { conversationId } = {}) {
  const a = store.approvals.get(approvalId);
  if (!a || openId) return;
  if (conversationId && a.conversation_id && a.conversation_id !== conversationId) {
    navigate(`#/chat/${enc(a.conversation_id)}`);
    return;
  }
  openId = approvalId;
  const thread = store.threads.get(a.thread_id);
  const device = deviceName(a.device_id);
  const riskRule = !isManualReason(a.reason);
  const cleanups = [];
  const aborter = new AbortController();
  cleanups.push(() => aborter.abort());

  sheet((close) => {
    let settled = false; // answered here or elsewhere: the buttons stay off
    let busy = false;
    const status = h('div', { role: 'status', class: 'hint' });
    const approve = h(
      'button',
      { class: ['btn', 'btn-lg', riskRule ? null : 'btn-primary'], type: 'button', 'aria-label': `Approve ${a.tool} on ${device}`, onclick: () => answer(true) },
      icon('check'),
      'Approve',
    );
    const deny = h(
      'button',
      { class: 'btn btn-lg', type: 'button', 'aria-label': `Deny ${a.tool} on ${device}`, onclick: () => answer(false) },
      icon('x'),
      'Deny',
    );
    const change = h('button', { class: 'link-btn', type: 'button', onclick: () => answer(false, true) }, 'Change it: deny and tell the thread what to do instead');
    const stopBtn = h('button', { class: 'link-btn danger', type: 'button', onclick: () => stopIt() }, icon('stop'), 'Stop the thread instead');
    const actions = h('div', { class: 'approval-actions' }, deny, approve);
    const waitText = h('span', null, `Requested ${ago(a.ts)}. The call waits until someone answers or the thread is stopped. `);

    /** Buttons follow who may answer, the connection and whether it's settled. */
    function sync() {
      const off = !isAdmin() || !store.online || settled || busy;
      approve.disabled = deny.disabled = change.disabled = stopBtn.disabled = off;
      if (settled || busy) return;
      if (!isAdmin()) status.textContent = 'This session uses a device token, which can’t answer approvals.';
      else if (!store.online) status.textContent = 'Offline. You can answer once the hub reconnects.';
      else if (/^Offline/.test(status.textContent)) status.textContent = '';
    }

    function answered(text) {
      settled = true;
      sync();
      actions.replaceChildren(h('div', { class: 'banner info' }, icon('check'), h('span', { class: 'grow' }, text)));
      change.hidden = stopBtn.hidden = true;
      setTimeout(() => close(), 1400);
    }

    async function answer(approved, thenChange = false) {
      busy = true;
      sync();
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
        busy = false;
        if (err.status === 404) {
          store.answered.add(a.approval_id);
          store.approvals.delete(a.approval_id);
          emit('approvals');
          answered('Answered on another device.');
        } else {
          status.textContent = describe(err);
          sync();
        }
      }
    }

    async function stopIt() {
      busy = true;
      sync();
      try {
        const stopped = await stopThread(a.thread_id);
        busy = false;
        if (stopped) answered('Thread stopped. The call won’t run.');
        else sync();
      } catch (err) {
        busy = false;
        status.textContent = describe(err);
        sync();
      }
    }

    cleanups.push(
      on('approval_resolved', (e) => {
        if (e.approval_id === a.approval_id && !settled) answered('Answered on another device.');
      }),
      // A stopped or finished thread drops its requests without an answer event.
      on('approvals', () => {
        if (!store.approvals.has(a.approval_id) && !settled && !busy) {
          answered(store.answered.has(a.approval_id) ? 'Answered on another device.' : 'This request is no longer waiting: its thread ended.');
        }
      }),
      on('online', sync),
    );

    // The real deadline, when the hub has an approval timeout.
    get('/v1/limits', { signal: aborter.signal })
      .then((report) => {
        const secs = report?.limits?.approval_timeout_secs;
        if (!secs) return;
        const left = Math.max(0, Math.round(a.ts + secs - Date.now() / 1000));
        const then = report.limits.approval_timeout_action === 'stop' ? 'it’s denied and the thread is stopped' : 'it’s denied and the thread goes on';
        waitText.textContent = `Requested ${ago(a.ts)}. If nobody answers within ${duration(secs)} (about ${duration(left)} left), ${then}. `;
      })
      .catch(() => {
        /* keep the general wording */
      });

    sync();
    const reasonText = a.reason || 'This tool is set to Manual, so every call waits for you.';
    return {
      title: 'Run this tool?',
      caption: `${a.tool} · ${device}${thread ? ` · ${thread.title}` : ''}`,
      body: [
        h('div', { class: ['reason', riskRule && 'risk'] }, icon('warning'), h('div', null, riskRule ? h('strong', { class: 'reason-kind' }, 'A safety rule asked. ') : null, reasonText)),
        h('div', null, h('div', { class: 'field-label' }, 'What will run'), h('pre', { class: 'code', tabindex: '0', 'aria-label': `Arguments for ${a.tool}` }, pretty(a.input))),
        h(
          'p',
          { class: 'hint' },
          waitText,
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
      foot: [h('div', { class: 'approval-foot' }, actions, h('div', { class: 'approval-links' }, change, stopBtn))],
    };
  }).finally(() => {
    for (const off of cleanups) off();
    openId = null;
  });
}
