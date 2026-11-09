import { beforeEach, describe, expect, test } from 'bun:test';
import { CACHE_KEY, createNotesSync, DRAFT_KEY, KEEPALIVE_MAX_BYTES, PUT_TIMEOUT_MS, SAVE_DELAY_MS } from '../js/notes-sync.js';
import { createStore } from '../js/store.js';

const flush = () => new Promise((resolve) => setTimeout(resolve, 0));
/** @param {any} data */
const ok = (data) => ({ kind: /** @type {const} */ ('ok'), data });
/** @param {string} text @param {number | null} [modifiedAt] @param {boolean} [editable] */
const mac = (text, modifiedAt = 5, editable = true) => ok({ text, modified_at: modifiedAt, editable });
/** @param {number} status @param {string} error */
const http = (status, error) => ({ kind: /** @type {const} */ ('http'), status, error });
const OFFLINE = { kind: /** @type {const} */ ('offline') };
const TIMEOUT = { kind: /** @type {const} */ ('timeout') };

function deferred() {
  /** @type {(value: any) => void} */
  let resolve = () => {};
  const promise = new Promise((r) => (resolve = r));
  return { promise, resolve };
}

/**
 * Un moteur dont les minuteries et le réseau sont à la main.
 * @param {{handler: (method: string, path: string, body: any, keepalive: boolean, timeoutMs?: number) => any, storage?: Storage, store?: any}} options
 */
function harness({ handler, storage = localStorage, store }) {
  let now = 0;
  let nextId = 1;
  /** @type {Map<number, {fn: () => void, at: number}>} */
  const pending = new Map();
  const timers = /** @type {any} */ ({
    setTimeout: (/** @type {() => void} */ fn, /** @type {number} */ ms) => {
      const id = nextId++;
      pending.set(id, { fn, at: now + ms });
      return id;
    },
    clearTimeout: (/** @type {number} */ id) => void pending.delete(id),
    setInterval: () => 0,
    clearInterval() {},
  });
  /** @type {{method: string, path: string, body: any, keepalive: boolean, timeoutMs?: number}[]} */
  const calls = [];
  let inFlight = 0;
  let maxInFlight = 0;
  const request = async (/** @type {string} */ method, /** @type {string} */ path, /** @type {any} */ body, /** @type {boolean} */ keepalive = false, /** @type {number | undefined} */ timeoutMs = undefined) => {
    calls.push({ method, path, body, keepalive, timeoutMs });
    if (method === 'PUT') maxInFlight = Math.max(maxInFlight, ++inFlight);
    try {
      return await handler(method, path, body, keepalive, timeoutMs);
    } finally {
      if (method === 'PUT') inFlight--;
    }
  };
  /** @type {string[]} */
  const states = [];
  const sync = createNotesSync({ request: /** @type {any} */ (request), timers, storage, store, onChange: () => states.push(sync.snapshot().state) });
  /** @param {number} ms */
  async function advance(ms) {
    const target = now + ms;
    for (;;) {
      /** @type {[number, {fn: () => void, at: number}] | null} */
      let next = null;
      for (const entry of pending) if (entry[1].at <= target && (!next || entry[1].at < next[1].at)) next = entry;
      if (!next) break;
      pending.delete(next[0]);
      now = next[1].at;
      next[1].fn();
      await flush();
    }
    now = target;
    await flush();
  }
  const puts = () => calls.filter((c) => c.method === 'PUT');
  const gets = () => calls.filter((c) => c.method === 'GET');
  const storedDraft = () => JSON.parse(storage.getItem(DRAFT_KEY) ?? 'null');
  /** Le brouillon stocké sans la liste des envois à vérifier. */
  const draft = () => {
    const stored = storedDraft();
    return stored && { text: stored.text, base: stored.base };
  };
  const cache = () => JSON.parse(storage.getItem(CACHE_KEY) ?? 'null');
  return { sync, calls, puts, gets, states, advance, draft, storedDraft, cache, maxInFlight: () => maxInFlight };
}

