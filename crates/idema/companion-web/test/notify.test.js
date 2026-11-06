import { afterEach, describe, expect, test } from 'bun:test';
import { mountActivationCard, mountSettings } from '../js/notify.js';
import { createPushController, enableMessage, testMessage } from '../js/push.js';

const flush = () => new Promise((resolve) => setTimeout(resolve, 0));

const KEY = Buffer.from(Uint8Array.from({ length: 65 }, (_, index) => (index === 0 ? 4 : index))).toString('base64url');
const ok = (/** @type {unknown} */ data) => ({ kind: 'ok', data });
const failure = (/** @type {number} */ status, /** @type {string} */ error) => ({ kind: 'http', status, error });

/** Le navigateur et le Mac en faux, et le vrai contrôleur entre les deux : on touche, on lit ce que l'écran dit. */
function setup({ supported = true, permission = 'default', answer = 'granted', subscribed = false, routes = /** @type {Record<string, any>} */ ({}) } = {}) {
  /** @type {any} */
  const registration = {
    current: null,
    pushManager: {
      getSubscription: async () => registration.current,
      subscribe: async () => {
        registration.current = {
          options: { applicationServerKey: new Uint8Array(Buffer.from(KEY, 'base64url')).buffer },
          toJSON: () => ({ endpoint: 'https://web.push.apple.com/x', keys: { p256dh: 'p', auth: 'a' } }),
          unsubscribe: async () => void (registration.current = null),
        };
        return registration.current;
      },
    },
  };
  const env = /** @type {any} */ (
    supported
      ? {
          serviceWorker: { ready: Promise.resolve(registration) },
          PushManager: {},
          Notification: {
            permission,
            requestPermission: async () => {
              env.Notification.permission = answer;
              return answer;
            },
          },
        }
      : {}
  );
  /** @type {Record<string, any>} */
  const mac = { 'GET /push/key': ok({ key: KEY }), 'GET /me': ok({ subscribed }), ...routes };
  const request = async (/** @type {string} */ method, /** @type {string} */ path) => mac[`${method} ${path}`] ?? ok(null);
  const push = createPushController({ request, env });
  const host = document.createElement('div');
  document.body.replaceChildren(host);
  return { push, host, registration };
}

/** @param {HTMLElement} host @param {string} label */
const button = (host, label) => /** @type {HTMLButtonElement | undefined} */ ([...host.querySelectorAll('button')].find((node) => node.textContent === label));
/** @param {HTMLElement} host */
const status = (host) => /** @type {HTMLElement} */ (host.querySelector('.status'));

/** @type {(() => void)[]} */
let unmounts = [];
afterEach(() => {
  for (const unmount of unmounts) unmount();
  unmounts = [];
});

const mountCard = (/** @type {ReturnType<typeof setup>} */ app) => {
  unmounts.push(mountActivationCard(app.host, app.push));
};
const mountSettingsScreen = (/** @type {ReturnType<typeof setup>} */ app) => {
  const unmount = mountSettings(app.host, /** @type {any} */ ({ push: app.push }));
  unmounts.push(/** @type {() => void} */ (unmount));
};

describe('the activation card', () => {
  test('it is shown to a device that can subscribe and is not subscribed yet', async () => {
    const app = setup();
    await app.push.refresh();
    mountCard(app);
    expect(app.host.hidden).toBe(false);
    expect(app.host.textContent).toContain('Activer les notifications');
    expect(button(app.host, 'Activer les notifications')?.disabled).toBe(false);
  });

  test('it is not shown once subscribed, nor where push does not exist', async () => {
    const subscribed = setup({ permission: 'granted', subscribed: true });
    await subscribed.push.enable();
    mountCard(subscribed);
    expect(subscribed.host.hidden).toBe(true);
    const unsupported = setup({ supported: false });
    await unsupported.push.refresh();
    mountCard(unsupported);
    expect(unsupported.host.hidden).toBe(true);
  });

  test('touching the button asks for the permission and the card goes away on success', async () => {
    const app = setup();
    mountCard(app);
    button(app.host, 'Activer les notifications')?.click();
    await flush();
    expect(app.push.state.subscribed).toBe(true);
    expect(app.host.hidden).toBe(true);
    expect(app.registration.current).not.toBeNull();
  });

  test('a refusal stays on the card, in words, with no button left to press', async () => {
    const app = setup({ answer: 'denied' });
    mountCard(app);
    button(app.host, 'Activer les notifications')?.click();
    await flush();
    expect(app.host.hidden).toBe(false);
    expect(status(app.host).textContent).toBe(enableMessage('denied'));
    expect(status(app.host).className).not.toContain('status--ok');
    expect(button(app.host, 'Activer les notifications')).toBeUndefined();
  });

  test('a Mac that does not answer is told with the same sentence as the offline banner', async () => {
    const app = setup({ routes: { 'GET /push/key': { kind: 'offline' } } });
    mountCard(app);
    button(app.host, 'Activer les notifications')?.click();
    await flush();
    expect(status(app.host).textContent).toBe(enableMessage('offline'));
    expect(button(app.host, 'Activer les notifications')?.disabled).toBe(false); // on peut réessayer
  });

  test('the button is disabled while the activation runs', async () => {
    const app = setup();
    mountCard(app);
    button(app.host, 'Activer les notifications')?.click();
    expect(button(app.host, 'Activer les notifications')?.disabled).toBe(true);
    await flush();
  });
});

