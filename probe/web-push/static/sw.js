// Service worker de la sonde Web Push IdemA.

// Touches envoyées au Mac quand on appuie sur un bouton d'action de la notification.
const ACTION_KEYS = {
  'key-1': '1',
  'key-2': '2',
  'key-enter': 'Enter',
};

self.addEventListener('install', () => {
  self.skipWaiting();
});

self.addEventListener('activate', (event) => {
  event.waitUntil(self.clients.claim());
});

// Journal de la sonde. Un échec réseau ici ne peut rien afficher à personne : on le laisse en console.
async function logProbe(event, detail) {
  try {
    await fetch('/api/probe-log', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ event, detail }),
    });
  } catch (err) {
    console.warn('probe-log injoignable', err);
  }
}

self.addEventListener('push', (event) => {
  event.waitUntil((async () => {
    // Le service worker doit toujours afficher une notification, sinon Safari révoque l'abonnement.
    let data = null;
    let parseError = null;
    try {
      data = event.data ? event.data.json() : null;
    } catch (err) {
      parseError = String(err);
    }
    const session = (data && typeof data.session === 'string') ? data.session : '';
    const title = (data && data.title) || 'Notification IdemA';
    const body = (data && data.body) || 'Message illisible';

    await self.registration.showNotification(title, {
      body,
      tag: session || 'idema',
      data: { session, url: '/?session=' + encodeURIComponent(session) },
      actions: [
        { action: 'key-1', title: '1' },
        { action: 'key-2', title: '2' },
        { action: 'key-enter', title: 'Entrée' },
      ],
    });

    const badgeSupported = 'setAppBadge' in self.navigator;
    let badgeError = null;
    if (data && typeof data.badge === 'number' && badgeSupported) {
      try {
        await self.navigator.setAppBadge(data.badge);
      } catch (err) {
        badgeError = String(err);
      }
    }

    await logProbe('push-received', {
      session,
      badge: data ? data.badge ?? null : null,
      badgeSupported,
      badgeError,
      parseError,
      actionsSupported: 'actions' in Notification.prototype,
      maxActions: Notification.maxActions ?? null,
    });
  })());
});

self.addEventListener('notificationclick', (event) => {
  event.notification.close();
  const data = event.notification.data || {};
  const action = event.action || '';

  event.waitUntil((async () => {
    await logProbe('notification-click', { action, session: data.session });

    if (action) {
      const keys = ACTION_KEYS[action];
      if (keys === undefined) {
        await logProbe('notification-action-unknown', { action });
        return;
      }
      try {
        const response = await fetch('/api/reply', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ session: data.session || '', keys, source: 'notification-action' }),
        });
        await logProbe('notification-action-reply', { action, status: response.status });
      } catch (err) {
        console.warn('reply injoignable', err);
        await logProbe('notification-action-error', { action, message: String(err) });
      }
      return;
    }

    const url = data.url || '/';
    const windows = await self.clients.matchAll({ type: 'window', includeUncontrolled: true });
    for (const client of windows) {
      if ('focus' in client) {
        await client.focus();
        if ('navigate' in client) {
          try {
            await client.navigate(url);
          } catch (err) {
            await logProbe('client-navigate-error', { message: String(err) });
          }
        }
        return;
      }
    }
    await self.clients.openWindow(url);
  })());
});
