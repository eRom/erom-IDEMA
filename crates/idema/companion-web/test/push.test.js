import { describe, expect, test } from 'bun:test';
import {
  createPushController,
  enableMessage,
  enableNotifications,
  ensureSubscriptionMatchesKey,
  isSupported,
  keyBytes,
  keysDiffer,
  sendTestNotification,
  syncBadge,
  testMessage,
} from '../js/push.js';

const flush = () => new Promise((resolve) => setTimeout(resolve, 0));

/** Un point non compressé factice : 65 octets dont le premier vaut 4, comme une clé P-256 publique. */
const keyOf = (/** @type {number} */ seed) => Uint8Array.from({ length: 65 }, (_, index) => (index === 0 ? 4 : (index * seed) % 251));
const base64Url = (/** @type {Uint8Array} */ bytes) => Buffer.from(bytes).toString('base64url');
const SERVER_KEY = base64Url(keyOf(7));
const OTHER_KEY = base64Url(keyOf(11));

/** @param {string} text */
const decode = (text) => new Uint8Array(Buffer.from(text, 'base64url'));

describe('keys', () => {
  test('the same key in base64url, with or without padding, as an ArrayBuffer or a view, is the same key', () => {
    const bytes = keyOf(7);
    expect(keysDiffer(SERVER_KEY, SERVER_KEY)).toBe(false);
    expect(keysDiffer(bytes.buffer, SERVER_KEY)).toBe(false);
    expect(keysDiffer(SERVER_KEY, bytes.buffer)).toBe(false);
    expect(keysDiffer(bytes, SERVER_KEY)).toBe(false);
    expect(keysDiffer(SERVER_KEY + '=', SERVER_KEY)).toBe(false);
    expect(keysDiffer(new Uint8Array([0, ...bytes]).subarray(1), SERVER_KEY)).toBe(false);
  });

  test('another key, a shorter one, a missing one and an unreadable one all differ', () => {
    expect(keysDiffer(OTHER_KEY, SERVER_KEY)).toBe(true);
    expect(keysDiffer(keyOf(7).slice(0, 64).buffer, SERVER_KEY)).toBe(true);
    expect(keysDiffer(null, SERVER_KEY)).toBe(true);
    expect(keysDiffer(undefined, undefined)).toBe(true);
    expect(keysDiffer('***not base64***', SERVER_KEY)).toBe(true);
  });

  test('the url-safe alphabet decodes to the bytes it encodes', () => {
    const bytes = Uint8Array.from([251, 255, 190, 63, 62]); // donne des « - » et des « _ »
    const text = base64Url(bytes);
    expect(text).toMatch(/[-_]/);
    expect(Array.from(keyBytes(text) ?? [])).toEqual(Array.from(bytes));
  });
});

/** Le navigateur : un service worker prêt, son gestionnaire d'abonnements, la permission. Tout est noté dans `log`. */
function fakeBrowser({ permission = 'default', answer = 'granted', existing = /** @type {Uint8Array | null} */ (null), never = false, subscribeFails = false, hideKey = false } = {}) {
  /** @type {any[]} */
  const log = [];
  /** @param {Uint8Array} key */
  const makeSubscription = (key) => ({
    options: { applicationServerKey: hideKey ? null : key.slice().buffer },
    toJSON: () => ({ endpoint: 'https://web.push.apple.com/abc', keys: { p256dh: 'p', auth: 'a' } }),
    unsubscribe: async () => {
      log.push('unsubscribe');
      registration.current = null;
      return true;
    },
  });
  const registration = {
    current: existing ? makeSubscription(existing) : null,
    pushManager: {
      getSubscription: async () => registration.current,
      /** @param {{userVisibleOnly: boolean, applicationServerKey: Uint8Array}} options */
      subscribe: async (options) => {
        log.push({ subscribe: options });
        if (subscribeFails) throw new DOMException('no', 'NotAllowedError');
        registration.current = makeSubscription(options.applicationServerKey);
        return registration.current;
      },
    },
  };
  const env = /** @type {any} */ ({
    serviceWorker: { ready: never ? new Promise(() => {}) : Promise.resolve(registration) },
    PushManager: {},
    Notification: {
      permission,
      requestPermission: async () => {
        log.push('permission');
        env.Notification.permission = answer;
        return answer;
      },
    },
  });
  return { env, log, registration };
}

/**
 * Un `request` à la main : les routes sont `"METHOD /chemin"` ; un chemin absent répond « ok sans corps ».
 * @param {Record<string, any>} routes
 */