/** Un Mac qui garde la note : GET la rend, PUT l'écrit (409 si la base n'est pas la sienne). */
function fakeMac(text = 'old', modifiedAt = 5) {
  const disk = { text, modifiedAt };
  /** @type {(method: string, path: string, body: any) => any} */
  const handler = (method, _path, body) => {
    if (method === 'GET') return mac(disk.text, disk.modifiedAt);
    if (body.base !== disk.modifiedAt && !body.force) return http(409, 'note_changed');
    disk.text = body.text;
    disk.modifiedAt += 1;
    return ok({ ok: true, modified_at: disk.modifiedAt });
  };
  return { disk, handler };
}

beforeEach(() => localStorage.clear());

describe('load', () => {
  test('load shows the Mac note and caches it', async () => {
    const h = harness({ handler: () => mac('hello', 7) });
    await h.sync.load();
    expect(h.sync.snapshot()).toMatchObject({ state: 'saved', text: 'hello', savedAt: 7 });
    expect(h.cache()).toEqual({ text: 'hello', modified_at: 7 });
  });

  test('load with a pending draft keeps the draft and sends it', async () => {
    localStorage.setItem(DRAFT_KEY, JSON.stringify({ text: 'd', base: 5 }));
    const h = harness({ handler: (method) => (method === 'GET' ? mac('old', 5) : ok({ ok: true, modified_at: 6 })) });
    await h.sync.load();
    expect(h.puts().map((c) => c.body)).toEqual([{ text: 'd', base: 5 }]);
    expect(h.sync.snapshot()).toMatchObject({ state: 'saved', text: 'd', savedAt: 6 });
    expect(h.draft()).toBeNull();
  });

  test('load offline falls back to the cache, editable', async () => {
    localStorage.setItem(CACHE_KEY, JSON.stringify({ text: 'cached', modified_at: 4 }));
    const h = harness({ handler: () => OFFLINE });
    await h.sync.load();
    expect(h.sync.snapshot()).toMatchObject({ state: 'offline', text: 'cached', savedAt: 4 });
    h.sync.edit('x');
    expect(h.draft()).toEqual({ text: 'x', base: 4 });
  });

  test('load offline with a pending draft shows the draft, offline', async () => {
    localStorage.setItem(DRAFT_KEY, JSON.stringify({ text: 'd', base: 5 }));
    const h = harness({ handler: () => OFFLINE });
    await h.sync.load();
    expect(h.sync.snapshot()).toMatchObject({ state: 'offline', text: 'd' });
    expect(h.puts().length).toBe(0);
  });

  test('load offline with no cache locks the editor', async () => {
    const h = harness({ handler: () => TIMEOUT });
    await h.sync.load();
    expect(h.sync.snapshot().state).toBe('locked');
    expect(h.sync.snapshot().reason).toContain('Le Mac ne répond pas');
    h.sync.edit('x');
    expect(h.draft()).toBeNull();
  });

  test('a non-editable note is read-only and edits send nothing', async () => {
    const h = harness({ handler: () => mac('locked text', 3, false) });
    await h.sync.load();
    expect(h.sync.snapshot()).toMatchObject({ state: 'readonly', text: 'locked text' });
    h.sync.edit('y');
    await h.advance(5000);
    expect(h.puts().length).toBe(0);
    expect(h.sync.snapshot().text).toBe('locked text');
  });

  test('a missing editable field means editable', async () => {
    const h = harness({ handler: () => ok({ text: 't', modified_at: 1 }) });
    await h.sync.load();
    expect(h.sync.snapshot().state).toBe('saved');
  });

  test('a note over 5 MB is read-only with the French reason', async () => {
    const h = harness({ handler: () => http(413, 'scratchpad_too_large') });
    await h.sync.load();
    expect(h.sync.snapshot()).toMatchObject({ state: 'readonly', reason: 'Note trop grosse pour le téléphone (5 Mo au plus) : modifiez-la sur le Mac.' });
  });

  test('another HTTP error on load is treated as offline: cache, or locked without one', async () => {
    const without = harness({ handler: () => http(500, 'boom') });
    await without.sync.load();
    expect(without.sync.snapshot().state).toBe('locked');
    localStorage.setItem(CACHE_KEY, JSON.stringify({ text: 'c', modified_at: 1 }));
    const withCache = harness({ handler: () => http(500, 'boom') });
    await withCache.sync.load();
    expect(withCache.sync.snapshot()).toMatchObject({ state: 'offline', text: 'c' });
  });

  test('resume with a pending draft does not reload over it', async () => {
    const mock = fakeMac('old', 5);
    const h = harness({ handler: mock.handler });
    await h.sync.load();
    mock.disk.text = 'laptop edit';
    h.sync.edit('mine');
    await h.sync.load();
    expect(h.sync.snapshot().text).toBe('mine');
  });

  test('a GET result never replaces the text once edit() was called, even without a draft', async () => {
    const mock = fakeMac('old', 5);
    const h = harness({ handler: mock.handler });
    await h.sync.load();
    h.sync.edit('typed');
    await h.advance(SAVE_DELAY_MS);
    expect(h.draft()).toBeNull();
    mock.disk.text = 'other';
    await h.sync.load();
    expect(h.sync.snapshot().text).toBe('typed');
  });
});

