// @ts-check
// Le moteur de sauvegarde de l'éditeur de notes (sans DOM) : le texte tapé n'est jamais remplacé par une réponse du Mac, sauf `takeMac`.

import { realTimers } from './time.js';

/** @typedef {'loading' | 'locked' | 'saved' | 'saving' | 'offline' | 'refused' | 'conflict' | 'readonly'} SyncState */
/**
 * @typedef {object} SyncSnapshot
 * @property {SyncState} state
 * @property {string} text le texte à afficher (le brouillon s'il y en a un)
 * @property {number | null} savedAt le modified_at connu du disque
 * @property {string} [reason] raison française d'un refus ou d'un verrou
 */

export const SAVE_DELAY_MS = 800;
export const PUT_TIMEOUT_MS = 15_000;
export const KEEPALIVE_MAX_BYTES = 64 * 1024;
const STORAGE_PREFIX = 'idema.notes.';
/** `{text, base, sent}` : le texte pas encore écrit, le modified_at dont il part, les envois à vérifier. */
export const DRAFT_KEY = STORAGE_PREFIX + 'draft';
/** `{text, modified_at}` : la dernière note connue du Mac. */
export const CACHE_KEY = STORAGE_PREFIX + 'cache';

/** Attentes avant de réessayer un envoi tombé hors ligne (la connexion, elle, n'a pas bougé) : 5 s, 15 s, puis 60 s. */
const RETRY_BACKOFF_MS = [5_000, 15_000, 60_000];
const UNEXPECTED_REASON = "erreur inattendue dans l'application";
const LOCKED_REASON = "Le Mac ne répond pas : la note s'ouvrira à son retour.";
const TOO_BIG_ON_PHONE = 'Note trop grosse pour le téléphone (5 Mo au plus) : modifiez-la sur le Mac.';
const REFUSALS = /** @type {Record<string, string>} */ ({
  control_char: 'caractère NUL dans la note',
  scratchpad_unwritable: 'le Mac ne peut pas écrire la note',
});

/** Le Mac retire les `\r` : deux textes qui ne diffèrent que par eux sont le même. @param {string} s */
const norm = (s) => s.replace(/\r/g, '');

/** @param {unknown} value @returns {number | null} */
const asBase = (value) => (typeof value === 'number' && Number.isFinite(value) ? value : null);

/**
 * @param {{
 *   request: import('./push.js').Request,
 *   timers?: import('./time.js').Timers,
 *   storage: Storage,
 *   onChange?: () => void,
 *   store?: {state: {connection: string}, subscribe(fn: (state: any) => void): () => void},
 * }} deps `store` : au retour de la connexion, une note verrouillée se charge et un brouillon hors ligne part.
 */
