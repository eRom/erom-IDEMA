// @ts-check
// La coque : installation, appairage, cadre, reprise, présence, mesures. Les écrans s'y branchent par `registerView`
// (router.js). Réseau, flux et minuteries sont injectés : les tests de cycle de vie y mettent des faux.

import { API_PREFIX, api as realApi } from './api.js';
import { el } from './dom.js';
import { icon } from './icons.js';
import { isStandalone, renderInstall } from './install.js';
import { bannerText, createRetry } from './offline.js';
import { mountActivationCard } from './notify.js';
import { renderPairing } from './pairing.js';
import { createPresence } from './presence.js';
import { createPushController, syncBadge as realSyncBadge } from './push.js';
import { startRouter, viewFor } from './router.js';
import { realTimers } from './time.js';

const METRIC_MAX_MS = 600_000;
/** Attente avant de rouvrir un flux tombé : 2 s, puis 5 s. */
const STREAM_BACKOFF_MS = [2000, 5000];

const TABS = /** @type {const} */ ([
  { route: 'fleet', hash: '#/', label: 'Flotte' },
  { route: 'absence', hash: '#/absence', label: 'Absence' },
  { route: 'notes', hash: '#/notes', label: 'Notes' },
]);

/**
 * @param {object} deps
 * @param {HTMLElement} deps.root
 * @param {ReturnType<typeof import('./store.js').createStore>} deps.store
 * @param {typeof realApi} [deps.api]
 * @param {typeof EventSource} [deps.EventSource]
 * @param {import('./time.js').Timers} [deps.timers]
 * @param {ReturnType<typeof createPushController>} [deps.push] les notifications (par défaut, celles du navigateur)
 * @param {typeof realSyncBadge} [deps.syncBadge]
 */