describe('autosave', () => {
  test('edits are debounced into one PUT 800 ms after the last keystroke', async () => {
    const mock = fakeMac();
    const h = harness({ handler: mock.handler });
    await h.sync.load();
    h.sync.edit('a');
    await h.advance(300);
    h.sync.edit('ab');
    await h.advance(300);
    h.sync.edit('abc');
    await h.advance(SAVE_DELAY_MS - 1);
    expect(h.puts().length).toBe(0);
    await h.advance(1);
    expect(h.puts().length).toBe(1);
    expect(h.puts()[0]).toMatchObject({ body: { text: 'abc', base: 5 }, timeoutMs: PUT_TIMEOUT_MS, keepalive: false });
  });

  test('typing during an in-flight PUT schedules one more PUT and never replaces the text', async () => {
    const slow = deferred();
    let putCount = 0;
    const h = harness({
      handler: (method, _path, body) => {
        if (method === 'GET') return mac('old', 5);
        return ++putCount === 1 ? slow.promise : ok({ ok: true, modified_at: 20 + body.text.length });
      },
    });
    await h.sync.load();
    h.sync.edit('a');
    await h.advance(SAVE_DELAY_MS);
    expect(h.puts().length).toBe(1);
    h.sync.edit('ab');
    slow.resolve(ok({ ok: true, modified_at: 11 }));
    await flush();
    expect(h.sync.snapshot().text).toBe('ab');
    expect(h.draft()).toEqual({ text: 'ab', base: 11 });
    await h.advance(SAVE_DELAY_MS);
    expect(h.puts().map((c) => c.body)).toEqual([{ text: 'a', base: 5 }, { text: 'ab', base: 11 }]);
    expect(h.sync.snapshot()).toMatchObject({ state: 'saved', text: 'ab' });
    expect(h.draft()).toBeNull();
  });

  test('a successful PUT clears the draft, moves the base and refreshes the cache', async () => {
    const mock = fakeMac('old', 5);
    const h = harness({ handler: mock.handler });
    await h.sync.load();
    h.sync.edit('new');
    expect(h.draft()).toEqual({ text: 'new', base: 5 });
    await h.advance(SAVE_DELAY_MS);
    expect(h.draft()).toBeNull();
    expect(h.sync.snapshot()).toMatchObject({ state: 'saved', savedAt: 6 });
    expect(h.cache()).toEqual({ text: 'new', modified_at: 6 });
  });

  test('offline PUT keeps the draft and says offline; the next load sends it', async () => {
    const mock = fakeMac('old', 5);
    let up = false;
    const h = harness({ handler: (method, path, body, keepalive) => (up ? mock.handler(method, path, body) : method === 'GET' ? mac('old', 5) : OFFLINE) });
    await h.sync.load();
    h.sync.edit('new');
    await h.advance(SAVE_DELAY_MS);
    expect(h.sync.snapshot().state).toBe('offline');
    expect(h.draft()).toEqual({ text: 'new', base: 5 });
    up = true;
    await h.sync.load();
    expect(h.puts().length).toBe(2);
    expect(h.sync.snapshot()).toMatchObject({ state: 'saved', text: 'new' });
    expect(h.draft()).toBeNull();
  });

  test('a refused PUT shows the French reason and does not retry until the next keystroke', async () => {
    const h = harness({ handler: (method) => (method === 'GET' ? mac('old', 5) : http(500, 'scratchpad_unwritable')) });
    await h.sync.load();
    h.sync.edit('a');
    await h.advance(SAVE_DELAY_MS);
    expect(h.sync.snapshot()).toMatchObject({ state: 'refused', reason: 'le Mac ne peut pas écrire la note' });
    await h.advance(10_000);
    expect(h.puts().length).toBe(1);
    await h.sync.flushNow();
    expect(h.puts().length).toBe(1);
    h.sync.edit('ab');
    await h.advance(SAVE_DELAY_MS);
    expect(h.puts().length).toBe(2);
    expect(h.draft()).toEqual({ text: 'ab', base: 5 });
  });

  test('refusal reasons: NUL character and too-large note', async () => {
    const control = harness({ handler: (method) => (method === 'GET' ? mac('o', 5) : http(400, 'control_char')) });
    await control.sync.load();
    control.sync.edit('a');
    await control.advance(SAVE_DELAY_MS);
    expect(control.sync.snapshot().reason).toBe('caractère NUL dans la note');
    const big = harness({ handler: (method) => (method === 'GET' ? mac('o', 5) : http(413, 'unknown')) });
    await big.sync.load();
    big.sync.edit('a');
    await big.advance(SAVE_DELAY_MS);
    expect(big.sync.snapshot()).toMatchObject({ state: 'refused', reason: 'note trop grosse (5 Mo au plus)' });
  });
});

