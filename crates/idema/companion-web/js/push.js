// @ts-check
// Les notifications côté page : abonnement (le geste exigé par iOS), clé du Mac, test, pastille de l'icône.
// Le service worker (sw.js) affiche les push ; ici on s'abonne et on garde l'abonnement d'aplomb.

/** Attente du service worker prêt : passé ce délai, il n'est pas enregistré et rien ne viendra. */
export const READY_TIMEOUT_MS = 8000;
/** Le test attend l'envoi réel vers le service push (10 s de curl côté Mac), donc plus que les 4 s ordinaires. */
export const TEST_TIMEOUT_MS = 15_000;

/**
 * Ce que la page demande au navigateur, lu à l'appel : les tests y mettent des faux.
 * @typedef {object} PushEnv
 * @property {ServiceWorkerContainer | undefined} serviceWorker
 * @property {typeof Notification | undefined} Notification
 * @property {unknown} PushManager
 */

/**
 * @typedef {(method: string, path: string, body?: unknown, keepalive?: boolean, timeoutMs?: number)
 *   => Promise<import('./api.js').ApiResult>} Request
 */

/** @returns {PushEnv} */
export function browserEnv() {
  return {
    get serviceWorker() {
      return navigator.serviceWorker;
    },
    get Notification() {
      return globalThis.Notification;
    },
    get PushManager() {
      return /** @type {any} */ (globalThis).PushManager;
    },
  };
}

/** @param {PushEnv} env */
export function isSupported(env) {
  return Boolean(env.serviceWorker && env.Notification && env.PushManager);
}

/** Enregistre `/sw.js` (le même chemin que la sonde, qu'il remplace). Rend faux si le navigateur le refuse. @param {PushEnv} env */
export async function registerWorker(env) {
  if (!env.serviceWorker) return false;
  try {
    await env.serviceWorker.register('/sw.js');
    return true;
  } catch {
    return false;
  }
}

// --- Clés ---

/** @param {string} text base64url, avec ou sans bourrage @returns {Uint8Array | null} */
function decodeBase64Url(text) {
  try {
    const binary = atob(text.replace(/-/g, '+').replace(/_/g, '/').padEnd(Math.ceil(text.length / 4) * 4, '='));
    return Uint8Array.from(binary, (char) => char.charCodeAt(0));
  } catch {
    return null;
  }
}

/**
 * Les octets d'une clé donnée en base64url (texte) ou en `ArrayBuffer` / vue ; `null` si illisible.
 * @param {unknown} key
 * @returns {Uint8Array | null}
 */
export function keyBytes(key) {
  if (typeof key === 'string') return decodeBase64Url(key);
  if (key instanceof ArrayBuffer) return new Uint8Array(key);
  if (ArrayBuffer.isView(key)) return new Uint8Array(key.buffer, key.byteOffset, key.byteLength);
  return null;
}

/** Les deux clés sont-elles différentes ? Une clé illisible ou absente ne vaut jamais la même. @param {unknown} a @param {unknown} b */
export function keysDiffer(a, b) {
  const left = keyBytes(a);
  const right = keyBytes(b);
  return !left || !right || left.length !== right.length || left.some((byte, index) => byte !== right[index]);
}

// --- Abonnement ---

/** @param {import('./api.js').ApiResult} reply @param {string} fallback */
function failureOf(reply, fallback) {
  if (reply.kind === 'offline' || reply.kind === 'timeout') return reply.kind;
  if (reply.kind === 'http' && reply.status === 503) return 'push_unavailable';
  return fallback;
}

/** @param {Request} request @returns {Promise<{key: string} | {outcome: string}>} */
async function fetchKey(request) {
  const reply = await request('GET', '/push/key');
  if (reply.kind === 'ok' && typeof reply.data?.key === 'string') return { key: reply.data.key };
  return { outcome: failureOf(reply, 'key_unavailable') };
}

/** Le service worker prêt, ou `null` s'il ne l'est pas à temps. @param {PushEnv} env @param {number} timeoutMs */
async function readyWorker(env, timeoutMs) {
  /** @type {any} */
  let timer;
  const late = new Promise((resolve) => {
    timer = setTimeout(() => resolve(null), timeoutMs);
  });
  try {
    return /** @type {ServiceWorkerRegistration | null} */ (await Promise.race([env.serviceWorker?.ready, late]));
  } catch {
    return null;
  } finally {
    clearTimeout(timer);
  }
}

/** @param {Request} request @param {PushSubscription} subscription */
async function saveSubscription(request, subscription) {
  const reply = await request('PUT', '/push/subscription', subscription.toJSON());
  if (reply.kind === 'ok') return 'enabled';
  if (reply.kind === 'http' && reply.status === 400) return 'refused';
  return failureOf(reply, 'save_failed');
}

/**
 * Le geste de l'utilisateur : la permission se demande avant tout `await`, sinon iOS ne la montre pas.
 * Rend un code de résultat (voir `enableMessage`).
 * @param {{request: Request, env: PushEnv, readyTimeoutMs?: number}} deps
 */
