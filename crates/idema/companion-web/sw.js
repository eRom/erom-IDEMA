// Service worker d'IdemA, à ce chemin pour remplacer celui de la sonde déjà installé sur l'iPhone.
// Coque minimale : il prend la main tout de suite ; le cache et les notifications arrivent avec la PWA complète.

self.addEventListener('install', () => {
  self.skipWaiting();
});

self.addEventListener('activate', (event) => {
  event.waitUntil(self.clients.claim());
});
