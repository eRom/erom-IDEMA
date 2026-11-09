import { afterEach, beforeEach, describe, expect, test } from 'bun:test';
import { DRAFT_KEY } from '../js/notes-sync.js';
import { mountNotes } from '../js/notes.js';
import { registerView } from '../js/router.js';
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
async function boot(extra = {}) {
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
    ...extra,
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

  test('a 401 forgets the persisted subscription state too, which becomes unknown', async () => {
    const { createPushController, SUBSCRIBED_KEY } = await import('../js/push.js');
    const env = /** @type {any} */ ({ serviceWorker: { ready: new Promise(() => {}) }, PushManager: {}, Notification: { permission: 'granted' } });
    localStorage.setItem(SUBSCRIBED_KEY, 'true');
    const push = createPushController({ request: async () => ({ kind: 'offline' }), env });
    expect(push.state.subscribed).toBe(true);
    const app = await boot({ push });
    localStorage.setItem(SUBSCRIBED_KEY, 'true');
    app.fleetScript.push({ kind: 'unpaired' });
    await app.start();
    expect(app.root.querySelector('.code')).not.toBeNull();
    expect(push.state.subscribed).toBeNull();
    expect(localStorage.getItem(SUBSCRIBED_KEY)).toBeNull();
  });
});

/** Des notifications à la main : l'état qu'on veut, et le compte des lectures demandées. */
function fakePush(state = {}) {
  const push = {
    state: { supported: true, permission: 'default', subscribed: /** @type {boolean | null} */ (false), busy: false, message: null, ...state },
    refreshes: 0,
    refresh() {
      this.refreshes++;
      return Promise.resolve();
    },
    subscribe: () => () => {},
    reset() {
      this.state.subscribed = null;
    },
    enable: async () => {},
    test: async () => {},
  };
  return /** @type {typeof push & ReturnType<typeof import('../js/push.js').createPushController>} */ (/** @type {unknown} */ (push));
}

describe('notifications in the frame', () => {
  test('the badge follows each fleet received, never the stale one from the cache', async () => {
    /** @type {number[]} */
    const badges = [];
    const syncBadge = async (/** @type {number} */ count) => {
      badges.push(count);
      return true;
    };
    const app = await boot({ syncBadge: /** @type {any} */ (syncBadge), push: fakePush() });
    app.fleetScript.push({ kind: 'ok', data: { ...FLEET, badge: 2 } });
    await app.start();
    expect(badges).toEqual([2]); // la flotte en cache (badge 0) n'a rien posé
    FakeEventSource.open()[0].emitFleet({ ...FLEET, badge: 5 });
    FakeEventSource.open()[0].emitFleet({ ...FLEET, badge: 0 });
    expect(badges).toEqual([2, 5, 0]);
  });

  test('a connection change alone does not set the badge again', async () => {
    /** @type {number[]} */
    const badges = [];
    const app = await boot({ syncBadge: async (/** @type {number} */ count) => (badges.push(count), true), push: fakePush() });
    await app.start();
    const before = badges.length;
    app.store.setConnection('offline');
    app.store.setConnection('ok');
    expect(badges.length).toBe(before);
  });

  test('the subscription is checked when the app opens and at each resume', async () => {
    const push = fakePush();
    const app = await boot({ push });
    await app.start();
    expect(push.refreshes).toBe(1);
    setVisibility('hidden');
    setVisibility('visible');
    await flush();
    expect(push.refreshes).toBe(2);
  });

  test('the settings screen gets the notifications and the timers of the frame', async () => {
    const { registerView } = await import('../js/router.js');
    /** @type {any} */
    let received = null;
    registerView('settings', (root, context) => {
      received = context;
      root.textContent = 'settings screen';
    });
    const push = fakePush();
    const app = await boot({ push });
    await app.start();
    expect(app.root.querySelector('.topbar')).toBeNull(); // la roue est dans l'en-tête de la flotte
    location.hash = '#/settings';
    await flush();
    expect(app.root.querySelector('.view')?.textContent).toBe('settings screen');
    expect(received.push).toBe(push);
    expect(received.timers).toBe(app.timers);
    expect(app.presenceCalls().filter((call) => call.body !== null)).toEqual([]);
  });

  test('the activation card gives way to a session, and come back', async () => {
    const app = await boot({ push: fakePush() });
    await app.start();
    const notice = /** @type {HTMLElement} */ (app.root.querySelector('.notice'));
    expect(notice.hidden).toBe(false);
    location.hash = '#/s/idema-1a2b3c4d/0';
    await flush();
    expect(notice.hidden).toBe(true);
    location.hash = '#/';
    await flush();
    expect(notice.hidden).toBe(false);
  });

  test('a device that is not subscribed sees the activation card above the fleet', async () => {
    const { createPushController } = await import('../js/push.js');
    const env = /** @type {any} */ ({ serviceWorker: { ready: new Promise(() => {}) }, PushManager: {}, Notification: { permission: 'default' } });
    const app = await boot({ push: createPushController({ request: async () => ({ kind: 'ok', data: { subscribed: false } }), env }) });
    await app.start();
    expect(app.root.querySelector('.notice')?.textContent).toContain('Activer les notifications');
  });
});

