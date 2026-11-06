import { afterEach, describe, expect, test } from 'bun:test';
import { readdirSync } from 'node:fs';

const ORIGIN = 'http://localhost:3000';
const APP_FOLDER = new URL('..', import.meta.url).pathname;

/** L'URL d'un fichier du cache sans origine ni paramètres : `/js/api.js`. @param {any} request */
const pathOf = (request) => new URL(typeof request === 'string' ? request : request.url, ORIGIN).pathname;

/** Un `CacheStorage` à la main, dans l'ordre de création comme le vrai. */
function fakeCaches() {
  /** @type {Map<string, Map<string, Response>>} */
  const stores = new Map();
  const api = {
    stores,
    addAllCalls: 0,
    /** Chemins dont le téléchargement échoue (un 404 fait échouer `addAll` en entier). */
    failOn: /** @type {Set<string>} */ (new Set()),
    has: async (/** @type {string} */ name) => stores.has(name),
    keys: async () => [...stores.keys()],
    delete: async (/** @type {string} */ name) => stores.delete(name),
    open: async (/** @type {string} */ name) => {
      const entries = stores.get(name) ?? new Map();
      stores.set(name, entries);
      return {
        addAll: async (/** @type {string[]} */ urls) => {
          api.addAllCalls++;
          for (const url of urls) {
            if (api.failOn.has(url)) throw new TypeError('network');
            entries.set(url, new Response(`${name}:${url}`));
          }
        },
      };
    },
    match: async (/** @type {any} */ request) => {
      for (const entries of stores.values()) {
        const hit = entries.get(pathOf(request));
        if (hit) return hit.clone();
      }
      return undefined;
    },
  };
  return api;
}

let counter = 0;

/**
 * Charge `sw.js` dans un faux `ServiceWorkerGlobalScope` : le script s'exécute comme dans le navigateur, ses
 * écouteurs sont ceux qu'on déclenche ensuite.
 * @param {{version?: string | null, caches?: ReturnType<typeof fakeCaches>, windows?: any[], navigateFails?: boolean}} [options]
 */
async function loadWorker({ version = '1.0.0', caches = fakeCaches(), windows = [], navigateFails = false } = {}) {
  /** @type {Record<string, ((event: any) => void)[]>} */
  const listeners = {};
  const log = {
    shown: /** @type {{title: string, options: any}[]} */ ([]),
    badges: /** @type {number[]} */ ([]),
    opened: /** @type {string[]} */ ([]),
    focused: 0,
    navigated: /** @type {string[]} */ ([]),
    networkPaths: /** @type {string[]} */ ([]),
    claimed: 0,
    skipped: 0,
  };
  const client = {
    focus: async () => {
      log.focused++;
      return client;
    },
    navigate: async (/** @type {string} */ url) => {
      if (navigateFails) throw new TypeError('not controlled');
      log.navigated.push(url);
    },
  };
  const scope = {
    location: { origin: ORIGIN },
    addEventListener: (/** @type {string} */ type, /** @type {(event: any) => void} */ fn) => (listeners[type] ??= []).push(fn),
    skipWaiting: async () => void log.skipped++,
    caches,
    registration: {
      showNotification: async (/** @type {string} */ title, /** @type {any} */ options) => void log.shown.push({ title, options }),
    },
    navigator: {
      setAppBadge: async (/** @type {number} */ badge) => void log.badges.push(badge),
    },
    clients: {
      claim: async () => void log.claimed++,
      matchAll: async () => windows.map(() => client),
      openWindow: async (/** @type {string} */ url) => void log.opened.push(url),
    },
    fetch: async (/** @type {any} */ request) => {
      const path = pathOf(request);
      log.networkPaths.push(path);
      if (path === '/idema-api/health') {
        if (version === null) throw new TypeError('network');
        return Response.json({ ok: true, version });
      }
      return new Response(`network:${path}`);
    },
  };
  const saved = Object.getOwnPropertyDescriptor(globalThis, 'self');
  Object.defineProperty(globalThis, 'self', { value: scope, configurable: true, writable: true });
  try {
    const specifier = `../sw.js?load=${++counter}`;
    await import(specifier);
  } finally {
    if (saved) Object.defineProperty(globalThis, 'self', saved);
    else Reflect.deleteProperty(globalThis, 'self');
  }
  /**
   * Déclenche un événement et attend ce que le worker a mis dans `waitUntil`.
   * @param {string} type @param {Record<string, unknown>} [event]
   */
  async function dispatch(type, event = {}) {
    /** @type {Promise<unknown>[]} */
    const pending = [];
    /** @type {{responded: Promise<Response> | null}} */
    const outcome = { responded: null };
    const full = {
      ...event,
      waitUntil: (/** @type {Promise<unknown>} */ promise) => void pending.push(promise),
      respondWith: (/** @type {Promise<Response>} */ promise) => void (outcome.responded = Promise.resolve(promise)),
    };
    for (const fn of listeners[type] ?? []) fn(full);
    await Promise.all(pending);
    return outcome;
  }
  /** @param {string} path @param {{method?: string, mode?: string}} [init] */
  const fetchEvent = (path, { method = 'GET', mode = 'cors' } = {}) => ({ request: { url: ORIGIN + path, method, mode } });
  return { scope, log, caches, dispatch, fetchEvent };
}