export async function enableNotifications({ request, env, readyTimeoutMs = READY_TIMEOUT_MS }) {
  if (!isSupported(env) || !env.Notification) return 'unsupported';
  const permission = await env.Notification.requestPermission();
  if (permission !== 'granted') return permission === 'denied' ? 'denied' : 'dismissed';
  const mac = await fetchKey(request);
  if (!('key' in mac)) return mac.outcome;
  const registration = await readyWorker(env, readyTimeoutMs);
  if (!registration) return 'worker_unavailable';
  try {
    let subscription = await registration.pushManager.getSubscription();
    if (subscription && keysDiffer(subscription.options?.applicationServerKey, mac.key)) {
      await subscription.unsubscribe();
      subscription = null;
    }
    subscription ??= await registration.pushManager.subscribe({
      userVisibleOnly: true,
      applicationServerKey: /** @type {Uint8Array<ArrayBuffer>} */ (keyBytes(mac.key)),
    });
    return await saveSubscription(request, subscription);
  } catch {
    return 'subscribe_failed';
  }
}

/**
 * À chaque ouverture : l'abonnement du navigateur est-il celui que le Mac attend ? Clé différente : se désabonner puis
 * se réabonner ; abonnement connu du navigateur mais plus du Mac (retiré après un envoi mort) : le lui redonner.
 * Rend `none` (rien à faire : pas de permission ou pas d'abonnement), `unchanged`, `resubscribed`, `restored`, `failed`.
 * @param {{request: Request, env: PushEnv, serverSubscribed?: boolean, readyTimeoutMs?: number}} deps
 */
export async function ensureSubscriptionMatchesKey({ request, env, serverSubscribed = true, readyTimeoutMs = READY_TIMEOUT_MS }) {
  if (!isSupported(env) || env.Notification?.permission !== 'granted') return 'none';
  const registration = await readyWorker(env, readyTimeoutMs);
  if (!registration) return 'failed';
  try {
    let subscription = await registration.pushManager.getSubscription();
    if (!subscription) return 'none';
    const mac = await fetchKey(request);
    if (!('key' in mac)) return 'failed';
    // Sans clé lisible sur l'abonnement, la comparaison est impossible : on garde l'abonnement (un envoi mort le dira).
    const current = subscription.options?.applicationServerKey;
    const rotated = current !== null && current !== undefined && keysDiffer(current, mac.key);
    if (rotated) {
      await subscription.unsubscribe();
      subscription = await registration.pushManager.subscribe({
        userVisibleOnly: true,
        applicationServerKey: /** @type {Uint8Array<ArrayBuffer>} */ (keyBytes(mac.key)),
      });
    } else if (serverSubscribed) {
      return 'unchanged';
    }
    return (await saveSubscription(request, subscription)) === 'enabled' ? (rotated ? 'resubscribed' : 'restored') : 'failed';
  } catch {
    return 'failed';
  }
}

/** Retire l'abonnement du navigateur, sans le dire au Mac : celui-ci l'a déjà retiré. @param {PushEnv} env */
async function dropLocalSubscription(env) {
  try {
    const registration = await readyWorker(env, READY_TIMEOUT_MS);
    await (await registration?.pushManager.getSubscription())?.unsubscribe();
  } catch {
    // Rien à retirer : l'abonnement n'existait déjà plus.
  }
}

/**
 * `POST push/test` : rend `sent` ou la raison de l'échec.
 * @param {{request: Request}} deps
 */
export async function sendTestNotification({ request }) {
  const reply = await request('POST', '/push/test', undefined, false, TEST_TIMEOUT_MS);
  if (reply.kind === 'ok') return 'sent';
  if (reply.kind === 'http') {
    if (reply.status === 409) return 'not_subscribed';
    if (reply.status === 429) return 'in_progress';
    if (reply.status === 502) return reply.error === 'push_gone' ? 'gone' : 'push_failed';
  }
  return failureOf(reply, 'failed');
}

// --- Pastille ---

/**
 * Pose la pastille de l'icône au badge de la flotte. Un refus (permission absente) est sans suite : la prochaine
 * flotte reposera la pastille.
 * @param {number} badge
 * @param {{setAppBadge?: (count: number) => Promise<void>}} [nav]
 */
export async function syncBadge(badge, nav = navigator) {
  if (!Number.isInteger(badge) || badge < 0 || typeof nav.setAppBadge !== 'function') return false;
  try {
    await nav.setAppBadge(badge);
    return true;
  } catch {
    return false;
  }
}

// --- Les phrases ---

const NETWORK_TEXT = {
  offline: 'Mac injoignable : Tailscale est-il actif ?',
  timeout: 'Le Mac ne répond pas : en veille, ou IdemA fermé ?',
  push_unavailable: "Les notifications ne sont pas disponibles sur ce Mac.",
};