export function createShell({
  root,
  store,
  api = realApi,
  EventSource: Source = globalThis.EventSource,
  timers = realTimers,
  push: injectedPush,
  syncBadge = realSyncBadge,
}) {
  /** @type {HTMLElement | null} */
  let banner = null;
  /** @type {HTMLElement | null} */
  let view = null;
  /** @type {HTMLElement | null} */
  let tabBar = null;
  /** @type {HTMLElement | null} */
  let notice = null;
  /** @type {(() => void) | null} */
  let unmountCard = null;
  /** @type {(() => void) | void} */
  let unmountView;
  /** @type {(() => void) | null} */
  let stopRouter = null;
  /** @type {EventSource | null} */
  let stream = null;
  /** @type {any} */
  let streamTimer = null;
  let streamFailures = 0;
  /** Numéro de la dernière lecture de la flotte : une réponse qui n'a plus le sien est ignorée. */
  let generation = 0;
  /** Les mesures à froid ne valent que pour un appareil déjà appairé au lancement. */
  let coldStart = true;
  let shown = /** @type {'install' | 'pairing' | 'app'} */ ('app');

  const visible = () => document.visibilityState === 'visible';

  /**
   * Toute requête de l'app : un 401 ramène à l'appairage, quel que soit l'écran qui l'a lancée.
   * @param {string} method @param {string} path @param {unknown} [body] @param {boolean} [keepalive] @param {number} [timeoutMs]
   */
  async function request(method, path, body, keepalive = false, timeoutMs = undefined) {
    const result = await api(method, path, body, timeoutMs, keepalive);
    if (result.kind === 'unpaired') showPairing();
    return result;
  }

  /** @param {'shell_ready' | 'fleet_ready' | 'resume_ready'} name @param {number} ms */
  function sendMetric(name, ms) {
    const clamped = Math.min(METRIC_MAX_MS, Math.max(0, Math.round(ms)));
    void request('POST', '/client-metrics', { name, ms: clamped });
  }

  const push = injectedPush ?? createPushController({ request });
  // La pastille suit la flotte reçue (pas celle du cache au lancement, qui peut être ancienne).
  let badgedFleet = store.state.fleet;
  const presence = createPresence((body, keepalive) => void request('PUT', '/presence', body, keepalive), undefined, timers);
  const retry = createRetry(() => void refreshFleet(), undefined, timers);

  function closeStream() {
    stream?.close();
    stream = null;
  }

  function stopStreamRetry() {
    if (streamTimer !== null) timers.clearTimeout(streamTimer);
    streamTimer = null;
  }

  function openStream() {
    closeStream();
    const source = new Source(API_PREFIX + '/events');
    source.addEventListener('fleet', (event) => {
      streamFailures = 0;
      try {
        store.setFleet(JSON.parse(/** @type {MessageEvent} */ (event).data));
        store.setConnection('ok');
      } catch {
        // Un événement illisible est ignoré : le suivant, ou la relecture de la reprise, remet la flotte d'aplomb.
      }
    });
    // Le navigateur rouvrirait le flux sans attendre : on relit la flotte, après un délai.
    source.onerror = () => {
      closeStream();
      if (!visible()) return;
      stopStreamRetry();
      const delay = STREAM_BACKOFF_MS[Math.min(streamFailures++, STREAM_BACKOFF_MS.length - 1)];
      streamTimer = timers.setTimeout(() => {
        streamTimer = null;
        void refreshFleet();
      }, delay);
    };
    stream = source;
  }

  /** Relit la flotte ; vrai si elle est arrivée. Chaque appel est une requête neuve. */
  async function refreshFleet() {
    const mine = ++generation;
    const result = await request('GET', '/fleet');
    if (mine !== generation) return false;
    if (result.kind === 'ok') {
      store.setFleet(result.data);
      store.setConnection('ok');
      retry.stop();
      if (!stream && visible()) openStream();
      return true;
    }
    if (result.kind !== 'unpaired') {
      // Un 5xx : le Mac répond, mal. Rangé avec le délai dépassé.
      store.setConnection(result.kind === 'offline' ? 'offline' : 'timeout');
      closeStream();
      if (visible()) retry.start();
    }
    renderBanner();
    return false;
  }

  function renderBanner() {
    if (!banner) return;
    const { connection, fleetAt } = store.state;
    banner.hidden = connection === 'ok';
    banner.textContent = connection === 'ok' ? '' : bannerText(connection, fleetAt, Date.now());
  }

  /** @param {import('./router.js').Route} route */
  function renderRoute(route) {
    if (!view || !tabBar || !notice) return;
    unmountView?.();
    unmountView = undefined;
    view.replaceChildren();
    // La session s'empile au-dessus de la flotte : plein écran, sans barre d'onglets.
    tabBar.hidden = notice.hidden = route.name === 'session';
    for (const tab of tabBar.querySelectorAll('button')) {
      tab.setAttribute('aria-selected', String(tab.dataset.route === route.name));
    }
    const mount = viewFor(route.name);
    if (mount) unmountView = mount(view, { route, store, request, push, timers });
    presence.setTarget(route.name === 'session' ? { session: route.session, pane: route.pane } : null);
  }

  function teardownApp() {
    stopRouter?.();
    stopRouter = null;
    unmountView?.();
    unmountView = undefined;
    retry.stop();
    stopStreamRetry();
    closeStream();
    generation++;
    presence.setTarget(null);
    unmountCard?.();
    unmountCard = null;
    banner = view = tabBar = notice = null;
  }

  function startApp() {
    teardownApp();
    shown = 'app';
    banner = el('div', { class: 'banner', role: 'status', hidden: true });
    view = el('main', { class: 'view' });
    tabBar = el(
      'nav',
      { class: 'tabs', role: 'tablist', 'aria-label': 'Navigation' },
      TABS.map((tab) =>
        el(
          'button',
          {
            class: 'tab',
            type: 'button',
            role: 'tab',
            'data-route': tab.route,
            'aria-selected': 'false',
            onclick: () => {
              location.hash = tab.hash;
            },
          },
          icon(tab.route),
          el('span', null, tab.label),
        ),
      ),
    );
    const card = el('div');
    notice = el('div', { class: 'notice' }, card);
    unmountCard = mountActivationCard(card, push);
    root.replaceChildren(banner, notice, view, tabBar);
    renderBanner();
    stopRouter = startRouter(renderRoute);
    if (coldStart) sendMetric('shell_ready', performance.now());
    void push.refresh();
    void refreshFleet().then((arrived) => {
      if (arrived && coldStart) sendMetric('fleet_ready', performance.now());
      coldStart = false;
    });
  }

  function showPairing() {
    if (shown === 'pairing') return;
    teardownApp();
    shown = 'pairing';
    coldStart = false;
    // Plus appairé : la flotte en cache n'est plus à lui.
    store.clearFleet();
    push.reset();
    renderPairing(root, {
      onPaired: () => {
        location.hash = '#/';
        startApp();
      },
    });
  }

  /** Retour au premier plan : on relit la flotte et l'écran ouvert, on rouvre le flux. */
  async function resume() {
    if (shown !== 'app') return;
    const started = performance.now();
    closeStream();
    stopStreamRetry();
    dispatchEvent(new Event('idema:resume'));
    void push.refresh();
    if (await refreshFleet()) sendMetric('resume_ready', performance.now() - started);
  }

  function onVisibility() {
    if (!visible()) {
      store.touchVisit();
      // Ce qui est en vol ne vaut plus rien au retour : sa réponse, ou son abandon tardif, sera ignorée.
      generation++;
      presence.setVisible(false);
      retry.stop();
      stopStreamRetry();
      closeStream();
      return;
    }
    presence.setVisible(true);
    void resume();
  }

  document.addEventListener('visibilitychange', onVisibility);
  const unsubscribe = store.subscribe((state) => {
    renderBanner();
    if (state.fleet && state.fleet !== badgedFleet) {
      badgedFleet = state.fleet;
      void syncBadge(state.fleet.badge);
    }
  });

  return {
    async boot() {
      if (!isStandalone()) {
        shown = 'install';
        renderInstall(root);
        return;
      }
      // Flotte en cache : affichée tout de suite ; sinon on demande d'abord au Mac si l'appareil est appairé.
      if (!store.state.fleet) {
        const me = await api('GET', '/me');
        if (me.kind === 'unpaired') {
          showPairing();
          return;
        }
      }
      startApp();
    },
    /** Arrête tout : écouteurs, minuteries, flux. */
    dispose() {
      document.removeEventListener('visibilitychange', onVisibility);
      unsubscribe();
      teardownApp();
    },
  };
}