/** Les chemins d'un cache de version. @param {ReturnType<typeof fakeCaches>} caches @param {string} name */
const cached = (caches, name) => [...(caches.stores.get(name)?.keys() ?? [])].sort();

/** Les fichiers de l'app tels que le dossier les a (sans `sw.js` ni les outils de test). */
function appFilesOnDisk() {
  const files = ['/', '/app.css', '/manifest.webmanifest'];
  for (const folder of ['js', 'icons']) {
    for (const name of readdirSync(APP_FOLDER + folder)) files.push(`/${folder}/${name}`);
  }
  return files.sort();
}

afterEach(() => {
  Reflect.deleteProperty(globalThis, 'self');
});

describe('install and the app cache', () => {
  test('the cache is named after the version and holds every file of the app folder', async () => {
    const worker = await loadWorker({ version: '2.4.1' });
    await worker.dispatch('install');
    const names = await worker.caches.keys();
    expect(names.length).toBe(1);
    expect(names[0]).toContain('2.4.1');
    // Un fichier oublié dans la liste du worker laisserait l'app blanche hors ligne.
    expect(cached(worker.caches, names[0])).toEqual(appFilesOnDisk());
    expect(worker.log.skipped).toBe(1);
  });

  test('a Mac that does not answer at install leaves no cache and does not fail the install', async () => {
    const worker = await loadWorker({ version: null });
    await worker.dispatch('install');
    expect(await worker.caches.keys()).toEqual([]);
  });

  test('a download that fails midway leaves no half cache, and the older complete one stays', async () => {
    const caches = fakeCaches();
    await (await caches.open('idema-1.0.0')).addAll(['/']);
    caches.addAllCalls = 0;
    caches.failOn.add('/js/shell.js');
    const worker = await loadWorker({ version: '1.1.0', caches });
    await worker.dispatch('install');
    expect(await caches.keys()).toEqual(['idema-1.0.0']);
  });

  test('activation takes over the open pages', async () => {
    const worker = await loadWorker();
    await worker.dispatch('activate');
    expect(worker.log.claimed).toBe(1);
  });
});

