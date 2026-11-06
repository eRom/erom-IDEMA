import { afterEach, beforeEach, describe, expect, test } from 'bun:test';
import { api } from '../js/api.js';
import { mountNotes } from '../js/notes.js';
import { sendTestNotification } from '../js/push.js';
import { mountSession } from '../js/session.js';
import { createStore } from '../js/store.js';
import { stopTriage } from '../js/triage.js';

// Le contrat avec le garde du Mac (415 sans `Content-Type: application/json` sur toute écriture) : on branche les vrais
// écrans sur le vrai `api()` et on regarde ce que `fetch` reçoit. Aucune requête de la page ne doit y échapper.

const realFetch = globalThis.fetch;

/** @type {{method: string, path: string, contentType: string | null, body: unknown}[]} */
let sent;

beforeEach(() => {
  sent = [];
  globalThis.fetch = /** @type {typeof fetch} */ (
    /** @type {unknown} */ (
      async (/** @type {string} */ url, /** @type {RequestInit} */ init) => {
        sent.push({
          method: init.method ?? 'GET',
          path: url,
          contentType: new Headers(init.headers).get('content-type'),
          body: init.body,
        });
        return new Response(JSON.stringify({ text: 'prompt', accepts_input: true, state: 'idle', result: 'sent', outcome: 'sent' }), { status: 200 });
      }
    )
  );
});
afterEach(() => {
  globalThis.fetch = realFetch;
  stopTriage();
  document.body.replaceChildren();
});

const writes = () => sent.filter((call) => call.method !== 'GET' && call.method !== 'HEAD');
const flush = async () => {
  for (let i = 0; i < 8; i++) await Promise.resolve();
  await new Promise((resolve) => setTimeout(resolve, 0));
};

/** Les écritures sont comptées, jamais supposées : une liste vide ne prouve rien. @param {number} minimum */
function expectJsonWrites(minimum) {
  expect(writes().length).toBeGreaterThanOrEqual(minimum);
  for (const call of writes()) {
    expect(call.contentType).toBe('application/json');
    expect(typeof call.body).toBe('string');
    expect(() => JSON.parse(/** @type {string} */ (call.body))).not.toThrow();
  }
}

describe('every write of the page carries a JSON body', () => {
  test('the pane acknowledgement and an input from the session screen', async () => {
    location.hash = '#/s/idema-aaaa1111/1';
    const root = document.createElement('div');
    document.body.replaceChildren(root);
    const store = createStore(localStorage);
    const unmount = mountSession(root, /** @type {any} */ ({
      route: { name: 'session', session: 'idema-aaaa1111', pane: 1 },
      store,
      request: (/** @type {string} */ method, /** @type {string} */ path, /** @type {unknown} */ body, /** @type {boolean} */ keepalive, /** @type {number} */ timeoutMs, /** @type {Record<string, string>} */ headers) =>
        api(method, path, body, timeoutMs, keepalive, headers),
    }));
    await flush();
    /** @type {HTMLElement} */ (root.querySelector('button[aria-label="Entrée"]')).click();
    await flush();
    unmount?.();
    const acks = writes().filter((call) => call.path.endsWith('/ack'));
    expect(acks).toHaveLength(1);
    expect(acks[0].body).toBe('{}');
    expect(writes().some((call) => call.path.endsWith('/input'))).toBe(true);
    expectJsonWrites(2);
  });

  test('the test notification', async () => {
    const outcome = await sendTestNotification({
      request: (method, path, body, _keepalive, timeoutMs) => api(method, path, body, timeoutMs),
    });
    expect(outcome).toBe('sent');
    expect(writes().map((call) => call.path)).toEqual(['/idema-api/push/test']);
    expectJsonWrites(1);
  });

  test('a line added to the note', async () => {
    const root = document.createElement('div');
    document.body.replaceChildren(root);
    const store = createStore(localStorage);
    const unmount = mountNotes(root, /** @type {any} */ ({
      store,
      request: (/** @type {string} */ method, /** @type {string} */ path, /** @type {unknown} */ body) => api(method, path, body),
    }));
    await flush();
    /** @type {HTMLInputElement} */ (root.querySelector('.notes__input')).value = 'une ligne';
    /** @type {HTMLElement} */ (root.querySelector('.notes__form .btn')).click();
    await flush();
    unmount?.();
    expect(writes().map((call) => call.path)).toEqual(['/idema-api/scratchpad/append']);
    expectJsonWrites(1);
  });

  test('the presence heartbeat, the metrics and the pairing, as the shell sends them', async () => {
    await api('PUT', '/presence', null, undefined, true);
    await api('POST', '/client-metrics', { name: 'shell_ready', ms: 3 });
    await api('POST', '/pair', { code: '123456' });
    await api('PUT', '/push/subscription', { endpoint: 'https://push.example/x', keys: { p256dh: 'a', auth: 'b' } });
    expectJsonWrites(4);
  });
});
