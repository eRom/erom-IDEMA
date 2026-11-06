// @ts-check
// Service worker d'IdemA, à ce chemin pour remplacer celui de la sonde déjà installé sur l'iPhone.
// Script classique (pas de module : l'enregistrement iOS reste celui de la sonde) : tout tient dans ce fichier.
// Trois métiers : garder les fichiers de l'app pour ouvrir sans le Mac, afficher chaque push, ramener l'app au toucher.

const worker = /** @type {ServiceWorkerGlobalScope} */ (/** @type {unknown} */ (self));

const CACHE_PREFIX = 'idema-';
const HEALTH_URL = '/idema-api/health';
const HEALTH_TIMEOUT_MS = 4000;
const API_PREFIX = '/idema-api/';

/** Les fichiers de l'app, sans `sw.js` (le navigateur le gère) ni les logos (servis par le Mac). */
const APP_FILES = [
  '/',
  '/app.css',
  '/manifest.webmanifest',
  '/icons/icon-180.png',
  '/icons/icon-512.png',
  '/js/api.js',
  '/js/dom.js',
  '/js/icons.js',
  '/js/install.js',
  '/js/main.js',
  '/js/notify.js',
  '/js/offline.js',
  '/js/pairing.js',
  '/js/presence.js',
  '/js/push.js',
  '/js/router.js',
  '/js/shell.js',
  '/js/store.js',
  '/js/time.js',
];

/** Le nom du cache d'une version d'IdemA. @param {string} version */
function cacheName(version) {
  return CACHE_PREFIX + version;
}

/** Une adresse de l'app (chemin de l'origine, fragment compris), sinon `/`. @param {unknown} value */
function appUrl(value) {
  if (typeof value !== 'string') return '/';
  try {
    return new URL(value, worker.location.origin).origin === worker.location.origin && value.startsWith('/') ? value : '/';
  } catch {
    return '/';
  }
}

/**
 * Ce qu'un push dit, sans jamais lever : un corps illisible donne la notification de repli, car un push sans
 * notification fait révoquer l'abonnement par Safari. Le push de test n'a ni `session` ni `badge`.
 * @param {{json(): unknown} | null | undefined} data
 */
function parsePushPayload(data) {
  /** @type {Record<string, unknown>} */
  let fields = {};
  try {
    const raw = data ? data.json() : null;
    if (raw && typeof raw === 'object') fields = /** @type {Record<string, unknown>} */ (raw);
  } catch {
    // Corps illisible : les valeurs de repli ci-dessous.
  }
  /** @param {unknown} value @param {string} fallback */
  const text = (value, fallback) => (typeof value === 'string' && value.trim() !== '' ? value : fallback);
  const badge = fields.badge;
  return {
    title: text(fields.title, 'IdemA'),
    body: text(fields.body, 'Une session attend'),
    session: typeof fields.session === 'string' ? fields.session : '',
    badge: typeof badge === 'number' && Number.isInteger(badge) && badge >= 0 ? badge : null,
    url: appUrl(fields.url),
  };
}

/** @param {ReturnType<typeof parsePushPayload>} payload */
async function showPush(payload) {
  await worker.registration.showNotification(payload.title, {
    body: payload.body,
    tag: payload.session || 'idema',
    data: { url: payload.url },
  });
  if (payload.badge === null) return;
  try {
    await worker.navigator.setAppBadge(payload.badge);
  } catch {
    // Pastille refusée (permission retirée) : la notification est déjà affichée, la page la reposera à l'ouverture.
  }
}

/** La version d'IdemA que le Mac annonce, ou `null` s'il ne répond pas. */
async function currentVersion() {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), HEALTH_TIMEOUT_MS);
  try {
    const response = await worker.fetch(HEALTH_URL, { cache: 'no-store', signal: controller.signal });
    const { version } = await response.json();
    return typeof version === 'string' && version !== '' ? version : null;
  } catch {
    return null;
  } finally {
    clearTimeout(timer);
  }
}

/**
 * Met en cache les fichiers de la version courante, puis retire les caches des autres. Un échec en cours de route
 * efface le cache entamé : il n'en reste que de complets. `force` reprend les fichiers même si le cache existe (un
 * `sw.js` nouveau vient d'une version nouvelle de l'app).
 * @param {boolean} [force]
 */
async function refreshAppCache(force = false) {
  const version = await currentVersion();
  if (version === null) return;
  const name = cacheName(version);
  if (!force && (await worker.caches.has(name))) return;
  try {
    const cache = await worker.caches.open(name);
    await cache.addAll(APP_FILES);
  } catch {
    await worker.caches.delete(name);
    return;
  }
  for (const key of await worker.caches.keys()) {
    if (key.startsWith(CACHE_PREFIX) && key !== name) await worker.caches.delete(key);
  }
}

/** @param {FetchEvent} event */
async function serveApp(event) {
  const { request } = event;
  const page = request.mode === 'navigate';
  // Une page s'ouvre sur `/` quel que soit le fragment ou les paramètres.
  const lookup = worker.caches.match(page ? '/' : request, { ignoreSearch: true });
  // La version nouvelle est reprise pour le lancement suivant, pas pour celui-ci. `waitUntil` se pose tout de suite,
  // pendant la distribution de l'événement.
  if (page) event.waitUntil(refreshAppCache());
  return (await lookup) ?? worker.fetch(request);
}

/** @param {string} url */
async function openApp(url) {
  const windows = await worker.clients.matchAll({ type: 'window', includeUncontrolled: true });
  const target = windows[0];
  if (target) {
    try {
      const focused = await target.focus();
      // Revenir à `/` rechargerait la page pour rien : le test de notification ramène seulement l'app.
      if (url !== '/') await focused.navigate(url);
      return;
    } catch {
      // Fenêtre qui refuse le focus ou la navigation : on en ouvre une.
    }
  }
  await worker.clients.openWindow(url);
}

worker.addEventListener('install', (event) => {
  void worker.skipWaiting();
  event.waitUntil(refreshAppCache(true));
});

worker.addEventListener('activate', (event) => {
  event.waitUntil(worker.clients.claim());
});

worker.addEventListener('fetch', (event) => {
  const { request } = event;
  const url = new URL(request.url);
  const mine = url.origin === worker.location.origin && !url.pathname.startsWith(API_PREFIX) && url.pathname !== '/sw.js';
  if (request.method === 'GET' && mine) event.respondWith(serveApp(event));
});

worker.addEventListener('push', (event) => {
  event.waitUntil(showPush(parsePushPayload(event.data)));
});

worker.addEventListener('notificationclick', (event) => {
  event.notification.close();
  event.waitUntil(openApp(appUrl(event.notification.data?.url)));
});