describe('a stored note draft outside the Notes tab', () => {
  /** Un Mac qui garde la note, derrière la fausse api de la flotte ; `hold` met la lecture de la note en attente. */
  function withScratchpad() {
    const inner = fakeApi();
    const disk = { text: 'old', at: 5 };
    /** @type {{method: string, body: any}[]} */
    const pad = [];
    /** @type {((result: any) => void)[]} */
    const held = [];
    const state = { hold: false };
    /** @type {import('../js/api.js').api} */
    const api = (method, path, body, timeout, keepalive, headers) => {
      if (path !== '/scratchpad') return inner.api(method, path, body, timeout, keepalive, headers);
      pad.push({ method, body });
      if (method === 'GET') {
        const reply = { kind: /** @type {const} */ ('ok'), data: { text: disk.text, modified_at: disk.at, editable: true } };
        return state.hold ? new Promise((resolve) => held.push(() => resolve(/** @type {any} */ (reply)))) : Promise.resolve(/** @type {any} */ (reply));
      }
      const put = /** @type {{text: string, base: number}} */ (body);
      if (put.base !== disk.at) return Promise.resolve({ kind: /** @type {const} */ ('http'), status: 409, error: 'note_changed' });
      disk.text = put.text;
      disk.at += 1;
      return Promise.resolve({ kind: /** @type {const} */ ('ok'), data: { ok: true, modified_at: disk.at } });
    };
    return { api, disk, pad, held, state, puts: () => pad.filter((c) => c.method === 'PUT'), gets: () => pad.filter((c) => c.method === 'GET') };
  }
  const putDraft = (/** @type {string} */ text) => localStorage.setItem(DRAFT_KEY, JSON.stringify({ text, base: 5 }));

  beforeEach(() => registerView('notes', () => {}));
  afterEach(() => registerView('notes', () => {}));

  test('the app starting on the fleet sends the draft, once, and clears it', async () => {
    const mac = withScratchpad();
    const app = await boot({ api: mac.api });
    putDraft('late draft');
    await app.start();
    expect(mac.puts().map((c) => /** @type {any} */ (c.body))).toEqual([{ text: 'late draft', base: 5 }]);
    expect(mac.disk.text).toBe('late draft');
    expect(localStorage.getItem(DRAFT_KEY)).toBeNull();
  });

  test('no draft means no request to the note at all', async () => {
    const mac = withScratchpad();
    const app = await boot({ api: mac.api });
    await app.start();
    expect(mac.pad).toEqual([]);
  });

  test('a resume sends a draft that is still waiting', async () => {
    const mac = withScratchpad();
    const app = await boot({ api: mac.api });
    await app.start();
    putDraft('typed elsewhere');
    setVisibility('hidden');
    setVisibility('visible');
    await flush();
    expect(mac.puts().map((c) => /** @type {any} */ (c.body).text)).toEqual(['typed elsewhere']);
  });

  test('the connection coming back sends a draft that waited offline', async () => {
    const mac = withScratchpad();
    const app = await boot({ api: mac.api });
    await app.start();
    app.store.setConnection('offline');
    putDraft('typed while offline');
    app.store.setConnection('ok');
    await flush();
    expect(mac.puts().map((c) => /** @type {any} */ (c.body).text)).toEqual(['typed while offline']);
  });

  test('with the Notes tab mounted no second engine starts: the view owns the draft', async () => {
    registerView('notes', mountNotes);
    const mac = withScratchpad();
    const app = await boot({ api: mac.api });
    putDraft('late draft');
    location.hash = '#/notes';
    await app.start();
    expect(mac.gets().length).toBe(1);
    expect(mac.puts().map((c) => /** @type {any} */ (c.body).text)).toEqual(['late draft']);
  });

  test('opening Notes while the background engine waits on the Mac hands over: one write, the late reply changes nothing', async () => {
    registerView('notes', mountNotes);
    const mac = withScratchpad();
    mac.state.hold = true;
    const app = await boot({ api: mac.api });
    putDraft('late draft');
    await app.start(); // le moteur sans écran attend la lecture
    expect(mac.gets().length).toBe(1);
    location.hash = '#/notes';
    await flush(); // l'onglet monte : sa propre lecture est aussi en attente
    expect(mac.gets().length).toBe(2);
    const [first, second] = mac.held;
    second(null);
    await flush();
    expect(mac.puts().map((c) => /** @type {any} */ (c.body).text)).toEqual(['late draft']);
    const stored = localStorage.getItem(DRAFT_KEY);
    first(null); // la réponse tardive du moteur arrêté
    await flush();
    expect(mac.puts().length).toBe(1);
    expect(localStorage.getItem(DRAFT_KEY)).toBe(stored);
  });

  test('leaving the Notes tab with a draft that did not go out starts the background engine', async () => {
    registerView('notes', () => {});
    const mac = withScratchpad();
    const app = await boot({ api: mac.api });
    location.hash = '#/notes';
    await app.start();
    putDraft('left behind');
    location.hash = '#/';
    await flush();
    await flush(); // le changement de fragment, puis la lecture et l'envoi
    expect(mac.puts().map((c) => /** @type {any} */ (c.body).text)).toEqual(['left behind']);
  });
});