describe('conflict', () => {
  test('a 409 against a different Mac note stops autosave and shows the conflict', async () => {
    const mock = fakeMac('old', 5);
    const h = harness({ handler: mock.handler });
    await h.sync.load();
    mock.disk.text = 'laptop';
    mock.disk.modifiedAt = 9;
    h.sync.edit('mine');
    await h.advance(SAVE_DELAY_MS);
    expect(h.sync.snapshot().state).toBe('conflict');
    expect(h.sync.snapshot().text).toBe('mine');
    h.sync.edit('mine more');
    await h.advance(10_000);
    expect(h.puts().length).toBe(1);
    expect(h.draft()).toEqual({ text: 'mine more', base: 5 });
  });

  test('overwrite sends force:true; takeMac drops the draft and reloads', async () => {
    const mock = fakeMac('old', 5);
    const h = harness({ handler: mock.handler });
    await h.sync.load();
    mock.disk.text = 'laptop';
    mock.disk.modifiedAt = 9;
    h.sync.edit('mine');
    await h.advance(SAVE_DELAY_MS);
    await h.sync.overwrite();
    expect(h.puts()[1].body).toEqual({ text: 'mine', base: 5, force: true });
    expect(mock.disk.text).toBe('mine');
    expect(h.sync.snapshot()).toMatchObject({ state: 'saved', text: 'mine', savedAt: 10 });
    expect(h.draft()).toBeNull();

    mock.disk.text = 'laptop 2';
    mock.disk.modifiedAt = 30;
    h.sync.edit('mine 2');
    await h.advance(SAVE_DELAY_MS);
    expect(h.sync.snapshot().state).toBe('conflict');
    await h.sync.takeMac();
    expect(h.sync.snapshot()).toMatchObject({ state: 'saved', text: 'laptop 2', savedAt: 30 });
    expect(h.draft()).toBeNull();
  });

  test('takeMac offline keeps the draft', async () => {
    const mock = fakeMac('old', 5);
    let down = false;
    const h = harness({ handler: (method, path, body) => (down && method === 'GET' ? OFFLINE : mock.handler(method, path, body)) });
    await h.sync.load();
    mock.disk.text = 'laptop';
    mock.disk.modifiedAt = 9;
    h.sync.edit('mine');
    await h.advance(SAVE_DELAY_MS);
    down = true;
    await h.sync.takeMac();
    expect(h.sync.snapshot()).toMatchObject({ state: 'conflict', text: 'mine' });
    expect(h.draft()).toEqual({ text: 'mine', base: 5 });
  });

  test('takeMac drops nothing typed while its GET was in flight', async () => {
    const mock = fakeMac('old', 5);
    const slow = deferred();
    let slowGet = false;
    const h = harness({ handler: (method, path, body) => (slowGet && method === 'GET' ? slow.promise : mock.handler(method, path, body)) });
    await h.sync.load();
    mock.disk.text = 'laptop';
    mock.disk.modifiedAt = 9;
    h.sync.edit('mine');
    await h.advance(SAVE_DELAY_MS);
    slowGet = true;
    const taking = h.sync.takeMac();
    await flush();
    h.sync.edit('mine!');
    slow.resolve(mac('laptop', 9));
    await taking;
    expect(h.sync.snapshot().text).toBe('mine!');
    expect(h.draft()).toEqual({ text: 'mine!', base: 5 });
  });
});