describe('serving the app', () => {
  test('a page opens from the cache whatever the fragment or the parameters, even with the Mac down', async () => {
    const caches = fakeCaches();
    await loadWorker({ caches }).then((worker) => worker.dispatch('install'));
    const offline = await loadWorker({ caches, version: null });
    const outcome = await offline.dispatch('fetch', offline.fetchEvent('/?from=notification', { mode: 'navigate' }));
    expect(await (await outcome.responded)?.text()).toBe('idema-1.0.0:/');
    const asset = await offline.dispatch('fetch', offline.fetchEvent('/js/api.js'));
    expect(await (await asset.responded)?.text()).toBe('idema-1.0.0:/js/api.js');
    expect(offline.log.networkPaths.filter((path) => path.startsWith('/js'))).toEqual([]);
  });

  test('a file the cache does not hold goes to the network', async () => {
    const worker = await loadWorker();
    const outcome = await worker.dispatch('fetch', worker.fetchEvent('/logos/claude.png'));
    expect(await (await outcome.responded)?.text()).toBe('network:/logos/claude.png');
  });

  test('the API, writes and the worker itself are never answered from the cache', async () => {
    const worker = await loadWorker();
    await worker.dispatch('install');
    for (const [path, init] of /** @type {const} */ ([
      ['/idema-api/fleet', {}],
      ['/idema-api/health', {}],
      ['/sw.js', {}],
      ['/js/api.js', { method: 'POST' }],
    ])) {
      expect((await worker.dispatch('fetch', worker.fetchEvent(path, init))).responded).toBeNull();
    }
  });

  test('a new version is downloaded at the launch, used by the next one, and the old cache goes', async () => {
    const caches = fakeCaches();
    await loadWorker({ caches, version: '1.0.0' }).then((worker) => worker.dispatch('install'));
    const upgraded = await loadWorker({ caches, version: '1.1.0' });
    const launch = await upgraded.dispatch('fetch', upgraded.fetchEvent('/', { mode: 'navigate' }));
    expect(await (await launch.responded)?.text()).toBe('idema-1.0.0:/'); // ce lancement reste sur l'ancienne
    expect(await caches.keys()).toEqual(['idema-1.1.0']);
    const next = await upgraded.dispatch('fetch', upgraded.fetchEvent('/', { mode: 'navigate' }));
    expect(await (await next.responded)?.text()).toBe('idema-1.1.0:/');
  });

  test('the same version is not downloaded again at each launch', async () => {
    const worker = await loadWorker();
    await worker.dispatch('install');
    const before = worker.caches.addAllCalls;
    await worker.dispatch('fetch', worker.fetchEvent('/', { mode: 'navigate' }));
    await worker.dispatch('fetch', worker.fetchEvent('/', { mode: 'navigate' }));
    expect(worker.caches.addAllCalls).toBe(before);
  });
});