function fakeRequest(routes = {}) {
  /** @type {{method: string, path: string, body: unknown, timeoutMs?: number}[]} */
  const calls = [];
  const request = async (/** @type {string} */ method, /** @type {string} */ path, /** @type {unknown} */ body, /** @type {boolean | undefined} */ _keepalive, /** @type {number | undefined} */ timeoutMs) => {
    calls.push({ method, path, body, timeoutMs });
    const route = routes[`${method} ${path}`];
    return typeof route === 'function' ? route() : (route ?? { kind: 'ok', data: null });
  };
  return { request, calls, called: (/** @type {string} */ path) => calls.filter((call) => call.path === path) };
}

const ok = (/** @type {unknown} */ data) => ({ kind: 'ok', data });
const failure = (/** @type {number} */ status, /** @type {string} */ error) => ({ kind: 'http', status, error });
const withKey = { 'GET /push/key': ok({ key: SERVER_KEY }) };

describe('enableNotifications', () => {
  test('the permission is asked before anything else, then the Mac key is used to subscribe and the result is saved', async () => {
    const browser = fakeBrowser();
    const mac = fakeRequest(withKey);
    const order = browser.log;
    const request = async (/** @type {string} */ method, /** @type {string} */ path, /** @type {unknown} */ body) => {
      order.push(`${method} ${path}`);
      return mac.request(method, path, body, false, undefined);
    };
    expect(await enableNotifications({ request, env: browser.env })).toBe('enabled');
    expect(order[0]).toBe('permission');
    expect(order[1]).toBe('GET /push/key');
    const subscribe = order.find((entry) => typeof entry === 'object')?.subscribe;
    expect(subscribe.userVisibleOnly).toBe(true);
    expect(Array.from(subscribe.applicationServerKey)).toEqual(Array.from(keyOf(7)));
    expect(mac.called('/push/subscription')).toEqual([
      { method: 'PUT', path: '/push/subscription', body: { endpoint: 'https://web.push.apple.com/abc', keys: { p256dh: 'p', auth: 'a' } }, timeoutMs: undefined },
    ]);
  });

  test('a refused or dismissed permission stops there, without touching the Mac', async () => {
    for (const [answer, outcome] of [['denied', 'denied'], ['default', 'dismissed']]) {
      const browser = fakeBrowser({ answer });
      const mac = fakeRequest(withKey);
      expect(await enableNotifications({ request: mac.request, env: browser.env })).toBe(outcome);
      expect(mac.calls).toEqual([]);
    }
  });

  test('a browser without push says so before asking anything', async () => {
    const mac = fakeRequest(withKey);
    const env = /** @type {any} */ ({ serviceWorker: undefined, Notification: undefined, PushManager: undefined });
    expect(isSupported(env)).toBe(false);
    expect(await enableNotifications({ request: mac.request, env })).toBe('unsupported');
    expect(mac.calls).toEqual([]);
  });

  test('a subscription made for another key is dropped before subscribing again', async () => {
    const browser = fakeBrowser({ existing: keyOf(11) });
    const mac = fakeRequest(withKey);
    expect(await enableNotifications({ request: mac.request, env: browser.env })).toBe('enabled');
    const kinds = browser.log.map((entry) => (typeof entry === 'string' ? entry : 'subscribe'));
    expect(kinds).toEqual(['permission', 'unsubscribe', 'subscribe']);
  });

  test('a subscription already on the right key is kept and sent again', async () => {
    const browser = fakeBrowser({ existing: keyOf(7) });
    const mac = fakeRequest(withKey);
    expect(await enableNotifications({ request: mac.request, env: browser.env })).toBe('enabled');
    expect(browser.log).toEqual(['permission']);
    expect(mac.called('/push/subscription')).toHaveLength(1);
  });

  test('each way it can fail has its own outcome', async () => {
    const cases = /** @type {[string, ReturnType<typeof fakeBrowser>, Record<string, any>][]} */ ([
      ['offline', fakeBrowser(), { 'GET /push/key': { kind: 'offline' } }],
      ['timeout', fakeBrowser(), { 'GET /push/key': { kind: 'timeout' } }],
      ['push_unavailable', fakeBrowser(), { 'GET /push/key': failure(503, 'push_unavailable') }],
      ['key_unavailable', fakeBrowser(), { 'GET /push/key': ok({ nokey: 1 }) }],
      ['subscribe_failed', fakeBrowser({ subscribeFails: true }), withKey],
      ['refused', fakeBrowser(), { ...withKey, 'PUT /push/subscription': failure(400, 'bad_subscription') }],
      ['save_failed', fakeBrowser(), { ...withKey, 'PUT /push/subscription': failure(500, 'internal') }],
      ['offline', fakeBrowser(), { ...withKey, 'PUT /push/subscription': { kind: 'offline' } }],
    ]);
    for (const [outcome, browser, routes] of cases) {
      expect(await enableNotifications({ request: fakeRequest(routes).request, env: browser.env })).toBe(outcome);
    }
  });

  test('a service worker that never gets ready does not hang the button', async () => {
    const browser = fakeBrowser({ never: true });
    const outcome = await enableNotifications({ request: fakeRequest(withKey).request, env: browser.env, readyTimeoutMs: 10 });
    expect(outcome).toBe('worker_unavailable');
  });
});

