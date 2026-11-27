// Tests for crates/hub/web/sw.js (run with: node crates/hub/web/tests/sw.test.mjs).
// Kept outside crates/hub/src/web.rs ASSETS so it is never served by the hub.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import { fileURLToPath } from 'node:url';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const swSource = fs.readFileSync(path.join(__dirname, '..', 'sw.js'), 'utf8');

function createHarness({ userAgent = '', brands = null, focusedChat = null, fetchHandler = null, existingNotes = [] } = {}) {
  const listeners = new Map();
  const shown = [];
  const openedWindows = [];
  const notes = existingNotes.map((tag) => ({
    tag,
    closed: false,
    close() {
      this.closed = true;
    },
  }));
  const fetchCalls = [];

  const clients = {
    async claim() {},
    async matchAll() {
      if (!focusedChat) return [];
      return [
        {
          url: 'https://hub.example/',
          visibilityState: 'visible',
          focused: true,
          focus: async () => {},
          postMessage(msg, [port]) {
            if (msg.type === 'grouper:where') {
              port.postMessage({
                signedIn: true,
                visible: true,
                focused: true,
                conversation: focusedChat,
              });
            } else if (msg.type === 'grouper:open') {
              port.postMessage({ ok: true });
            }
          },
        },
      ];
    },
    async openWindow(url) {
      openedWindows.push(url);
    },
  };

  const registration = {
    async showNotification(title, options) {
      shown.push({ title, options });
      if (options?.tag) {
        notes.push({
          tag: options.tag,
          closed: false,
          close() {
            this.closed = true;
          },
        });
      }
    },
    async getNotifications(filter) {
      return notes.filter((n) => !n.closed && (!filter?.tag || n.tag === filter.tag));
    },
  };

  const selfObj = {
    location: { origin: 'https://hub.example' },
    navigator: {
      userAgent,
      ...(brands ? { userAgentData: { brands } } : null),
    },
    clients,
    registration,
    skipWaiting() {},
    addEventListener(name, fn) {
      listeners.set(name, fn);
    },
  };

  const fetchMock = async (url, opts) => {
    fetchCalls.push({ url, opts });
    if (fetchHandler) return fetchHandler(url, opts);
    return { ok: true, json: async () => [] };
  };

  const context = vm.createContext({
    self: selfObj,
    fetch: fetchMock,
    URL,
    AbortController,
    MessageChannel,
    setTimeout,
    clearTimeout,
    console,
  });
  vm.runInContext(swSource, context, { filename: 'sw.js' });

  async function dispatchPush(payload) {
    let waited = Promise.resolve();
    const event = {
      data: payload === undefined ? null : { json: () => payload },
      waitUntil(p) {
        waited = p;
      },
    };
    listeners.get('push')(event);
    await waited;
  }

  async function dispatchClick(data) {
    let closed = false;
    let waited = Promise.resolve();
    const event = {
      notification: {
        data,
        close() {
          closed = true;
        },
      },
      waitUntil(p) {
        waited = p;
      },
    };
    listeners.get('notificationclick')(event);
    await waited;
    return { closed };
  }

  return { shown, notes, fetchCalls, openedWindows, dispatchPush, dispatchClick };
}