describe('push', () => {
  /** @param {unknown} body @param {Awaited<ReturnType<typeof loadWorker>>} worker */
  const push = (worker, body) =>
    worker.dispatch('push', { data: body === undefined ? null : { json: () => (typeof body === 'function' ? body() : body) } });

  test('an episode shows its title and body, tags the session, keeps the url and sets the badge', async () => {
    const worker = await loadWorker();
    await push(worker, {
      kind: 'waiting',
      title: 'Claude attend · idema',
      body: 'Continuer ?',
      session: 'idema-1a2b3c4d',
      pane: 2,
      badge: 3,
      url: '/#/s/idema-1a2b3c4d/2',
    });
    expect(worker.log.shown).toHaveLength(1);
    const { title, options } = worker.log.shown[0];
    expect(title).toBe('Claude attend · idema');
    expect(options.body).toBe('Continuer ?');
    expect(options.tag).toBe('idema-1a2b3c4d');
    expect(options.data.url).toBe('/#/s/idema-1a2b3c4d/2');
    expect(worker.log.badges).toEqual([3]);
  });

  test('the test push has no session and no badge: it is shown and the badge is left alone', async () => {
    const worker = await loadWorker();
    await push(worker, { title: 'Notification de test', body: 'IdemA peut te joindre.', type: 'test', url: '/' });
    expect(worker.log.shown[0].title).toBe('Notification de test');
    expect(worker.log.shown[0].options.tag).toBe('idema');
    expect(worker.log.shown[0].options.data.url).toBe('/');
    expect(worker.log.badges).toEqual([]);
  });

  test('a badge of zero is set, so that the last answered session clears the icon', async () => {
    const worker = await loadWorker();
    await push(worker, { title: 't', body: 'b', session: 's', badge: 0, url: '/' });
    expect(worker.log.badges).toEqual([0]);
  });

  test('an unreadable body still shows the fallback notification, whatever fails', async () => {
    for (const body of [
      () => {
        throw new SyntaxError('not json');
      },
      undefined,
      null,
      'just text',
      42,
      [],
      {},
      { title: '', body: '   ', session: 7, badge: 'x' },
    ]) {
      const worker = await loadWorker();
      await push(worker, body);
      expect(worker.log.shown).toHaveLength(1);
      expect(worker.log.shown[0].title).toBe('IdemA');
      expect(worker.log.shown[0].options.body).toBe('Une session attend');
      expect(worker.log.shown[0].options.data.url).toBe('/');
      expect(worker.log.badges).toEqual([]);
    }
  });

  test('a badge that is not a whole non-negative number is ignored', async () => {
    for (const badge of [-1, 1.5, '3', Number.NaN, null, {}]) {
      const worker = await loadWorker();
      await push(worker, { title: 't', body: 'b', badge });
      expect(worker.log.badges).toEqual([]);
    }
  });

  test('a refused badge does not undo the notification', async () => {
    const worker = await loadWorker();
    worker.scope.navigator.setAppBadge = async () => {
      throw new DOMException('denied', 'NotAllowedError');
    };
    await push(worker, { title: 't', body: 'b', session: 's', badge: 2 });
    expect(worker.log.shown).toHaveLength(1);
  });

  test('a url that leaves the app is replaced by the app root', async () => {
    for (const url of ['https://evil.example/x', '//evil.example/x', '/\\evil.example', 'javascript:alert(1)', 5, null]) {
      const worker = await loadWorker();
      await push(worker, { title: 't', body: 'b', url });
      expect(worker.log.shown[0].options.data.url).toBe('/');
    }
  });
});

describe('notificationclick', () => {
  /** @param {Awaited<ReturnType<typeof loadWorker>>} worker @param {unknown} data */
  const click = async (worker, data) => {
    let closed = 0;
    await worker.dispatch('notificationclick', { notification: { close: () => void closed++, data } });
    return closed;
  };

  test('with the app open, it comes forward on the pane the notification names', async () => {
    const worker = await loadWorker({ windows: [{}] });
    expect(await click(worker, { url: '/#/s/idema-1a2b3c4d/2' })).toBe(1);
    expect(worker.log.focused).toBe(1);
    expect(worker.log.navigated).toEqual(['/#/s/idema-1a2b3c4d/2']);
    expect(worker.log.opened).toEqual([]);
  });

  test('the test notification brings the app forward without reloading it', async () => {
    const worker = await loadWorker({ windows: [{}] });
    await click(worker, { url: '/' });
    expect(worker.log.focused).toBe(1);
    expect(worker.log.navigated).toEqual([]);
  });

  test('with no window open, it opens one on the pane', async () => {
    const worker = await loadWorker();
    await click(worker, { url: '/#/s/idema-1a2b3c4d/0' });
    expect(worker.log.opened).toEqual(['/#/s/idema-1a2b3c4d/0']);
  });

  test('a window that refuses to navigate is replaced by a new one', async () => {
    const worker = await loadWorker({ windows: [{}], navigateFails: true });
    await click(worker, { url: '/#/s/a/1' });
    expect(worker.log.opened).toEqual(['/#/s/a/1']);
  });

  test('a notification without data, or with a hostile url, opens the app root', async () => {
    for (const data of [undefined, null, {}, { url: 'https://evil.example/' }]) {
      const worker = await loadWorker();
      await click(worker, data);
      expect(worker.log.opened).toEqual(['/']);
    }
  });
});