export function createNotesSync({ request, timers = realTimers, storage, onChange, store }) {
  /** @type {SyncState} */
  let state = 'loading';
  let reason = /** @type {string | undefined} */ (undefined);
  let text = '';
  let savedAt = /** @type {number | null} */ (null);
  /** Non nul tant que `text` n'est pas sur le disque (miroir du stockage, qui peut refuser). */
  let draft = /** @type {{text: string, base: number | null} | null} */ (null);
  /** Numéro de la dernière frappe. */
  let rev = 0;
  let alive = true;
  let timer = /** @type {any} */ (null);
  let retryTimer = /** @type {any} */ (null);
  let retries = 0;
  /** Textes envoyés sans réponse sûre (hors ligne, délai) : le Mac les a peut-être écrits. */
  let unconfirmed = /** @type {string[]} */ ([]);

  /** Plus d'écriture après `dispose()` : le montage suivant est seul maître du stockage. @param {string} key @param {unknown} value */
  function save(key, value) {
    if (!alive) return false;
    try {
      storage.setItem(key, JSON.stringify(value));
      return true;
    } catch {
      return false; // stockage plein ou refusé : le brouillon reste en mémoire et l'envoi continue
    }
  }
  /** @param {string} key */
  function readJson(key) {
    try {
      return JSON.parse(storage.getItem(key) ?? 'null');
    } catch {
      return null;
    }
  }
  function readDraft() {
    const stored = readJson(DRAFT_KEY);
    if (!stored || typeof stored.text !== 'string') return null;
    const sent = Array.isArray(stored.sent) ? stored.sent.filter((/** @type {unknown} */ t) => typeof t === 'string') : [];
    return { text: /** @type {string} */ (stored.text), base: asBase(stored.base), sent: /** @type {string[]} */ (sent) };
  }
  function readCache() {
    const stored = readJson(CACHE_KEY);
    return stored && typeof stored.text === 'string' ? { text: /** @type {string} */ (stored.text), modified_at: asBase(stored.modified_at) } : null;
  }
  /** @param {string} t @param {number | null} modifiedAt */
  const writeCache = (t, modifiedAt) => save(CACHE_KEY, { text: t, modified_at: modifiedAt });
  function persistDraft() {
    if (draft && !save(DRAFT_KEY, { ...draft, sent: unconfirmed })) save(DRAFT_KEY, draft); // sans les envois à vérifier si la place manque
  }
  function setDraft() {
    draft = { text, base: savedAt };
    persistDraft();
  }
  /** @param {string} t */
  function remember(t) {
    unconfirmed = [...unconfirmed.filter((u) => norm(u) !== norm(t)), t].slice(-3);
  }
  /** @param {string} macText @param {...string} more */
  const isOwn = (macText, ...more) => [...unconfirmed, ...more].some((t) => norm(t) === macText);
  function dropDraft() {
    draft = null;
    unconfirmed = [];
    if (!alive) return;
    try {
      storage.removeItem(DRAFT_KEY);
    } catch {
      // Rien à effacer : le stockage refuse.
    }
  }

  /** @param {SyncState} next @param {string} [why] */
  function setState(next, why) {
    state = next;
    reason = why;
    if (alive) onChange?.();
  }
  function arm() {
    if (alive && timer === null) timer = timers.setTimeout(onTimer, SAVE_DELAY_MS);
  }
  function clearTimer() {
    if (timer !== null) timers.clearTimeout(timer);
    timer = null;
  }
  function clearRetry() {
    if (retryTimer !== null) timers.clearTimeout(retryTimer);
    retryTimer = null;
    retries = 0;
  }
  /** Un envoi tombé hors ligne alors que la connexion reste « ok » ne se relance pas tout seul : on le réessaie, de plus en plus lentement. */
  function scheduleRetry() {
    if (!alive || retryTimer !== null) return;
    retryTimer = timers.setTimeout(() => {
      retryTimer = null;
      if (state === 'offline') void flushNow();
    }, RETRY_BACKOFF_MS[Math.min(retries++, RETRY_BACKOFF_MS.length - 1)]);
  }

  // Un seul travail réseau à la fois : deux PUT en vol se prendraient pour un conflit.
  let tail = /** @type {Promise<void>} */ (Promise.resolve());
  /** Travaux en cours ou en attente. */
  let jobs = 0;
  /** @param {() => Promise<void>} job @param {boolean} [now] démarre dans l'appel même si rien n'est en cours (keepalive : `dispose()` suit) */
  function serial(job, now = false) {
    const idle = jobs++ === 0;
    // Une exception inattendue ne tue pas la file : elle devient un refus visible.
    const guarded = async () => {
      try {
        await job();
      } catch {
        if (alive) setState('refused', UNEXPECTED_REASON);
      }
    };
    const run = now && idle ? guarded() : tail.then(() => (alive ? guarded() : undefined));
    tail = run.then(() => void jobs--);
    return tail;
  }

  /** Un moteur démonté n'émet plus rien. @type {import('./push.js').Request} */
  const call = (method, path, body, keepalive, timeoutMs) => (alive ? request(method, path, body, keepalive, timeoutMs) : Promise.resolve({ kind: 'offline' }));

  /** Lit la note du Mac (délai par défaut : hors ligne, l'état doit tomber vite) ; une réponse 200 sans corps est une lecture ratée. */
  async function readMac() {
    const res = await call('GET', '/scratchpad');
    return res.kind === 'ok' && !res.data ? { kind: /** @type {const} */ ('offline') } : res;
  }

  /** Le Mac a écrit `sent` : la base avance ; le brouillon reste s'il y a plus récent. @param {string} sent @param {number | null} modifiedAt */
  function onSaved(sent, modifiedAt) {
    savedAt = modifiedAt;
    writeCache(sent, modifiedAt);
    unconfirmed = [];
    clearRetry();
    if (norm(text) === norm(sent)) {
      dropDraft();
      setState('saved');
    } else {
      setDraft();
      arm();
      setState('saving');
    }
  }

  /** Le Mac porte un de nos textes : sa base, sans bandeau. Vrai si c'est le courant. @param {string} macText @param {number | null} macAt */
  function adoptOwn(macText, macAt) {
    savedAt = macAt;
    writeCache(macText, macAt);
    if (norm(text) === norm(macText)) {
      dropDraft();
      setState('saved');
      return true;
    }
    unconfirmed = [];
    setDraft();
    setState('saving');
    return false;
  }

  /**
   * Un 409 : si le Mac porte déjà un de nos textes (réponse perdue), on reprend sa base et on renvoie le courant ; sinon conflit.
   * @param {string} sent
   * @returns {Promise<void>}
   */
  async function reconcile(sent) {
    const res = await readMac();
    if (res.kind !== 'ok') return setState('offline');
    const macRaw = String(res.data.text ?? '');
    const macAt = asBase(res.data.modified_at);
    if (isOwn(norm(macRaw), sent, text)) return adoptOwn(macRaw, macAt) ? undefined : doSend();
    writeCache(macRaw, macAt);
    clearTimer();
    setState('conflict');
  }

  /** @param {{force?: boolean, keepalive?: boolean}} [options] @returns {Promise<void>} */
  async function doSend({ force = false, keepalive = false } = {}) {
    const sent = text;
    const body = force ? { text: sent, base: savedAt, force: true } : { text: sent, base: savedAt };
    if (keepalive && new TextEncoder().encode(JSON.stringify(body)).length > KEEPALIVE_MAX_BYTES) return arm(); // trop gros : un envoi normal suit
    if (state !== 'offline') setState('saving');
    remember(sent); // retenu avant l'envoi : si la page meurt en vol, le montage suivant le reconnaîtra
    persistDraft();
    const res = await call('PUT', '/scratchpad', body, keepalive, PUT_TIMEOUT_MS);
    if (res.kind === 'ok') return onSaved(sent, asBase(res.data?.modified_at));
    if (res.kind !== 'http') {
      scheduleRetry();
      return setState('offline'); // hors ligne ou délai : le Mac a peut-être écrit, le 409 suivant reconnaîtra ce texte
    }
    if (res.status === 409) return reconcile(sent);
    return setState('refused', res.status === 413 ? 'note trop grosse (5 Mo au plus)' : (REFUSALS[res.error] ?? "le Mac a refusé l'enregistrement"));
  }

  /** @param {{data: any}} res */
  function adopt(res) {
    const mac = String(res.data.text ?? '');
    const macAt = asBase(res.data.modified_at);
    writeCache(mac, macAt);
    return { mac, macAt, editable: res.data.editable !== false };
  }

  async function doLoad() {
    const stored = draft ? null : readDraft();
    const pending = draft ?? stored;
    if (pending) {
      if (stored) unconfirmed = stored.sent;
      draft = { text: pending.text, base: pending.base };
      remember(pending.text);
      text = pending.text;
      savedAt = pending.base;
      if (state === 'loading' || state === 'locked') setState('saving');
    }
    const seen = rev;
    const res = await readMac();
    if (res.kind === 'ok') {
      const { mac, macAt, editable } = adopt(res);
      if (draft) {
        if (isOwn(norm(mac), draft.text) && adoptOwn(mac, macAt)) return editable ? undefined : setState('readonly');
        if (!editable) return setState('readonly');
        return doSend();
      }
      if (seen === rev) {
        text = mac; // rien tapé pendant la lecture (un brouillon naît à la frappe) : la note du Mac fait foi, même si l'écran en montrait une plus ancienne
        savedAt = macAt;
      }
      return setState(editable ? 'saved' : 'readonly');
    }
    if (res.kind === 'http' && res.status === 413) return setState('readonly', TOO_BIG_ON_PHONE);
    if (draft) {
      scheduleRetry();
      return setState('offline');
    }
    if (state === 'loading' || state === 'locked') {
      const cached = readCache();
      if (!cached) return setState('locked', LOCKED_REASON);
      text = cached.text;
      savedAt = cached.modified_at;
      return setState('offline');
    }
  }

  /** Les états où une frappe part toute seule. @param {boolean} afterEdit */
  const sendable = (afterEdit) => state === 'saved' || state === 'saving' || state === 'offline' || (afterEdit && state === 'refused');

  /** @param {boolean} afterEdit */
  const sendIfDirty = (afterEdit) =>
    serial(async () => {
      if (draft && sendable(afterEdit)) await doSend();
    });

  function onTimer() {
    timer = null;
    void sendIfDirty(true);
  }

  /** @param {boolean} [keepalive] */
  function flushNow(keepalive = false) {
    if (!draft || !sendable(false)) return Promise.resolve();
    if (keepalive && jobs > 0) return Promise.resolve(); // le brouillon suffit, le minuteur reste
    clearTimer();
    return keepalive ? serial(() => doSend({ keepalive: true }), true) : sendIfDirty(false);
  }

  function load() {
    return serial(doLoad);
  }

  /** Un seul envoi de retour en attente à la fois : il part dès que le travail en cours est fini. */
  let reconnectQueued = false;
  /** La connexion revient : le délai de reprise repart de zéro et ce qui attendait le Mac part tout de suite. */
  function onReconnect() {
    clearRetry();
    if (reconnectQueued) return;
    reconnectQueued = true;
    void serial(async () => {
      reconnectQueued = false;
      if (state === 'locked' || (state === 'offline' && !draft)) return doLoad();
      if (draft && sendable(false)) {
        clearTimer();
        return doSend();
      }
    });
  }

  const unsubscribe = store
    ? (() => {
        let previous = store.state.connection;
        return store.subscribe((next) => {
          const was = previous;
          previous = next.connection;
          if (alive && next.connection === 'ok' && was !== 'ok') onReconnect();
        });
      })()
    : () => {};

  return {
    load,
    /** @param {string} next */
    edit(next) {
      if (state === 'loading' || state === 'locked' || state === 'readonly') return;
      rev++;
      text = next;
      setDraft();
      if (state === 'conflict') return; // rien ne part avant le choix de Romain
      if (state === 'saved') setState('saving');
      clearTimer();
      timer = timers.setTimeout(onTimer, SAVE_DELAY_MS);
    },
    flushNow,
    /** Écraser la note du Mac par le brouillon (après un conflit). */
    overwrite() {
      return serial(async () => {
        if (state === 'conflict') await doSend({ force: true });
      });
    },
    /** Reprendre la note du Mac et abandonner le brouillon ; hors ligne, le brouillon reste. */
    takeMac() {
      return serial(async () => {
        clearTimer();
        const seen = rev;
        const res = await readMac();
        if (res.kind !== 'ok' || seen !== rev) return; // rien de tapé ne se perd
        const { mac, macAt, editable } = adopt(res);
        text = mac;
        savedAt = macAt;
        dropDraft();
        setState(editable ? 'saved' : 'readonly');
      });
    },
    /** @returns {SyncSnapshot} */
    snapshot() {
      return reason === undefined ? { state, text, savedAt } : { state, text, savedAt, reason };
    },
    dispose() {
      alive = false;
      clearTimer();
      clearRetry();
      unsubscribe();
    },
  };
}

