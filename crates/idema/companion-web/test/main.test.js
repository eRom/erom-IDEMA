import { afterEach, beforeEach, describe, expect, test } from 'bun:test';
import { createShell } from '../js/shell.js';
import { createStore } from '../js/store.js';

const FLEET = { mac: 'Mac', generated_at: 1, badge: 0, projects: [], sessions: [] };
const flush = () => new Promise((resolve) => setTimeout(resolve, 0));

/** Un `EventSource` à la main : chaque instance est gardée, on la fait tomber ou parler à volonté. */
class FakeEventSource {
  /** @type {FakeEventSource[]} */
  static instances = [];
  /** @param {string} url */
  constructor(url) {
    this.url = url;
    this.closed = false;
    /** @type {(() => void) | null} */
    this.onerror = null;
    /** @type {Record<string, ((event: {data: string}) => void)[]>} */
    this.listeners = {};
    FakeEventSource.instances.push(this);
  }
  /** @param {string} type @param {(event: any) => void} fn */
  addEventListener(type, fn) {
    (this.listeners[type] ??= []).push(fn);
  }
  close() {
    this.closed = true;
  }
  /** @param {unknown} fleet */
  emitFleet(fleet) {
    for (const fn of this.listeners.fleet ?? []) fn({ data: JSON.stringify(fleet) });
  }
  fail() {
    this.onerror?.();
  }
  static open() {
    return FakeEventSource.instances.filter((source) => !source.closed);
  }
}

/** Une horloge à la main : rien ne tourne tant que le test n'avance pas. */
function fakeTimers() {
  let now = 0;
  let nextId = 1;
  /** @type {Map<number, {fn: () => void, due: number, every: number | null}>} */
  const active = new Map();
  /** @param {() => void} fn @param {number} ms @param {number | null} every */
  const add = (fn, ms, every) => {
    const id = nextId++;
    active.set(id, { fn, due: now + ms, every });
    return id;
  };
  return {
    setInterval: (/** @type {() => void} */ fn, /** @type {number} */ ms) => add(fn, ms, ms),
    setTimeout: (/** @type {() => void} */ fn, /** @type {number} */ ms) => add(fn, ms, null),
    clearInterval: (/** @type {number} */ id) => void active.delete(id),
    clearTimeout: (/** @type {number} */ id) => void active.delete(id),
    /** @param {number} ms */
    advance(ms) {
      const target = now + ms;
      for (;;) {
        /** @type {[number, {fn: () => void, due: number, every: number | null}] | null} */
        let next = null;
        for (const entry of active) if (entry[1].due <= target && (!next || entry[1].due < next[1].due)) next = entry;
        if (!next) break;
        const [id, timer] = next;
        now = timer.due;
        if (timer.every === null) active.delete(id);
        else timer.due += timer.every;
        timer.fn();
      }
      now = target;
    },
  };
}

/**
 * Une fausse `api`. Les lectures de la flotte suivent `fleetScript` : un résultat, ou `'hang'` pour une requête qui ne
 * répond pas (on la règle plus tard avec `settle`) ; sans consigne, la flotte arrive.
 */
function fakeApi() {
  /** @type {{method: string, path: string, body: unknown, keepalive: boolean, settle?: (result: any) => void}[]} */
  const calls = [];
  /** @type {any[]} */
  const fleetScript = [];
  /** @type {import('../js/api.js').api} */
  const api = (method, path, body, _timeout, keepalive = false) => {
    /** @type {(typeof calls)[number]} */
    const call = { method, path, body, keepalive };
    calls.push(call);
    if (method === 'GET' && path === '/fleet') {
      const next = fleetScript.shift() ?? { kind: 'ok', data: FLEET };
      if (next === 'hang') {
        return new Promise((resolve) => {
          call.settle = resolve;
        });
      }
      return Promise.resolve(next);
    }
    return Promise.resolve({ kind: 'ok', data: null });
  };
  return {
    api,
    calls,
    fleetScript,
    fleetCalls: () => calls.filter((call) => call.method === 'GET' && call.path === '/fleet'),
    presenceCalls: () => calls.filter((call) => call.path === '/presence'),
    metricNames: () => calls.filter((call) => call.path === '/client-metrics').map((call) => /** @type {any} */ (call.body).name),
  };
}