describe('silent reconciliation (no false conflict)', () => {
  test('a keepalive PUT that succeeded but whose reply was lost: the next load adopts the base without PUT', async () => {
    const mock = fakeMac('old', 5);
    const lost = { on: false };
    const h1 = harness({
      handler: (method, path, body) => {
        const result = mock.handler(method, path, body);
        return lost.on && method === 'PUT' ? OFFLINE : result;
      },
    });
    await h1.sync.load();
    h1.sync.edit('written');
    lost.on = true;
    await h1.sync.flushNow(true);
    expect(mock.disk.text).toBe('written');
    expect(h1.draft()).toEqual({ text: 'written', base: 5 });
    const h2 = harness({ handler: mock.handler });
    await h2.sync.load();
    expect(h2.puts().length).toBe(0);
    expect(h2.sync.snapshot()).toMatchObject({ state: 'saved', text: 'written', savedAt: 6 });
    expect(h2.draft()).toBeNull();
  });

  test('a PUT that timed out although the Mac wrote: the retry meets a 409 and adopts silently', async () => {
    const mock = fakeMac('old', 5);
    let timeOut = true;
    const h = harness({
      handler: (method, path, body) => {
        const result = mock.handler(method, path, body);
        return timeOut && method === 'PUT' ? TIMEOUT : result;
      },
    });
    await h.sync.load();
    h.sync.edit('written');
    await h.advance(SAVE_DELAY_MS);
    expect(h.sync.snapshot().state).toBe('offline');
    timeOut = false;
    await h.sync.flushNow();
    expect(h.states).not.toContain('conflict');
    expect(h.sync.snapshot()).toMatchObject({ state: 'saved', text: 'written', savedAt: 6 });
    expect(h.draft()).toBeNull();
  });

  test('flushNow during an in-flight PUT never runs two PUTs at once', async () => {
    const slow = deferred();
    let putCount = 0;
    const h = harness({ handler: (method, _p, body) => (method === 'GET' ? mac('old', 5) : ++putCount === 1 ? slow.promise : ok({ ok: true, modified_at: 12 })) });
    await h.sync.load();
    h.sync.edit('a');
    await h.advance(SAVE_DELAY_MS);
    h.sync.edit('ab');
    const flushing = h.sync.flushNow();
    await flush();
    expect(h.puts().length).toBe(1);
    slow.resolve(ok({ ok: true, modified_at: 11 }));
    await flushing;
    expect(h.maxInFlight()).toBe(1);
    expect(h.puts().map((c) => c.body)).toEqual([{ text: 'a', base: 5 }, { text: 'ab', base: 11 }]);
    expect(h.sync.snapshot().state).toBe('saved');
  });

  test('a success during typing rewrites the draft base: the reload sends it without a 409', async () => {
    const mock = fakeMac('old', 5);
    const slow = deferred();
    let hold = true;
    const h1 = harness({ handler: (method, path, body) => (hold && method === 'PUT' ? slow.promise.then(() => mock.handler(method, path, body)) : mock.handler(method, path, body)) });
    await h1.sync.load();
    h1.sync.edit('a');
    await h1.advance(SAVE_DELAY_MS);
    h1.sync.edit('ab');
    hold = false;
    slow.resolve(null);
    await flush();
    expect(h1.draft()).toEqual({ text: 'ab', base: 6 });
    const h2 = harness({ handler: mock.handler });
    await h2.sync.load();
    expect(h2.puts().map((c) => c.body)).toEqual([{ text: 'ab', base: 6 }]);
    expect(h2.states).not.toContain('conflict');
    expect(mock.disk.text).toBe('ab');
  });

  test('a keepalive is not sent while a PUT is in flight; the draft is enough', async () => {
    const slow = deferred();
    let putCount = 0;
    const h = harness({ handler: (method) => (method === 'GET' ? mac('old', 5) : ++putCount === 1 ? slow.promise : ok({ ok: true, modified_at: 12 })) });
    await h.sync.load();
    h.sync.edit('a');
    await h.advance(SAVE_DELAY_MS);
    h.sync.edit('ab');
    await h.sync.flushNow(true);
    expect(h.puts().length).toBe(1);
    expect(h.draft()).toEqual({ text: 'ab', base: 5 });
    slow.resolve(ok({ ok: true, modified_at: 11 }));
    await flush();
    // Le keepalive refusé n'a pas désarmé le minuteur : la suite part quand même, en envoi normal.
    await h.advance(SAVE_DELAY_MS);
    expect(h.puts().map((c) => c.body)).toEqual([{ text: 'a', base: 5 }, { text: 'ab', base: 11 }]);
    expect(h.puts()[1].keepalive).toBe(false);
    expect(h.sync.snapshot().state).toBe('saved');
  });
});

