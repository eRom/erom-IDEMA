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

/** Le nom du cache d'une construction d'IdemA : sa version et l'empreinte de ses fichiers (`health`). @param {string} version @param {string} assets */
function cacheName(version, assets) {
  return `${CACHE_PREFIX}${version}-${assets}`;
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

/** La construction d'IdemA que le Mac annonce, ou `null` s'il ne répond pas. */
async function currentBuild() {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), HEALTH_TIMEOUT_MS);
  try {
    const response = await worker.fetch(HEALTH_URL, { cache: 'no-store', signal: controller.signal });
    const { version, assets } = await response.json();
    return typeof version === 'string' && version !== '' && typeof assets === 'string' && assets !== '' ? { version, assets } : null;
  } catch {
    return null;
  } finally {
    clearTimeout(timer);
  }
}

/** Un cache est complet quand la page d'accueil y est : `addAll` les pose toutes ou aucune. @param {string} name */
async function isComplete(name) {
  return (await (await worker.caches.open(name)).match('/')) !== undefined;
}

/**
 * Télécharge les fichiers de la construction courante dans leur cache, s'il n'y est pas déjà complet. Un échec
 * n'efface que ce que cet appel a créé, et ne fait jamais échouer l'installation. Les caches plus anciens restent
 * jusqu'au lancement suivant (`activeCache`).
 */
async function refreshAppCache() {
  try {
    const build = await currentBuild();
    if (build === null) return;
    const name = cacheName(build.version, build.assets);
    if (await worker.caches.has(name)) {
      if (await isComplete(name)) return;
      // Reste d'un téléchargement interrompu : on le reprend à zéro.
      await worker.caches.delete(name);
    }
    try {
      await (await worker.caches.open(name)).addAll(APP_FILES);
    } catch {
      await worker.caches.delete(name);
    }
  } catch {
    // Stockage ou réseau en défaut : l'app garde son cache précédent, le lancement suivant réessaie.
  }
}

/** Le nom du cache que ce worker sert, une fois choisi (le choix se refait à chaque lancement). @type {string | null} */
let activeName = null;

/**
 * Le plus récent des caches complets ; avec `prune`, les plus anciens sont retirés : c'est ici, au début d'un lancement
 * et avant toute lecture, que la version téléchargée au lancement précédent prend la place de l'ancienne.
 * @param {boolean} prune
 */
async function activeCache(prune) {
  if (activeName !== null && !prune) return activeName;
  const names = (await worker.caches.keys()).filter((name) => name.startsWith(CACHE_PREFIX));
  activeName = null;
  for (let index = names.length - 1; index >= 0 && activeName === null; index--) {
    if (await isComplete(names[index])) {
      activeName = names[index];
      if (prune) await Promise.all(names.slice(0, index).map((older) => worker.caches.delete(older)));
    }
  }
  return activeName;
}

/** @param {FetchEvent} event */
async function serveApp(event) {
  const { request } = event;
  const page = request.mode === 'navigate';
  // `waitUntil` se pose tout de suite, pendant la distribution de l'événement ; le téléchargement attend la fin du
  // choix du cache (la version nouvelle sert au lancement suivant).
  const chosen = activeCache(page).catch(() => null);
  if (page) event.waitUntil(chosen.then(refreshAppCache));
  const name = await chosen;
  // Une page s'ouvre sur `/` quel que soit le fragment ou les paramètres.
  const cached = name === null ? undefined : await (await worker.caches.open(name)).match(page ? '/' : request, { ignoreSearch: true });
  return cached ?? worker.fetch(request);
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
  event.waitUntil(refreshAppCache());
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
