import { afterEach, describe, expect, test } from 'bun:test';
import { api, classifyFailure } from '../js/api.js';

const realFetch = globalThis.fetch;
afterEach(() => {
  globalThis.fetch = realFetch;
});

/** @param {(url: string, init: RequestInit) => Promise<Response>} impl */
function stubFetch(impl) {
  globalThis.fetch = /** @type {typeof fetch} */ (/** @type {unknown} */ (impl));
}

describe('classifyFailure', () => {
  test('a TypeError is an immediate failure', () => {
    expect(classifyFailure(new TypeError('Load failed'), false)).toBe('offline');
  });

  test('an abort is a timeout', () => {
    expect(classifyFailure(new DOMException('aborted', 'AbortError'), true)).toBe('timeout');
  });
});

describe('api', () => {
  test('a 401 means the device is not paired', async () => {
    stubFetch(async () => new Response('{"error":"unpaired"}', { status: 401 }));
    expect(await api('GET', '/fleet')).toEqual({ kind: 'unpaired' });
  });

  test('a 200 gives the parsed body, under the API prefix, with the JSON header on writes', async () => {
    /** @type {{url: string, init: RequestInit}[]} */
    const seen = [];
    stubFetch(async (url, init) => {
      seen.push({ url, init });
      return new Response('{"ok":true}', { status: 200 });
    });
    expect(await api('PUT', '/presence', null)).toEqual({ kind: 'ok', data: { ok: true } });
    expect(seen[0].url).toBe('/idema-api/presence');
    expect(seen[0].init.method).toBe('PUT');
    expect(seen[0].init.body).toBe('null');
    expect(new Headers(seen[0].init.headers).get('content-type')).toBe('application/json');
  });

  test('keepalive is asked for only when wanted', async () => {
    /** @type {RequestInit[]} */
    const inits = [];
    stubFetch(async (_url, init) => {
      inits.push(init);
      return new Response('{}', { status: 200 });
    });
    await api('PUT', '/presence', null, undefined, true);
    await api('PUT', '/presence', null);
    expect(inits.map((init) => init.keepalive ?? false)).toEqual([true, false]);
  });

  test('a 204 is a success without data', async () => {
    stubFetch(async () => new Response(null, { status: 204 }));
    expect(await api('POST', '/client-metrics', { name: 'shell_ready', ms: 1 })).toEqual({
      kind: 'ok',
      data: null,
    });
  });

  test('a refusal carries its status and error code', async () => {
    stubFetch(async () => new Response('{"ok":false,"error":"wrong_code"}', { status: 403 }));
    expect(await api('POST', '/pair', { code: '000000' })).toEqual({
      kind: 'http',
      status: 403,
      error: 'wrong_code',
    });
  });

  test('an error body that is not JSON still gives an http result', async () => {
    stubFetch(async () => new Response('<html>boom</html>', { status: 502 }));
    expect(await api('GET', '/fleet')).toEqual({ kind: 'http', status: 502, error: 'unknown' });
  });

  test('a network error is offline', async () => {
    stubFetch(async () => {
      throw new TypeError('Load failed');
    });
    expect(await api('GET', '/fleet')).toEqual({ kind: 'offline' });
  });

  test('a request that never answers is cut and reported as a timeout', async () => {
    stubFetch(
      (_url, init) =>
        new Promise((_resolve, reject) => {
          init.signal?.addEventListener('abort', () =>
            reject(new DOMException('aborted', 'AbortError')),
          );
        }),
    );
    const started = Date.now();
    expect(await api('GET', '/fleet', undefined, 50)).toEqual({ kind: 'timeout' });
    expect(Date.now() - started).toBeLessThan(2000);
  });
});