describe('flushNow keepalive', () => {
  test('flushNow(true) uses keepalive only under 64 KB of JSON body, measured in bytes', async () => {
    const h = harness({ handler: (method) => (method === 'GET' ? mac('old', 5) : ok({ ok: true, modified_at: 6 })) });
    await h.sync.load();
    h.sync.edit('small');
    await h.sync.flushNow(true);
    expect(h.puts().map((c) => c.keepalive)).toEqual([true]);

    // 40 000 caractères mais 80 000 octets : au-dessus de la limite.
    const big = 'é'.repeat(40_000);
    expect(new TextEncoder().encode(JSON.stringify({ text: big, base: 6 })).length).toBeGreaterThan(KEEPALIVE_MAX_BYTES);
    h.sync.edit(big);
    await h.sync.flushNow(true);
    expect(h.puts().length).toBe(1);
    expect(h.draft()?.text).toBe(big);
    // Trop gros pour un keepalive : un envoi normal suit, il ne reste pas en « Enregistrement… ».
    await h.advance(SAVE_DELAY_MS);
    expect(h.puts().length).toBe(2);
    expect(h.puts()[1].keepalive).toBe(false);
    expect(h.sync.snapshot().state).toBe('saved');
  });

  test('flushNow(false) sends a big note normally', async () => {
    const h = harness({ handler: (method) => (method === 'GET' ? mac('old', 5) : ok({ ok: true, modified_at: 6 })) });
    await h.sync.load();
    h.sync.edit('x'.repeat(70_000));
    await h.sync.flushNow();
    expect(h.puts().length).toBe(1);
    expect(h.puts()[0].keepalive).toBe(false);
  });
});

describe('storage failures', () => {
  test('a storage whose setItem throws does not stop the PUT, and the in-memory draft survives', async () => {
    const broken = /** @type {Storage} */ (/** @type {unknown} */ ({
      getItem: () => null,
      setItem: () => {
        throw new Error('quota');
      },
      removeItem: () => {},
    }));
    let up = false;
    const mock = fakeMac('old', 5);
    const h = harness({ storage: broken, handler: (method, path, body) => (method === 'GET' ? mac('old', 5) : up ? mock.handler(method, path, body) : OFFLINE) });
    await h.sync.load();
    h.sync.edit('kept');
    await h.advance(SAVE_DELAY_MS);
    expect(h.puts().length).toBe(1);
    expect(h.sync.snapshot().state).toBe('offline');
    up = true;
    await h.sync.load();
    expect(h.sync.snapshot()).toMatchObject({ state: 'saved', text: 'kept' });
    expect(mock.disk.text).toBe('kept');
  });
});