const tests = [
  [
    'WebKit (iPhone PWA) always shows notification even when chat is focused and prunes stale ones',
    async () => {
      const h = createHarness({
        userAgent: 'Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Mobile/15E148 Safari/604.1',
        focusedChat: 'c1',
        existingNotes: ['grouper-approval-old1', 'grouper-approval-still-pending'],
        fetchHandler: async () => ({
          ok: true,
          json: async () => [{ approval_id: 'still-pending' }, { approval_id: 'new1' }],
        }),
      });
      await h.dispatchPush({
        type: 'approval_requested',
        approval_id: 'new1',
        conversation_id: 'c1',
        thread_id: 't1',
        tool: 'run_shell',
        device: 'Laptop',
        thread_title: 'Build',
        chat_title: 'Project',
      });
      assert.equal(h.shown.length, 1, 'WebKit must never skip showNotification');
      assert.equal(h.shown[0].options.tag, 'grouper-approval-new1');
      assert.equal(h.fetchCalls.length, 1);
      assert.equal(h.fetchCalls[0].url, '/v1/approvals');
      assert.equal(h.fetchCalls[0].opts.credentials, 'same-origin');
      assert.equal(h.notes.find((n) => n.tag === 'grouper-approval-old1').closed, true);
      assert.equal(h.notes.find((n) => n.tag === 'grouper-approval-still-pending').closed, false);
      assert.equal(h.notes.find((n) => n.tag === 'grouper-approval-new1').closed, false);
    },
  ],
  [
    'iOS Chrome (CriOS, WebKit engine) always shows notification even when chat is focused',
    async () => {
      const h = createHarness({
        userAgent: 'Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) CriOS/130.0.0.0 Mobile/15E148 Safari/604.1',
        focusedChat: 'c1',
      });
      await h.dispatchPush({
        type: 'approval_requested',
        approval_id: 'new2',
        conversation_id: 'c1',
        tool: 'write_file',
      });
      assert.equal(h.shown.length, 1);
    },
  ],
  [
    'Chromium suppresses OS notification when visible focused tab is on that chat and still prunes stale ones',
    async () => {
      const h = createHarness({
        userAgent: 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36',
        brands: [{ brand: 'Chromium', version: '130' }],
        focusedChat: 'c1',
        existingNotes: ['grouper-approval-stale1'],
        fetchHandler: async () => ({ ok: true, json: async () => [] }),
      });
      await h.dispatchPush({
        type: 'approval_requested',
        approval_id: 'a1',
        conversation_id: 'c1',
        tool: 'run_shell',
      });
      assert.equal(h.shown.length, 0);
      assert.equal(h.notes.find((n) => n.tag === 'grouper-approval-stale1').closed, true);
    },
  ],
  [
    'Chromium shows OS notification when focused tab is on a different chat',
    async () => {
      const h = createHarness({
        userAgent: 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36',
        brands: [{ brand: 'Chromium', version: '130' }],
        focusedChat: 'c-other',
      });
      await h.dispatchPush({
        type: 'approval_requested',
        approval_id: 'a2',
        conversation_id: 'c1',
        tool: 'run_shell',
      });
      assert.equal(h.shown.length, 1);
      assert.equal(h.shown[0].options.tag, 'grouper-approval-a2');
    },
  ],
  [
    'Network failure during /v1/approvals prune still shows notification cleanly',
    async () => {
      const h = createHarness({
        userAgent: 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 Safari/605.1.15',
        fetchHandler: async () => {
          throw new Error('offline');
        },
      });
      await h.dispatchPush({
        type: 'approval_requested',
        approval_id: 'a3',
        conversation_id: 'c1',
        tool: 'list_files',
      });
      assert.equal(h.shown.length, 1);
    },
  ],
  [
    'Malformed or unknown push shows fallback notification to preserve userVisibleOnly',
    async () => {
      const h = createHarness();
      await h.dispatchPush(undefined);
      await h.dispatchPush({ type: 'approval_cleared', approval_id: 'legacy' });
      assert.equal(h.shown.length, 2);
      assert.equal(h.shown[0].options.tag, 'grouper-fallback');
      assert.equal(h.shown[1].options.tag, 'grouper-fallback');
    },
  ],
  [
    'notificationclick closes notification and opens deep link',
    async () => {
      const h = createHarness();
      const r = await h.dispatchClick({ conversation_id: 'c1', approval_id: 'a_abc' });
      assert.equal(r.closed, true);
      assert.deepEqual(h.openedWindows, ['https://hub.example/#/chat/c1?approval=a_abc']);
    },
  ],
];

let passed = 0;
for (const [name, fn] of tests) {
  await fn();
  passed++;
  console.log(`ok - ${name}`);
}
console.log(`${passed}/${tests.length} sw.js tests passed`);