/** @type {string} */
let visibility;
/** @param {'visible' | 'hidden'} next */
function setVisibility(next) {
  visibility = next;
  document.dispatchEvent(new Event('visibilitychange'));
}

/** @type {ReturnType<typeof createShell>[]} */
let shells = [];

/** Une coque avec un téléphone déjà appairé et une flotte en cache : elle démarre sans demander `me`. */
async function boot() {
  localStorage.clear();
  createStore(localStorage).setFleet(FLEET, Date.now());
  const store = createStore(localStorage);
  const root = document.createElement('div');
  document.body.replaceChildren(root);
  const timers = fakeTimers();
  const fake = fakeApi();
  const shell = createShell({
    root,
    store,
    api: fake.api,
    EventSource: /** @type {any} */ (FakeEventSource),
    timers,
  });
  shells.push(shell);
  return { shell, root, store, timers, ...fake, start: async () => (await shell.boot(), await flush()) };
}

beforeEach(() => {
  visibility = 'visible';
  FakeEventSource.instances = [];
  location.hash = '';
  Object.defineProperty(document, 'visibilityState', { configurable: true, get: () => visibility });
  Object.defineProperty(navigator, 'standalone', { configurable: true, value: true });
});

afterEach(() => {
  for (const shell of shells) shell.dispose();
  shells = [];
  Reflect.deleteProperty(document, 'visibilityState');
  Reflect.deleteProperty(navigator, 'standalone');
});

/** @param {HTMLElement} root */
const bannerHidden = (root) => /** @type {HTMLElement} */ (root.querySelector('.banner')).hidden;

describe('resume', () => {
  test('after a request frozen in flight, resume starts a fresh fetch and the late abort changes nothing', async () => {
    const app = await boot();
    app.fleetScript.push({ kind: 'offline' }, 'hang');
    await app.start();
    expect(bannerHidden(app.root)).toBe(false);
    expect(FakeEventSource.open().length).toBe(0);

    app.timers.advance(5000); // la reprise de 5 s lance une lecture que la page gelée ne verra jamais finir
    await flush();
    expect(app.fleetCalls().length).toBe(2);
    setVisibility('hidden');
    setVisibility('visible');
    await flush();

    expect(app.fleetCalls().length).toBe(3); // une requête neuve, pas la vieille reprise
    expect(bannerHidden(app.root)).toBe(true);
    expect(app.store.state.connection).toBe('ok');
    expect(FakeEventSource.open().length).toBe(1);

    app.fleetCalls()[1].settle?.({ kind: 'timeout' }); // l'abandon tardif, au dégel
    await flush();
    expect(bannerHidden(app.root)).toBe(true);
    expect(app.store.state.connection).toBe('ok');
    expect(app.metricNames()).toContain('resume_ready');
  });

  test('repeated resumes never leave more than one EventSource open', async () => {
    const app = await boot();
    await app.start();
    expect(FakeEventSource.open().length).toBe(1);
    for (let index = 0; index < 4; index++) {
      setVisibility('hidden');
      expect(FakeEventSource.open().length).toBe(0);
      setVisibility('visible');
      await flush();
      expect(FakeEventSource.open().length).toBe(1);
    }
  });

  test('a stale answer arriving while hidden opens no stream', async () => {
    const app = await boot();
    app.fleetScript.push('hang');
    await app.start();
    setVisibility('hidden');
    app.fleetCalls()[0].settle?.({ kind: 'ok', data: FLEET });
    await flush();
    expect(FakeEventSource.instances.length).toBe(0);
  });

  test('the cold-start metrics are sent once, then resume_ready on return', async () => {
    const app = await boot();
    await app.start();
    expect(app.metricNames()).toEqual(['shell_ready', 'fleet_ready']);
    setVisibility('hidden');
    setVisibility('visible');
    await flush();
    expect(app.metricNames()).toEqual(['shell_ready', 'fleet_ready', 'resume_ready']);
  });
});

