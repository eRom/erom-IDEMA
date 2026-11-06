// @ts-check
// Service worker d'IdemA, à ce chemin pour remplacer celui de la sonde déjà installé sur l'iPhone.
// Coque minimale : il prend la main tout de suite ; le cache et les notifications arrivent avec la tâche suivante.

const worker = /** @type {ServiceWorkerGlobalScope} */ (/** @type {unknown} */ (self));

worker.addEventListener('install', () => {
  void worker.skipWaiting();
});

worker.addEventListener('activate', (event) => {
  event.waitUntil(worker.clients.claim());
});