describe('store connection', () => {
  test('when the connection comes back, a locked editor loads and an offline one flushes', async () => {
    const store = createStore(localStorage);
    store.setConnection('offline');
    const mock = fakeMac('old', 5);
    let up = false;
    const h = harness({ store, handler: (method, path, body) => (up ? mock.handler(method, path, body) : OFFLINE) });
    await h.sync.load();
    expect(h.sync.snapshot().state).toBe('locked');
    up = true;
    store.setConnection('ok');
    await flush();
    expect(h.sync.snapshot()).toMatchObject({ state: 'saved', text: 'old' });

    store.setConnection('offline');
    up = false;
    h.sync.edit('typed');
    await h.advance(SAVE_DELAY_MS);
    expect(h.sync.snapshot().state).toBe('offline');
    up = true;
    store.setConnection('ok');
    await flush();
    expect(h.sync.snapshot().state).toBe('saved');
    expect(mock.disk.text).toBe('typed');
  });

  test('a disposed engine ignores the connection and its timers', async () => {
    const store = createStore(localStorage);
    const mock = fakeMac('old', 5);
    const h = harness({ store, handler: mock.handler });
    await h.sync.load();
    h.sync.edit('x');
    h.sync.dispose();
    await h.advance(5000);
    expect(h.puts().length).toBe(0);
    expect(h.draft()).toEqual({ text: 'x', base: 5 });
  });
});

describe('texts sent without a sure answer', () => {
  test('PUT a times out though the Mac wrote it, then typing ab and resending: no conflict', async () => {
    const mock = fakeMac('old', 5);
    const release = deferred();
    let first = true;
    const h = harness({
      handler: async (method, path, body) => {
        if (method === 'PUT' && first) {
          first = false;
          mock.handler(method, path, body); // le Mac écrit « a », la réponse se perd
          await release.promise;
          return TIMEOUT;
        }
        return mock.handler(method, path, body);
      },
    });
    await h.sync.load();
    h.sync.edit('a');
    await h.advance(SAVE_DELAY_MS);
    h.sync.edit('ab');
    await h.advance(SAVE_DELAY_MS);
    release.resolve(null);
    await flush();
    await h.advance(SAVE_DELAY_MS);
    await h.sync.flushNow();
    expect(h.states).not.toContain('conflict');
    expect(h.sync.snapshot()).toMatchObject({ state: 'saved', text: 'ab' });
    expect(mock.disk.text).toBe('ab');
    expect(h.draft()).toBeNull();
  });

  test('the unconfirmed text is stored with the draft, so a new instance recognises it', async () => {
    const mock = fakeMac('old', 5);
    let lost = false;
    const h1 = harness({ handler: (method, path, body) => (lost && method === 'PUT' ? (mock.handler(method, path, body), TIMEOUT) : mock.handler(method, path, body)) });
    await h1.sync.load();
    h1.sync.edit('a');
    lost = true;
    await h1.advance(SAVE_DELAY_MS);
    h1.sync.edit('ab');
    expect(h1.storedDraft()).toMatchObject({ text: 'ab', sent: ['a'] });
    h1.sync.dispose();
    const h2 = harness({ handler: mock.handler });
    await h2.sync.load();
    expect(h2.states).not.toContain('conflict');
    expect(h2.sync.snapshot()).toMatchObject({ state: 'saved', text: 'ab' });
    expect(mock.disk.text).toBe('ab');
  });

  test('reopening and typing during the load GET, while the Mac holds the phone own text: no conflict', async () => {
    const mock = fakeMac('x', 6);
    localStorage.setItem(DRAFT_KEY, JSON.stringify({ text: 'x', base: 5 }));
    const gate = deferred();
    let firstGet = true;
    const h = harness({
      handler: async (method, path, body) => {
        if (method === 'GET' && firstGet) {
          firstGet = false;
          await gate.promise;
        }
        return mock.handler(method, path, body);
      },
    });
    const loading = h.sync.load();
    await flush();
    h.sync.edit('xy');
    gate.resolve(null);
    await loading;
    await h.advance(SAVE_DELAY_MS);
    expect(h.states).not.toContain('conflict');
    expect(h.sync.snapshot()).toMatchObject({ state: 'saved', text: 'xy' });
    expect(mock.disk.text).toBe('xy');
  });
});

