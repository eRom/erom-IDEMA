// @ts-check
// La coque : installation, appairage, cadre (bandeau, écran, barre d'onglets), reprise, présence, mesures.
// Les écrans de la flotte, d'Absence, de Notes et de la session se branchent par `registerView` (router.js).

import { API_PREFIX, api } from './api.js';
import { el } from './dom.js';
import { icon } from './icons.js';
import { isStandalone, renderInstall } from './install.js';
import { bannerText, createRetry } from './offline.js';
import { renderPairing } from './pairing.js';
import { createPresence } from './presence.js';
import { startRouter, viewFor } from './router.js';
import { store } from './store.js';

const root = /** @type {HTMLElement} */ (document.getElementById('app'));
const METRIC_MAX_MS = 600_000;

const TABS = /** @type {const} */ ([
  { route: 'fleet', hash: '#/', label: 'Flotte' },
  { route: 'absence', hash: '#/absence', label: 'Absence' },
  { route: 'notes', hash: '#/notes', label: 'Notes' },
]);

/** @type {HTMLElement | null} */
let banner = null;
/** @type {HTMLElement | null} */
let view = null;
/** @type {HTMLElement | null} */
let tabBar = null;
/** @type {(() => void) | void} */
let unmountView;
/** @type {(() => void) | null} */
let stopRouter = null;
/** @type {EventSource | null} */
let stream = null;
/** @type {Promise<boolean> | null} */
let inflight = null;
/** Les mesures de démarrage à froid ne valent que pour un appareil déjà appairé au lancement. */
let coldStart = true;
let shown = /** @type {'install' | 'pairing' | 'app'} */ ('app');

/**
 * Toute requête de l'app : un 401 ramène à l'appairage, quel que soit l'écran qui l'a lancée.
 * @param {string} method @param {string} path @param {unknown} [body]
 */
async function request(method, path, body) {
  const result = await api(method, path, body);
  if (result.kind === 'unpaired') showPairing();
  return result;
}

/** @param {'shell_ready' | 'fleet_ready' | 'resume_ready'} name @param {number} ms */
function sendMetric(name, ms) {
  const clamped = Math.min(METRIC_MAX_MS, Math.max(0, Math.round(ms)));
  void request('POST', '/client-metrics', { name, ms: clamped });
}

const presence = createPresence((body) => void request('PUT', '/presence', body));

const retry = createRetry(() => void refreshFleet());

function closeStream() {
  stream?.close();
  stream = null;
}

function openStream() {
  closeStream();
  const source = new EventSource(API_PREFIX + '/events');
  source.addEventListener('fleet', (event) => {
    try {
      store.setFleet(JSON.parse(/** @type {MessageEvent} */ (event).data));
      store.setConnection('ok');
    } catch {
      // Un événement illisible est ignoré : le suivant, ou la relecture de la reprise, remet la flotte d'aplomb.
    }
  });
  // Le navigateur rouvrirait seul le flux ; on préfère relire la flotte, qui dit pourquoi il est tombé.
  source.onerror = () => {
    closeStream();
    void refreshFleet();
  };
  stream = source;
}

/** Relit la flotte ; vrai si elle est arrivée. Les appels simultanés n'en font qu'un. */
function refreshFleet() {
  inflight ??= (async () => {
    const result = await request('GET', '/fleet');
    if (result.kind === 'ok') {
      store.setFleet(result.data);
      store.setConnection('ok');
      retry.stop();
      if (!stream) openStream();
      return true;
    }
    if (result.kind !== 'unpaired') {
      // Un 5xx du service n'est pas « injoignable » : le Mac répond, mal. On le range avec le délai dépassé.
      store.setConnection(result.kind === 'offline' ? 'offline' : 'timeout');
      closeStream();
      if (document.visibilityState === 'visible') retry.start();
    }
    renderBanner();
    return false;
  })().finally(() => {
    inflight = null;
  });
  return inflight;
}

function renderBanner() {
  if (!banner) return;
  const { connection, fleetAt } = store.state;
  banner.hidden = connection === 'ok';
  banner.textContent = connection === 'ok' ? '' : bannerText(connection, fleetAt, Date.now());
}

/** @param {import('./router.js').Route} route */
function renderRoute(route) {
  if (!view || !tabBar) return;
  unmountView?.();
  unmountView = undefined;
  view.replaceChildren();
  // La session s'empile au-dessus de la flotte : plein écran, sans barre d'onglets.
  tabBar.hidden = route.name === 'session';
  for (const tab of tabBar.querySelectorAll('button')) {
    tab.setAttribute('aria-selected', String(tab.dataset.route === route.name));
  }
  const mount = viewFor(route.name);
  if (mount) unmountView = mount(view, { route, store, request });
  presence.setTarget(route.name === 'session' ? { session: route.session, pane: route.pane } : null);
}

function teardownApp() {
  stopRouter?.();
  stopRouter = null;
  unmountView?.();
  unmountView = undefined;
  retry.stop();
  closeStream();
  presence.setTarget(null);
  banner = view = tabBar = null;
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
  root.replaceChildren(banner, view, tabBar);
  renderBanner();
  stopRouter = startRouter(renderRoute);
  if (coldStart) sendMetric('shell_ready', performance.now());
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
  renderPairing(root, {
    onPaired: () => {
      location.hash = '#/';
      startApp();
    },
  });
}

/** Retour au premier plan : iOS a gelé la page, on relit la flotte, l'écran ouvert, et on rouvre le flux. */
async function resume() {
  if (shown !== 'app') return;
  const started = performance.now();
  closeStream();
  dispatchEvent(new Event('idema:resume'));
  if (await refreshFleet()) sendMetric('resume_ready', performance.now() - started);
}

document.addEventListener('visibilitychange', () => {
  if (document.visibilityState === 'hidden') {
    store.touchVisit();
    presence.setVisible(false);
    retry.stop();
    closeStream();
    return;
  }
  presence.setVisible(true);
  void resume();
});

store.subscribe(renderBanner);

async function boot() {
  if (!isStandalone()) {
    shown = 'install';
    renderInstall(root);
    return;
  }
  // Une flotte en cache s'affiche tout de suite ; sans elle, on demande d'abord au Mac si l'appareil est appairé.
  if (!store.state.fleet) {
    const me = await api('GET', '/me');
    if (me.kind === 'unpaired') {
      showPairing();
      return;
    }
  }
  startApp();
}

void boot();