describe('ensureSubscriptionMatchesKey', () => {
  test('without the permission, or without a subscription, there is nothing to do and nothing is asked', async () => {
    const noPermission = fakeRequest(withKey);
    expect(await ensureSubscriptionMatchesKey({ request: noPermission.request, env: fakeBrowser({ permission: 'default' }).env })).toBe('none');
    expect(noPermission.calls).toEqual([]);
    const noSubscription = fakeRequest(withKey);
    expect(await ensureSubscriptionMatchesKey({ request: noSubscription.request, env: fakeBrowser({ permission: 'granted' }).env })).toBe('none');
    expect(noSubscription.called('/push/subscription')).toEqual([]);
  });

  test('the same key, already known to the Mac, changes nothing', async () => {
    const browser = fakeBrowser({ permission: 'granted', existing: keyOf(7) });
    const mac = fakeRequest(withKey);
    expect(await ensureSubscriptionMatchesKey({ request: mac.request, env: browser.env })).toBe('unchanged');
    expect(browser.log).toEqual([]);
    expect(mac.called('/push/subscription')).toEqual([]);
  });

  test('a different key unsubscribes, subscribes with the new one and tells the Mac', async () => {
    const browser = fakeBrowser({ permission: 'granted', existing: keyOf(11) });
    const mac = fakeRequest(withKey);
    expect(await ensureSubscriptionMatchesKey({ request: mac.request, env: browser.env })).toBe('resubscribed');
    expect(browser.log[0]).toBe('unsubscribe');
    expect(Array.from(browser.log[1].subscribe.applicationServerKey)).toEqual(Array.from(decode(SERVER_KEY)));
    expect(mac.called('/push/subscription')).toHaveLength(1);
  });

  test('a subscription the Mac forgot (a dead send removed it) is given back to it', async () => {
    const browser = fakeBrowser({ permission: 'granted', existing: keyOf(7) });
    const mac = fakeRequest(withKey);
    expect(await ensureSubscriptionMatchesKey({ request: mac.request, env: browser.env, serverSubscribed: false })).toBe('restored');
    expect(browser.log).toEqual([]);
    expect(mac.called('/push/subscription')).toHaveLength(1);
  });

  test('a key the browser does not reveal is not read as a rotation', async () => {
    const browser = fakeBrowser({ permission: 'granted', existing: keyOf(11), hideKey: true });
    expect(await ensureSubscriptionMatchesKey({ request: fakeRequest(withKey).request, env: browser.env })).toBe('unchanged');
    expect(browser.log).toEqual([]);
  });

  test('a Mac that cannot be reached leaves the subscription alone', async () => {
    const browser = fakeBrowser({ permission: 'granted', existing: keyOf(11) });
    const mac = fakeRequest({ 'GET /push/key': { kind: 'offline' } });
    expect(await ensureSubscriptionMatchesKey({ request: mac.request, env: browser.env })).toBe('failed');
    expect(browser.log).toEqual([]);
  });

  test('a new subscription the Mac refuses is reported as failed, not as done', async () => {
    const browser = fakeBrowser({ permission: 'granted', existing: keyOf(11) });
    const mac = fakeRequest({ ...withKey, 'PUT /push/subscription': failure(400, 'endpoint_not_allowed') });
    expect(await ensureSubscriptionMatchesKey({ request: mac.request, env: browser.env })).toBe('failed');
  });
});