describe('the settings screen', () => {
  test('it says what the state is, in each case', async () => {
    const cases = /** @type {[string, Parameters<typeof setup>[0], string][]} */ ([
      ['disabled', {}, 'Désactivées'],
      ['denied', { permission: 'denied' }, 'Refusées'],
      ['unsupported', { supported: false }, 'Indisponibles'],
    ]);
    for (const [, options, text] of cases) {
      const app = setup(options);
      mountSettingsScreen(app);
      await flush();
      expect(app.host.textContent).toContain(text);
    }
    const app = setup({ permission: 'granted', subscribed: true });
    await app.push.enable();
    mountSettingsScreen(app);
    expect(app.host.textContent).toContain('Activées');
  });

  test('it always offers the test, and the limit of the badge is in the help text', async () => {
    const app = setup({ permission: 'granted', subscribed: true });
    mountSettingsScreen(app);
    expect(button(app.host, 'Envoyer une notification de test')?.disabled).toBe(false);
    expect(app.host.textContent).toContain('pastille');
    expect(app.host.textContent).not.toContain('—');
  });

  test('each answer of the Mac to the test is shown as its own sentence, the success in green', async () => {
    const cases = /** @type {[string, any][]} */ ([
      ['sent', ok({ result: 'sent' })],
      ['not_subscribed', failure(409, 'not_subscribed')],
      ['in_progress', failure(429, 'test_in_progress')],
      ['gone', failure(502, 'push_gone')],
      ['push_failed', failure(502, 'push_rejected')],
      ['offline', { kind: 'offline' }],
      ['timeout', { kind: 'timeout' }],
    ]);
    const seen = new Set();
    for (const [outcome, reply] of cases) {
      const app = setup({ permission: 'granted', subscribed: true, routes: { 'POST /push/test': reply } });
      mountSettingsScreen(app);
      button(app.host, 'Envoyer une notification de test')?.click();
      await flush();
      expect(status(app.host).textContent).toBe(testMessage(outcome));
      expect(status(app.host).className.includes('status--ok')).toBe(outcome === 'sent');
      seen.add(status(app.host).textContent);
    }
    expect(seen.size).toBe(cases.length);
  });

  test('the test is disabled while it runs, and where push does not exist', async () => {
    const app = setup({ permission: 'granted', subscribed: true });
    mountSettingsScreen(app);
    button(app.host, 'Envoyer une notification de test')?.click();
    expect(button(app.host, 'Envoyer une notification de test')?.disabled).toBe(true);
    await flush();
    const unsupported = setup({ supported: false });
    mountSettingsScreen(unsupported);
    expect(button(unsupported.host, 'Envoyer une notification de test')?.disabled).toBe(true);
  });

  test('a dead subscription brings the activation button back', async () => {
    const app = setup({ permission: 'granted', subscribed: true, routes: { 'POST /push/test': failure(502, 'push_gone') } });
    await app.push.enable(); // abonné d'abord
    mountSettingsScreen(app);
    expect(button(app.host, 'Activer les notifications')).toBeUndefined();
    button(app.host, 'Envoyer une notification de test')?.click();
    await flush();
    expect(button(app.host, 'Activer les notifications')).toBeDefined();
  });

  test('unmounting stops the screen from following the state', async () => {
    const app = setup();
    const unmount = /** @type {() => void} */ (mountSettings(app.host, /** @type {any} */ ({ push: app.push })));
    unmount();
    app.host.replaceChildren();
    await app.push.enable();
    expect(app.host.childElementCount).toBe(0);
  });
});