/** Les états où un moteur sans écran n'a plus rien à tenter seul (le brouillon, s'il reste, attend l'onglet Notes). */
const SETTLED = new Set(['saved', 'conflict', 'refused', 'readonly', 'locked']);

/**
 * Envoie le brouillon enregistré quand l'onglet Notes n'est pas monté (le moteur ne vit autrement que dans cet onglet) :
 * un moteur sans écran, jamais deux à la fois, supprimé dès qu'il a fini. À arrêter (`stop`) avant que Notes ne monte.
 * @param {{request: import('./push.js').Request, timers?: import('./time.js').Timers, storage: Storage, store?: Parameters<typeof createNotesSync>[0]['store']}} deps
 */
export function createDraftFlusher({ request, timers = realTimers, storage, store }) {
  /** @type {ReturnType<typeof createNotesSync> | null} */
  let engine = null;
  function hasDraft() {
    try {
      return typeof JSON.parse(storage.getItem(DRAFT_KEY) ?? 'null')?.text === 'string';
    } catch {
      return false;
    }
  }
  function stop() {
    const running = engine;
    engine = null;
    running?.dispose();
  }
  return {
    /** Lance le moteur s'il y a un brouillon ; s'il tourne déjà, `reload` le fait relire (retour au premier plan). @param {boolean} [reload] */
    kick(reload = false) {
      if (engine) {
        if (reload) void engine.load();
        return;
      }
      if (!hasDraft()) return;
      const mine = createNotesSync({
        request,
        timers,
        storage,
        store,
        onChange: () => {
          if (engine === mine && SETTLED.has(mine.snapshot().state)) stop();
        },
      });
      engine = mine;
      void mine.load();
    },
    stop,
  };
}