describe('sendTestNotification', () => {
  test('the Mac answers become outcomes, and the call waits for the real send', async () => {
    const cases = /** @type {[string, any][]} */ ([
      ['sent', ok({ result: 'sent' })],
      ['not_subscribed', failure(409, 'not_subscribed')],
      ['in_progress', failure(429, 'test_in_progress')],
      ['gone', failure(502, 'push_gone')],
      ['push_failed', failure(502, 'push_rejected')],
      ['push_unavailable', failure(503, 'push_unavailable')],
      ['offline', { kind: 'offline' }],
      ['timeout', { kind: 'timeout' }],
      ['failed', failure(500, 'internal')],
    ]);
    for (const [outcome, reply] of cases) {
      const mac = fakeRequest({ 'POST /push/test': reply });
      expect(await sendTestNotification({ request: mac.request })).toBe(outcome);
      expect(mac.calls[0].timeoutMs).toBeGreaterThan(10_000); // le curl du Mac peut durer 10 s
    }
  });
});

describe('syncBadge', () => {
  test('a whole non-negative count is set, zero included', async () => {
    /** @type {number[]} */
    const set = [];
    const nav = { setAppBadge: async (/** @type {number} */ count) => void set.push(count) };
    expect(await syncBadge(3, nav)).toBe(true);
    expect(await syncBadge(0, nav)).toBe(true);
    expect(set).toEqual([3, 0]);
  });

  test('anything else, a refusal or a browser without badges, is false and harmless', async () => {
    /** @type {number[]} */
    const set = [];
    const nav = { setAppBadge: async (/** @type {number} */ count) => void set.push(count) };
    for (const badge of [-1, 1.5, Number.NaN, /** @type {any} */ ('3'), /** @type {any} */ (undefined)]) expect(await syncBadge(badge, nav)).toBe(false);
    expect(set).toEqual([]);
    expect(await syncBadge(2, {})).toBe(false);
    expect(await syncBadge(2, { setAppBadge: async () => Promise.reject(new DOMException('no', 'NotAllowedError')) })).toBe(false);
  });
});

describe('the sentences', () => {
  const ENABLE = ['enabled', 'unsupported', 'denied', 'dismissed', 'offline', 'timeout', 'push_unavailable', 'key_unavailable', 'worker_unavailable', 'subscribe_failed', 'refused', 'save_failed'];
  const TEST = ['sent', 'not_subscribed', 'in_progress', 'gone', 'push_failed', 'push_unavailable', 'offline', 'timeout', 'failed'];

  test('each outcome says something of its own, in French, with no raw code and no em dash', () => {
    for (const [outcomes, say] of /** @type {const} */ ([[ENABLE, enableMessage], [TEST, testMessage]])) {
      const texts = outcomes.map((outcome) => say(outcome));
      expect(new Set(texts).size).toBe(texts.length);
      for (const [index, text] of texts.entries()) {
        expect(text).not.toContain('—');
        expect(text).not.toContain(outcomes[index]);
        expect(text).toMatch(/[.?]$/);
      }
    }
  });

  test('an unknown outcome, even one named like an Object member, gets the generic sentence', () => {
    for (const outcome of ['constructor', '__proto__', 'toString', 'nope']) {
      expect(enableMessage(outcome)).toBe(enableMessage('nope'));
      expect(testMessage(outcome)).toBe(testMessage('nope'));
    }
    expect(enableMessage('nope')).not.toBe(enableMessage('enabled'));
  });

  test('a refusal points to the iPhone settings, and a dead subscription to the activation', () => {
    expect(enableMessage('denied')).toContain('Réglages');
    expect(testMessage('gone')).toContain('Réactivez');
    expect(testMessage('offline')).toContain('Tailscale');
  });
});