const ENABLE_TEXT = {
  ...NETWORK_TEXT,
  enabled: 'Notifications activées.',
  unsupported: "Les notifications ne sont pas disponibles ici : ouvrez IdemA depuis l'écran d'accueil.",
  denied: "Notifications refusées. Autorisez IdemA dans Réglages, Notifications, puis revenez ici.",
  dismissed: "La permission n'a pas été accordée. Touchez de nouveau le bouton pour réessayer.",
  key_unavailable: "Le Mac n'a pas donné sa clé d'envoi. Réessayez dans un instant.",
  worker_unavailable: "Le service de notifications n'est pas prêt. Fermez puis rouvrez IdemA.",
  subscribe_failed: "L'abonnement a échoué. Réessayez.",
  refused: "Le Mac a refusé l'abonnement de cet appareil.",
  save_failed: "Le Mac n'a pas pu enregistrer l'abonnement. Réessayez.",
};

const TEST_TEXT = {
  ...NETWORK_TEXT,
  sent: "Notification envoyée : elle arrive dans quelques secondes.",
  not_subscribed: "Cet appareil n'est pas abonné. Activez d'abord les notifications.",
  in_progress: 'Un test est déjà en cours. Patientez un instant.',
  gone: "L'abonnement n'était plus valable : il a été retiré. Réactivez les notifications.",
  push_failed: "Le service de notifications n'a pas accepté l'envoi. Réessayez dans un instant.",
  failed: "Le test n'a pas pu partir. Réessayez.",
};

/** @param {Record<string, string>} table @param {string} outcome @param {string} fallback */
function phrase(table, outcome, fallback) {
  return Object.hasOwn(table, outcome) ? table[outcome] : fallback;
}

/** @param {string} outcome */
export function enableMessage(outcome) {
  return phrase(ENABLE_TEXT, outcome, "L'activation a échoué. Réessayez.");
}

/** @param {string} outcome */
export function testMessage(outcome) {
  return phrase(TEST_TEXT, outcome, TEST_TEXT.failed);
}

// --- L'état que les écrans lisent ---

/**
 * @typedef {object} PushState
 * @property {boolean} supported le navigateur sait faire du push (app installée, iOS 16.4 et plus)
 * @property {NotificationPermission} permission
 * @property {boolean} subscribed cet appareil est abonné chez le Mac
 * @property {boolean} busy un geste est en cours
 * @property {{text: string, ok: boolean} | null} message le dernier résultat, dit en français
 */

/** @param {{request: Request, env?: PushEnv}} deps */
export function createPushController({ request, env = browserEnv() }) {
  /** @type {PushState} */
  const state = { supported: isSupported(env), permission: env.Notification?.permission ?? 'default', subscribed: false, busy: false, message: null };
  /** @type {Set<(state: PushState) => void>} */
  const subscribers = new Set();
  /** @type {Promise<void> | null} */
  let refreshing = null;

  function notify() {
    state.supported = isSupported(env);
    state.permission = env.Notification?.permission ?? 'default';
    subscribers.forEach((fn) => fn(state));
  }

  /** Un geste à la fois : `run` rend le code de résultat et ce que l'état en retient. @param {() => Promise<string>} run @param {(outcome: string) => string} say */
  async function gesture(run, say) {
    if (state.busy) return;
    state.busy = true;
    state.message = null;
    notify();
    const outcome = await run();
    state.busy = false;
    state.message = { text: say(outcome), ok: outcome === 'enabled' || outcome === 'sent' };
    return outcome;
  }

  return {
    state,
    /** @param {(state: PushState) => void} fn @returns {() => void} */
    subscribe(fn) {
      subscribers.add(fn);
      return () => subscribers.delete(fn);
    },
    /** À chaque ouverture : qui est abonné, et la clé est-elle la bonne ? Hors ligne, l'état connu reste. */
    refresh() {
      async function run() {
        notify();
        if (!state.supported) return;
        const me = await request('GET', '/me');
        if (me.kind !== 'ok') return;
        const serverSubscribed = me.data?.subscribed === true;
        const outcome = await ensureSubscriptionMatchesKey({ request, env, serverSubscribed });
        state.subscribed = outcome === 'unchanged' || outcome === 'restored' || outcome === 'resubscribed' || (outcome === 'failed' && serverSubscribed);
        notify();
      }
      // Les appels qui se chevauchent (démarrage, reprise, ouverture des réglages) partagent la même lecture.
      refreshing ??= run().finally(() => {
        refreshing = null;
      });
      return refreshing;
    },
    /** Le geste « Activer les notifications » : à appeler directement depuis le toucher. */
    async enable() {
      const outcome = await gesture(() => enableNotifications({ request, env }), enableMessage);
      if (outcome === 'enabled') state.subscribed = true;
      notify();
    },
    async test() {
      const outcome = await gesture(() => sendTestNotification({ request }), testMessage);
      // Abonnement mort : le Mac l'a retiré ; le garder dans le navigateur le ferait redonner à chaque ouverture.
      if (outcome === 'gone') await dropLocalSubscription(env);
      if (outcome === 'gone' || outcome === 'not_subscribed') state.subscribed = false;
      notify();
    },
  };
}
