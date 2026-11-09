// @ts-check
// Le moteur de sauvegarde de l'éditeur de notes : écriture différée, brouillon hors ligne, cache, base et conflit.
// Sans DOM. Règle d'or : le texte tapé n'est jamais remplacé par une réponse du Mac, sauf `takeMac`.

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
/** `{text, base}` : le texte tapé pas encore écrit, et le modified_at du disque dont il est parti. */
export const DRAFT_KEY = STORAGE_PREFIX + 'draft';
/** `{text, modified_at}` : la dernière note connue du Mac, pour ouvrir sans lui. */
export const CACHE_KEY = STORAGE_PREFIX + 'cache';

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
  /** Le brouillon : non nul tant que `text` n'est pas sur le disque. Miroir mémoire du stockage (qui peut refuser). */
  let draft = /** @type {{text: string, base: number | null} | null} */ (null);
  /** `edit()` a été appelé depuis le montage : plus aucune réponse du Mac ne remplace `text` (sauf `takeMac`). */
  let touched = false;
  /** Numéro de la dernière frappe, pour qu'une action asynchrone sache si on a tapé pendant qu'elle attendait. */
  let rev = 0;
  let alive = true;
  let timer = /** @type {any} */ (null);

  /** @param {string} key @param {unknown} value */
  function save(key, value) {
    try {
      storage.setItem(key, JSON.stringify(value));
    } catch {
      // Stockage plein ou refusé : le brouillon reste en mémoire et l'envoi continue.
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
    return stored && typeof stored.text === 'string' ? { text: /** @type {string} */ (stored.text), base: asBase(stored.base) } : null;
  }
  function readCache() {
    const stored = readJson(CACHE_KEY);
    return stored && typeof stored.text === 'string' ? { text: /** @type {string} */ (stored.text), modified_at: asBase(stored.modified_at) } : null;
  }
  /** @param {string} t @param {number | null} modifiedAt */
  const writeCache = (t, modifiedAt) => save(CACHE_KEY, { text: t, modified_at: modifiedAt });
  function setDraft() {
    draft = { text, base: savedAt };
    save(DRAFT_KEY, draft);
  }
  function dropDraft() {
    draft = null;
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
  function clearTimer() {
    if (timer !== null) timers.clearTimeout(timer);
    timer = null;
  }

  // Un seul travail réseau à la fois (chargement, PUT, reprise) : sinon deux PUT se doublent et se prennent pour un conflit.
  let tail = /** @type {Promise<void>} */ (Promise.resolve());
  /** Travaux en cours ou en attente : un keepalive de page qui se cache ne double pas un envoi en vol. */
  let jobs = 0;
  /** @param {() => Promise<void>} job */
  function serial(job) {
    jobs++;
    tail = tail.then(job).catch(() => {}).then(() => void jobs--);
    return tail;
  }

  /** Le Mac a écrit `sent` : la base avance, le cache suit, le brouillon part s'il n'y a rien de plus récent. @param {string} sent @param {number | null} modifiedAt */
  function onSaved(sent, modifiedAt) {
    savedAt = modifiedAt;
    writeCache(sent, modifiedAt);
    if (norm(text) === norm(sent)) {
      dropDraft();
      setState('saved');
    } else {
      setDraft(); // frappé pendant l'envoi : le brouillon repart de la nouvelle base
      setState('saving');
    }
  }

  /**
   * Un 409 : la base n'est plus celle du Mac. Si le Mac a déjà notre texte (réponse perdue, délai dépassé), on prend
   * sa base sans rien dire ; sinon, conflit. Hors ligne, on ne sait pas : le brouillon attend.
   * @param {string} sent
   */
  async function reconcile(sent) {
    const res = await request('GET', '/scratchpad');
    if (res.kind !== 'ok') return setState('offline');
    const macText = norm(String(res.data.text ?? ''));
    const macAt = asBase(res.data.modified_at);
    if (macText === norm(sent)) return onSaved(sent, macAt);
    if (macText === norm(text)) return onSaved(text, macAt);
    writeCache(String(res.data.text ?? ''), macAt);
    clearTimer();
    setState('conflict');
  }

  /** @param {{force?: boolean, keepalive?: boolean}} [options] */
  async function doSend({ force = false, keepalive = false } = {}) {
    const sent = text;
    const body = force ? { text: sent, base: savedAt, force: true } : { text: sent, base: savedAt };
    if (keepalive && new TextEncoder().encode(JSON.stringify(body)).length > KEEPALIVE_MAX_BYTES) return; // le brouillon suffit
    if (state !== 'offline') setState('saving');
    const res = await request('PUT', '/scratchpad', body, keepalive, PUT_TIMEOUT_MS);
    if (res.kind === 'ok') return onSaved(sent, asBase(res.data?.modified_at));
    if (res.kind !== 'http') return setState('offline'); // hors ligne, délai (le Mac a peut-être écrit : le 409 suivant le dira), non appairé
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
    const pending = draft ?? readDraft();
    if (pending) {
      draft = pending;
      text = pending.text;
      savedAt = pending.base;
      if (state === 'loading' || state === 'locked') setState('saving');
    }
    const res = await request('GET', '/scratchpad');
    if (res.kind === 'ok') {
      const { mac, macAt, editable } = adopt(res);
      if (draft) {
        if (norm(mac) === norm(draft.text)) {
          savedAt = macAt;
          dropDraft();
          return setState(editable ? 'saved' : 'readonly');
        }
        if (!editable) return setState('readonly');
        return doSend(); // la base du brouillon décide : un Mac qui a bougé répondra 409
      }
      if (!touched) {
        text = mac;
        savedAt = macAt;
      } else if (norm(text) === norm(mac)) {
        savedAt = macAt;
      } // sinon la base périmée reste : la prochaine écriture sera refusée (409) plutôt que d'écraser le Mac
      return setState(editable ? 'saved' : 'readonly');
    }
    if (res.kind === 'http' && res.status === 413) return setState('readonly', TOO_BIG_ON_PHONE);
    if (draft) return setState('offline');
    if (state === 'loading' || state === 'locked') {
      const cached = readCache();
      if (!cached) return setState('locked', LOCKED_REASON);
      text = cached.text;
      savedAt = cached.modified_at;
      return setState('offline');
    }
    // Un éditeur qui marche déjà ne se verrouille pas pour une reprise ratée.
  }

  /** Les états où une frappe part toute seule. @param {boolean} afterEdit */
  const sendable = (afterEdit) => state === 'saved' || state === 'saving' || state === 'offline' || (afterEdit && state === 'refused');

  function onTimer() {
    timer = null;
    void serial(async () => {
      if (draft && sendable(true)) await doSend();
    });
  }

  /** @param {boolean} [keepalive] */
  function flushNow(keepalive = false) {
    clearTimer();
    if (!draft || !sendable(false)) return Promise.resolve();
    // Un keepalive part de la page qui se cache : s'il y a déjà un travail en vol, le brouillon suffit.
    if (keepalive) return jobs > 0 ? Promise.resolve() : serial(() => doSend({ keepalive: true }));
    return serial(async () => {
      if (draft && sendable(false)) await doSend();
    });
  }

  function load() {
    return serial(doLoad);
  }

  const unsubscribe = store
    ? (() => {
        let previous = store.state.connection;
        return store.subscribe((next) => {
          const was = previous;
          previous = next.connection;
          if (!alive || next.connection !== 'ok' || was === 'ok') return;
          if (state === 'locked') void load();
          else if (state === 'offline') void (draft ? flushNow() : load());
        });
      })()
    : () => {};

  return {
    load,
    /** @param {string} next */
    edit(next) {
      if (state === 'loading' || state === 'locked' || state === 'readonly') return;
      touched = true;
      rev++;
      text = next;
      setDraft();
      if (state === 'conflict') return; // le brouillon se met à jour, rien ne part avant le choix de Romain
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
        const res = await request('GET', '/scratchpad');
        if (res.kind !== 'ok' || seen !== rev) return; // rien tapé ne doit se perdre pendant l'attente
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
      unsubscribe();
    },
  };
}