describe('presence', () => {
  test('it only exists on a session screen: every 15 s, null on hide, back on show', async () => {
    const app = await boot();
    await app.start();
    app.timers.advance(60_000);
    expect(app.presenceCalls().length).toBe(0);

    location.hash = '#/s/idema-1a2b3c4d/2';
    window.dispatchEvent(new Event('hashchange'));
    expect(app.presenceCalls().map((call) => call.body)).toEqual([{ session: 'idema-1a2b3c4d', pane: 2 }]);
    app.timers.advance(15_000);
    app.timers.advance(15_000);
    expect(app.presenceCalls().length).toBe(3);

    setVisibility('hidden');
    const hide = app.presenceCalls().at(-1);
    expect(hide?.body).toBeNull();
    expect(hide?.keepalive).toBe(true);
    app.timers.advance(60_000);
    expect(app.presenceCalls().length).toBe(4);

    setVisibility('visible');
    expect(app.presenceCalls().at(-1)?.body).toEqual({ session: 'idema-1a2b3c4d', pane: 2 });

    location.hash = '#/';
    window.dispatchEvent(new Event('hashchange'));
    expect(app.presenceCalls().at(-1)?.body).toBeNull();
    const total = app.presenceCalls().length;
    app.timers.advance(60_000);
    expect(app.presenceCalls().length).toBe(total);
  });
});

describe('offline retry', () => {
  test('it retries every 5 s while the Mac is unreachable and stops once back', async () => {
    const app = await boot();
    app.fleetScript.push({ kind: 'offline' }, { kind: 'timeout' }, { kind: 'offline' });
    await app.start();
    expect(app.fleetCalls().length).toBe(1);
    expect(bannerHidden(app.root)).toBe(false);

    app.timers.advance(4999);
    await flush();
    expect(app.fleetCalls().length).toBe(1);
    app.timers.advance(1);
    await flush();
    expect(app.fleetCalls().length).toBe(2);
    expect(app.root.querySelector('.banner')?.textContent).toContain('ne répond pas');
    app.timers.advance(5000);
    await flush();
    expect(app.fleetCalls().length).toBe(3);

    app.timers.advance(5000); // la quatrième lecture réussit
    await flush();
    expect(app.fleetCalls().length).toBe(4);
    expect(bannerHidden(app.root)).toBe(true);
    app.timers.advance(60_000);
    await flush();
    expect(app.fleetCalls().length).toBe(4);
  });

  test('nothing retries while the page is hidden', async () => {
    const app = await boot();
    app.fleetScript.push({ kind: 'offline' });
    await app.start();
    setVisibility('hidden');
    app.timers.advance(60_000);
    await flush();
    expect(app.fleetCalls().length).toBe(1);
  });
});

describe('stream errors', () => {
  test('a dropped stream is reopened after 2 s, then 5 s, never at once', async () => {
    const app = await boot();
    await app.start();
    const [first] = FakeEventSource.instances;
    first.fail();
    expect(FakeEventSource.open().length).toBe(0);
    app.timers.advance(1999);
    await flush();
    expect(app.fleetCalls().length).toBe(1);
    app.timers.advance(1);
    await flush();
    expect(app.fleetCalls().length).toBe(2);
    expect(FakeEventSource.open().length).toBe(1);

    FakeEventSource.open()[0].fail(); // retombé sans avoir rien reçu : on attend plus longtemps
    app.timers.advance(4999);
    await flush();
    expect(app.fleetCalls().length).toBe(2);
    app.timers.advance(1);
    await flush();
    expect(app.fleetCalls().length).toBe(3);
  });

  test('a fleet event resets the wait', async () => {
    const app = await boot();
    await app.start();
    FakeEventSource.open()[0].fail();
    app.timers.advance(2000);
    await flush();
    FakeEventSource.open()[0].emitFleet({ ...FLEET, mac: 'Autre Mac' });
    expect(app.store.state.fleet?.mac).toBe('Autre Mac');
    FakeEventSource.open()[0].fail();
    app.timers.advance(2000);
    await flush();
    expect(app.fleetCalls().length).toBe(3);
  });
});

describe('pairing lost', () => {
  test('a 401 shows the pairing screen and forgets the cached fleet', async () => {
    const app = await boot();
    app.fleetScript.push({ kind: 'unpaired' });
    await app.start();
    expect(app.root.querySelector('.code')).not.toBeNull();
    expect(app.store.state.fleet).toBeNull();
    expect(localStorage.getItem('idema.fleet')).toBeNull();
    expect(FakeEventSource.open().length).toBe(0);
  });
});
