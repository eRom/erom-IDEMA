// @ts-check
// L'état de la page : la flotte reçue, la connexion, la dernière visite et les préférences (localStorage).

const FLEET_KEY = 'idema.fleet';
const VISIT_KEY = 'idema.lastVisit';
const PREF_PREFIX = 'idema.pref.';

/**
 * L'instantané de la flotte (`GET /idema-api/fleet`). Les écrans de la flotte précisent les champs qu'ils lisent.
 * @typedef {{mac: string, generated_at: number, badge: number, sessions: any[], projects: any[], ideation_label?: string | null}} Fleet
 */

/**
 * @typedef {object} State
 * @property {Fleet | null} fleet la dernière flotte reçue (celle du cache au lancement)
 * @property {number | null} fleetAt instant de sa réception, côté téléphone
 * @property {'ok' | 'offline' | 'timeout'} connection
 * @property {number | null} lastVisit instant du dernier masquage de la page
 */

/** @param {Storage} storage @param {string} key */
function read(storage, key) {
  try {
    return storage.getItem(key);
  } catch {
    return null;
  }
}

/** @param {Storage} storage @param {string} key @param {string} value */
function write(storage, key, value) {
  try {
    storage.setItem(key, value);
  } catch {
    // Stockage refusé (mode privé, quota) : la page garde son état en mémoire et s'en passe.
  }
}

/** La flotte du cache, si elle a la forme d'une flotte. @param {Storage} storage */
function readCachedFleet(storage) {
  try {
    const cached = JSON.parse(read(storage, FLEET_KEY) ?? 'null');
    if (cached && Array.isArray(cached.fleet?.sessions) && Number.isFinite(cached.at)) {
      return { fleet: /** @type {Fleet} */ (cached.fleet), at: /** @type {number} */ (cached.at) };
    }
  } catch {
    // Cache illisible : comme s'il n'y en avait pas.
  }
  return null;
}

/** @param {Storage} storage */
export function createStore(storage) {
  const cached = readCachedFleet(storage);
  const savedVisit = Number(read(storage, VISIT_KEY));
  /** @type {State} */
  const state = {
    fleet: cached?.fleet ?? null,
    fleetAt: cached?.at ?? null,
    connection: 'ok',
    lastVisit: read(storage, VISIT_KEY) !== null && Number.isFinite(savedVisit) ? savedVisit : null,
  };
  /** @type {Set<(state: State) => void>} */
  const subscribers = new Set();
  const notify = () => subscribers.forEach((fn) => fn(state));

  return {
    state,
    /** @param {(state: State) => void} fn @returns {() => void} le désabonnement */
    subscribe(fn) {
      subscribers.add(fn);
      return () => subscribers.delete(fn);
    },
    /** @param {Fleet} fleet @param {number} [nowMs] */
    setFleet(fleet, nowMs = Date.now()) {
      state.fleet = fleet;
      state.fleetAt = nowMs;
      write(storage, FLEET_KEY, JSON.stringify({ fleet, at: nowMs }));
      notify();
    },
    /** L'appareil n'est plus appairé : la flotte en cache n'est plus à lui. */
    clearFleet() {
      state.fleet = null;
      state.fleetAt = null;
      try {
        storage.removeItem(FLEET_KEY);
      } catch {
        // Stockage refusé : rien à effacer.
      }
      notify();
    },
    /** @param {State['connection']} connection */
    setConnection(connection) {
      if (state.connection === connection) return;
      state.connection = connection;
      notify();
    },
    /** Marque le masquage de la page : la prochaine ouverture sait depuis quand on est parti. @param {number} [nowMs] */
    touchVisit(nowMs = Date.now()) {
      state.lastVisit = nowMs;
      write(storage, VISIT_KEY, String(nowMs));
    },
    /** @template T @param {string} key @param {T} fallback @returns {T} */
    getPref(key, fallback) {
      try {
        const raw = read(storage, PREF_PREFIX + key);
        return raw === null ? fallback : JSON.parse(raw);
      } catch {
        return fallback;
      }
    },
    /** @param {string} key @param {unknown} value */
    setPref(key, value) {
      write(storage, PREF_PREFIX + key, JSON.stringify(value));
    },
  };
}

/** `localStorage`, ou un stockage vide si le navigateur le refuse (l'accès lève, la page resterait blanche). @returns {Storage} */
export function safeLocalStorage() {
  try {
    return globalThis.localStorage;
  } catch {
    return /** @type {Storage} */ (/** @type {unknown} */ ({
      getItem: () => null,
      setItem() {},
      removeItem() {},
    }));
  }
}

/** La page n'a qu'un état. */
export const store = createStore(safeLocalStorage());