describe('dispose', () => {
  test('a late reply of a disposed engine never touches the storage of the next one', async () => {
    const mock = fakeMac('old', 5);
    const gate = deferred();
    let slow = true;
    const a = harness({
      handler: async (method, path, body) => {
        if (method === 'PUT' && slow) {
          slow = false;
          await gate.promise;
        }
        return mock.handler(method, path, body);
      },
    });
    await a.sync.load();
    a.sync.edit('a');
    await a.advance(SAVE_DELAY_MS);
    a.sync.dispose();
    const b = harness({ handler: (method, path, body) => (method === 'GET' ? new Promise(() => {}) : mock.handler(method, path, body)) });
    void b.sync.load();
    await flush();
    b.sync.edit('ab');
    expect(b.draft()).toEqual({ text: 'ab', base: 5 });
    gate.resolve(null);
    await flush();
    expect(b.draft()).toEqual({ text: 'ab', base: 5 });
    expect(a.cache()).toEqual({ text: 'old', modified_at: 5 });
    expect(a.puts().length).toBe(1);
  });

  test('a disposed engine sends nothing more from its queue', async () => {
    const mock = fakeMac('old', 5);
    const gate = deferred();
    let slow = true;
    const h = harness({
      handler: async (method, path, body) => {
        if (method === 'PUT' && slow) {
          slow = false;
          await gate.promise;
        }
        return mock.handler(method, path, body);
      },
    });
    await h.sync.load();
    h.sync.edit('a');
    await h.advance(SAVE_DELAY_MS);
    h.sync.edit('ab');
    void h.sync.flushNow();
    h.sync.dispose();
    gate.resolve(null);
    await flush();
    expect(h.puts().length).toBe(1);
  });
});

describe('leaving the page', () => {
  test('flushNow(true) then dispose() right away still sends the keepalive PUT, and its late reply leaves the storage alone', async () => {
    const mock = fakeMac('old', 5);
    const gate = deferred();
    const h = harness({ handler: async (method, path, body) => (method === 'PUT' ? (await gate.promise, mock.handler(method, path, body)) : mock.handler(method, path, body)) });
    await h.sync.load();
    h.sync.edit('typed');
    void h.sync.flushNow(true);
    h.sync.dispose();
    expect(h.puts().map((c) => [c.body.text, c.keepalive])).toEqual([['typed', true]]);
    const before = localStorage.getItem(DRAFT_KEY);
    const cacheBefore = localStorage.getItem(CACHE_KEY);
    gate.resolve(null);
    await flush();
    expect(mock.disk.text).toBe('typed');
    expect(localStorage.getItem(DRAFT_KEY)).toBe(before);
    expect(localStorage.getItem(CACHE_KEY)).toBe(cacheBefore);
    expect(h.puts().length).toBe(1);
  });

  test('a PUT still in flight when the instance dies is recognised by the next one: saved, not conflict', async () => {
    const mock = fakeMac('old', 5);
    const gate = deferred();
    let slow = true;
    const a = harness({
      handler: async (method, path, body) => {
        if (method === 'PUT' && slow) {
          slow = false;
          await gate.promise;
        }
        return mock.handler(method, path, body);
      },
    });
    await a.sync.load();
    a.sync.edit('a');
    await a.advance(SAVE_DELAY_MS);
    a.sync.edit('ab');
    a.sync.dispose();
    gate.resolve(null);
    await flush();
    expect(mock.disk.text).toBe('a');
    const b = harness({ handler: mock.handler });
    await b.sync.load();
    expect(b.states).not.toContain('conflict');
    expect(b.sync.snapshot()).toMatchObject({ state: 'saved', text: 'ab' });
    expect(mock.disk.text).toBe('ab');
  });
});
