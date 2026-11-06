import { afterEach, describe, expect, test } from 'bun:test';
import { pairingMessage, renderPairing } from '../js/pairing.js';

const realFetch = globalThis.fetch;
afterEach(() => {
  globalThis.fetch = realFetch;
});

/** @param {(url: string, init: RequestInit) => Promise<Response>} impl */
function stubFetch(impl) {
  globalThis.fetch = /** @type {typeof fetch} */ (/** @type {unknown} */ (impl));
}

const tick = () => new Promise((resolve) => setTimeout(resolve, 0));

describe('pairingMessage', () => {
  test('each of the four refusals has its own plain sentence', () => {
    const messages = ['wrong_code', 'expired', 'locked', 'no_code'].map((error) =>
      pairingMessage({ kind: 'http', status: 403, error }),
    );
    expect(new Set(messages).size).toBe(4);
    for (const message of messages) expect(message).not.toContain('IdemA a répondu par une erreur');
  });

  test('an unreachable Mac gets the same causes as the offline banner', () => {
    expect(pairingMessage({ kind: 'offline' })).toContain('Tailscale');
    expect(pairingMessage({ kind: 'timeout' })).toContain('ne répond pas');
  });

  test('an error code that is an Object.prototype member is not a refusal', () => {
    const generic = pairingMessage({ kind: 'http', status: 500, error: 'internal' });
    for (const error of ['constructor', 'toString', '__proto__', 'hasOwnProperty']) {
      expect(pairingMessage({ kind: 'http', status: 403, error })).toBe(generic);
    }
  });

  test('any other failure is a generic sentence', () => {
    expect(pairingMessage({ kind: 'http', status: 500, error: 'internal' })).toContain('erreur');
  });
});

describe('renderPairing', () => {
  /** @returns {{root: HTMLElement, input: HTMLInputElement}} */
  function mount(onPaired = () => {}) {
    const root = document.createElement('div');
    renderPairing(root, { onPaired });
    return { root, input: /** @type {HTMLInputElement} */ (root.querySelector('input')) };
  }

  /** @param {HTMLInputElement} input @param {string} value */
  function type(input, value) {
    input.value = value;
    input.dispatchEvent(new Event('input'));
  }

  test('the field asks for a numeric keyboard and shows six cells', () => {
    const { root, input } = mount();
    expect(input.getAttribute('inputmode')).toBe('numeric');
    expect(input.getAttribute('autocomplete')).toBe('one-time-code');
    expect(root.querySelectorAll('.code__cell').length).toBe(6);
  });

  test('non-digits are dropped, and the sixth digit sends the code', async () => {
    /** @type {{url: string, body: unknown}[]} */
    const calls = [];
    stubFetch(async (url, init) => {
      calls.push({ url, body: JSON.parse(String(init.body)) });
      return new Response('{"device":{"id":"d","name":"iPhone"}}', { status: 200 });
    });
    let paired = 0;
    const { root, input } = mount(() => paired++);
    type(input, '12a34');
    expect(input.value).toBe('1234');
    expect(calls.length).toBe(0);
    type(input, '123456789');
    await tick();
    expect(calls).toEqual([{ url: '/idema-api/pair', body: { code: '123456' } }]);
    expect(paired).toBe(1);
    expect(root.querySelector('.error')?.textContent).toBe('');
  });

  test('a wrong code says so, empties the field and lets the user retry', async () => {
    stubFetch(async () => new Response('{"ok":false,"error":"wrong_code"}', { status: 403 }));
    let paired = 0;
    const { root, input } = mount(() => paired++);
    type(input, '000000');
    await tick();
    expect(paired).toBe(0);
    expect(root.querySelector('.error')?.textContent).toBe(pairingMessage({ kind: 'http', status: 403, error: 'wrong_code' }));
    expect(input.value).toBe('');
    expect(input.disabled).toBe(false);
  });
});