describe('createPushController', () => {
  test('a browser without push makes no request and says it is unsupported', async () => {
    const mac = fakeRequest();
    const push = createPushController({ request: mac.request, env: /** @type {any} */ ({}) });
    await push.refresh();
    expect(push.state.supported).toBe(false);
    expect(mac.calls).toEqual([]);
  });

  test('on opening, a browser subscription the Mac forgot is given back and the state says subscribed', async () => {
    const browser = fakeBrowser({ permission: 'granted', existing: keyOf(7) });
    const mac = fakeRequest({ ...withKey, 'GET /me': ok({ device: {}, subscribed: false }) });
    const push = createPushController({ request: mac.request, env: browser.env });
    await push.refresh();
    expect(push.state).toMatchObject({ supported: true, permission: 'granted', subscribed: true });
    expect(mac.called('/push/subscription')).toHaveLength(1);
  });

  test('on opening with no subscription, the state says not subscribed and the Mac is only read', async () => {
    const browser = fakeBrowser({ permission: 'default' });
    const mac = fakeRequest({ ...withKey, 'GET /me': ok({ device: {}, subscribed: false }) });
    const push = createPushController({ request: mac.request, env: browser.env });
    await push.refresh();
    expect(push.state.subscribed).toBe(false);
    expect(mac.calls.map((call) => call.method)).toEqual(['GET']);
  });

  test('a Mac out of reach keeps what was known', async () => {
    const browser = fakeBrowser({ permission: 'granted', existing: keyOf(7) });
    const mac = fakeRequest({ 'GET /me': { kind: 'offline' } });
    const push = createPushController({ request: mac.request, env: browser.env });
    push.state.subscribed = true;
    await push.refresh();
    expect(push.state.subscribed).toBe(true);
  });

  test('overlapping refreshes share one read of the Mac', async () => {
    const browser = fakeBrowser({ permission: 'granted', existing: keyOf(7) });
    const mac = fakeRequest({ ...withKey, 'GET /me': ok({ subscribed: true }) });
    const push = createPushController({ request: mac.request, env: browser.env });
    await Promise.all([push.refresh(), push.refresh(), push.refresh()]);
    expect(mac.called('/me')).toHaveLength(1);
    await push.refresh();
    expect(mac.called('/me')).toHaveLength(2);
  });

  test('the permission sheet is asked inside the touch, before any await', async () => {
    const browser = fakeBrowser();
    const push = createPushController({ request: fakeRequest(withKey).request, env: browser.env });
    const pending = push.enable();
    expect(browser.log).toEqual(['permission']); // aucune microtâche n'a tourné : c'est encore le geste
    await pending;
  });

  test('a successful activation says so and marks the device subscribed', async () => {
    const browser = fakeBrowser();
    const push = createPushController({ request: fakeRequest(withKey).request, env: browser.env });
    await push.enable();
    expect(push.state).toMatchObject({ subscribed: true, busy: false, permission: 'granted' });
    expect(push.state.message).toEqual({ text: enableMessage('enabled'), ok: true });
  });

  test('a refusal leaves the device unsubscribed and says where to change it', async () => {
    const browser = fakeBrowser({ answer: 'denied' });
    const push = createPushController({ request: fakeRequest(withKey).request, env: browser.env });
    await push.enable();
    expect(push.state).toMatchObject({ subscribed: false, permission: 'denied' });
    expect(push.state.message).toEqual({ text: enableMessage('denied'), ok: false });
  });

  test('a test that succeeds says so; two taps at once send one request', async () => {
    const browser = fakeBrowser({ permission: 'granted', existing: keyOf(7) });
    const mac = fakeRequest({ 'POST /push/test': async () => (await flush(), ok({ result: 'sent' })) });
    const push = createPushController({ request: mac.request, env: browser.env });
    await Promise.all([push.test(), push.test()]);
    expect(mac.called('/push/test')).toHaveLength(1);
    expect(push.state.message).toEqual({ text: testMessage('sent'), ok: true });
    expect(push.state.busy).toBe(false);
  });

  test('a dead subscription is dropped from the browser too, so that the next open does not give it back', async () => {
    const browser = fakeBrowser({ permission: 'granted', existing: keyOf(7) });
    const mac = fakeRequest({ 'POST /push/test': failure(502, 'push_gone'), 'GET /me': ok({ subscribed: false }), ...withKey });
    const push = createPushController({ request: mac.request, env: browser.env });
    push.state.subscribed = true;
    await push.test();
    expect(browser.log).toContain('unsubscribe');
    expect(browser.registration.current).toBeNull();
    expect(push.state.subscribed).toBe(false);
    expect(push.state.message?.text).toBe(testMessage('gone'));
    await push.refresh();
    expect(mac.called('/push/subscription')).toEqual([]);
  });

  test('subscribers hear every change and can leave', async () => {
    const browser = fakeBrowser();
    const push = createPushController({ request: fakeRequest(withKey).request, env: browser.env });
    let heard = 0;
    const leave = push.subscribe(() => heard++);
    await push.enable();
    const after = heard;
    expect(after).toBeGreaterThan(1); // « en cours », puis le résultat
    leave();
    await push.test();
    expect(heard).toBe(after);
  });
});
