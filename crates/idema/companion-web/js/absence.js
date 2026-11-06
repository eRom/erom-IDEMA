// @ts-check
// L'onglet Absence : ce qui s'est passé depuis la dernière visite, en phrases, puis, sous « Plus tôt », ce que le tampon
// du Mac garde d'avant.

import { el } from './dom.js';
import { sessionHash } from './router.js';
import { ageFr } from './time.js';
import { targetPane } from './triage.js';

/** « 16:02 », heure locale. @param {number} ms */
function clock(ms) {
  const d = new Date(ms);
  return `${String(d.getHours()).padStart(2, '0')}:${String(d.getMinutes()).padStart(2, '0')}`;
}

/** @typedef {{at: number, session: string, from: string, to: string, finished: boolean}} TimelineEvent */
/** @typedef {{session: string, at: number, text: string}} Phrase */

/** Combien de phrases « Plus tôt » au plus. */
export const EARLIER_MAX = 30;

/**
 * Les transitions en phrases, la plus récente d'abord. Une attente se dit à sa fin, ou « attend depuis » tant qu'elle
 * dure. Les événements d'avant `pairFrom` sont ignorés ; seules les phrases dont l'instant est dans `[from, until[` sont
 * gardées (l'appariement des attentes, lui, voit tout ce qui suit `pairFrom`).
 * @param {TimelineEvent[]} events chronologiques
 * @param {(session: string) => string} labelOf
 * @param {{pairFrom: number, from: number, until: number}} window
 * @returns {Phrase[]}
 */
function tell(events, labelOf, { pairFrom, from, until }) {
  /** @type {Map<string, number>} */
  const waits = new Map();
  /** @type {Phrase[]} */
  const items = [];
  for (const { at, session, from: before, to, finished } of events) {
    if (at < pairFrom) continue;
    const say = (/** @type {string} */ text) => at >= from && at < until && items.push({ session, at, text: `${labelOf(session)} ${text}` });
    const since = waits.get(session);
    if (to === 'waiting') waits.set(session, at);
    else if (before === 'waiting') {
      waits.delete(session);
      if (since !== undefined) say(`a attendu ${ageFr(since, at)}${to === 'running' ? ', puis a repris' : ''} (${clock(at)})`);
      else if (to === 'running') say(`a repris (${clock(at)})`);
    }
    if (to === 'error') say(`est passé en erreur (${clock(at)})`);
    if (finished) say(`a fini (${clock(at)})`);
  }
  for (const [session, at] of waits) if (at >= from && at < until) items.push({ session, at, text: `${labelOf(session)} attend depuis ${clock(at)}` });
  return items.reverse().sort((a, b) => b.at - a.at); // à égalité, la dernière dite passe devant
}

/**
 * Ce qui s'est passé depuis la dernière visite. La tête signale un redémarrage d'IdemA depuis la dernière visite : le
 * tampon d'avant est perdu.
 * @param {TimelineEvent[]} events chronologiques
 * @param {number} startedAt @param {number | null} lastVisit @param {(session: string) => string} [labelOf]
 */
export function phrases(events, startedAt, lastVisit, labelOf = () => "L'agent") {
  const since = lastVisit ?? 0;
  const items = tell(events, labelOf, { pairFrom: since, from: since, until: Infinity });
  return { head: startedAt > since ? `Depuis le démarrage d'IdemA à ${clock(startedAt)}` : null, items };
}

/**
 * Ce qui s'est passé avant la dernière visite et reste dans le tampon, la plus récente d'abord, répétitions fondues
 * (`mergeRepeats`), `EARLIER_MAX` lignes au plus. Les attentes s'apparient sur tout le tampon. Sans visite précédente,
 * tout est « nouveau » : rien d'antérieur.
 * @param {TimelineEvent[]} events chronologiques
 * @param {number | null} lastVisit @param {(session: string) => string} [labelOf]
 */
export function earlierPhrases(events, lastVisit, labelOf = () => "L'agent") {
  if (!lastVisit) return [];
  return mergeRepeats(tell(events, labelOf, { pairFrom: 0, from: 0, until: lastVisit })).slice(0, EARLIER_MAX);
}

/** Deux phrases identiques d'une même session à moins de 5 min d'écart n'en font qu'une, avec le nombre de fois. */
export const REPEAT_WINDOW_MS = 5 * 60_000;
const TRAILING_CLOCK = / \((\d\d:\d\d)\)$/;

/**
 * La phrase sous l'en-tête de sa ligne : quand l'en-tête est le nom même de l'agent sujet de la phrase (« Opus 5.5 »
 * au-dessus de « Opus 5.5 a attendu… »), le sujet n'est pas répété et la phrase commence par le verbe.
 * @param {string} caption l'en-tête de la ligne @param {string} subject l'agent que la phrase nomme @param {string} text
 */
export function sentenceUnder(caption, subject, text) {
  if (!caption || caption !== subject || !text.startsWith(`${subject} `)) return text;
  const rest = text.slice(subject.length + 1);
  return rest.charAt(0).toLocaleUpperCase('fr') + rest.slice(1);
}

/**
 * Fond les répétitions : des phrases consécutives (la plus récente d'abord) de la même session, au même texte à l'heure
 * près, à moins de `REPEAT_WINDOW_MS` les unes des autres, deviennent une seule, « …, 3 fois (08:24) », à l'heure de la
 * plus récente.
 * @template {{session: string, at: number, text: string}} T
 * @param {T[]} items la plus récente d'abord
 * @returns {T[]}
 */
export function mergeRepeats(items) {
  /** @type {{item: T, base: string, clock: string, count: number, oldest: number}[]} */
  const groups = [];
  for (const item of items) {
    const clock = item.text.match(TRAILING_CLOCK)?.[1] ?? '';
    const base = item.text.replace(TRAILING_CLOCK, '');
    const last = groups[groups.length - 1];
    if (last && last.item.session === item.session && last.base === base && last.oldest - item.at <= REPEAT_WINDOW_MS) {
      last.count += 1;
      last.oldest = item.at;
    } else groups.push({ item, base, clock, count: 1, oldest: item.at });
  }
  return groups.map(({ item, base, clock, count }) =>
    count === 1 ? item : { ...item, text: `${base}, ${count} fois${clock ? ` (${clock})` : ''}` },
  );
}

/** @type {import('./router.js').View} */
export function mountAbsence(root, { store, request }) {
  let alive = true;
  /** La dernière réponse de la chronologie : l'écran se redessine d'elle quand la flotte change (noms, liens). @type {{data: any, lastVisit: number | null} | 'failed' | null} */
  let last = null;
  /** @param {...import('./dom.js').Child} body */
  const page = (...body) => root.replaceChildren(el('div', { class: 'page' }, el('h1', null, 'Absence'), ...body));
  function render() {
    if (last === null) return page(el('p', { class: 'muted' }, 'Lecture…'));
    if (last === 'failed') return page(el('p', { class: 'muted' }, "Le Mac ne répond pas : impossible de lire l'absence."));
    const { data, lastVisit } = last;
    const find = (/** @type {string} */ name) => store.state.fleet?.sessions.find((s) => s.name === name);
    const labelOf = (/** @type {string} */ n) => find(n)?.agents[0]?.label ?? find(n)?.display_name ?? "L'agent";
    const told = phrases(data.events, data.started_at, lastVisit, labelOf);
    const { head } = told;
    const items = mergeRepeats(told.items);
    const earlier = earlierPhrases(data.events, lastVisit, labelOf);
    /** @param {{session: string, text: string}} phrase @param {string} [extra] */
    const row = ({ session, text }, extra = '') => {
      const s = find(session);
      const caption = s?.display_name ?? 'Session fermée';
      return el('button', { class: `row row--phrase${extra}`, type: 'button', onclick: () => s && void (location.hash = sessionHash(s.name, targetPane(s))) }, el('span', { class: 'row__caption' }, caption), sentenceUnder(caption, labelOf(session), text));
    };
    page(
      head && el('p', { class: 'muted' }, head),
      items.length ? items.map((item) => row(item)) : el('p', { class: 'muted' }, 'Rien à signaler depuis votre dernière visite.'),
      earlier.length > 0 && el('h2', { class: 'group__title' }, 'Plus tôt'),
      earlier.map((item) => row(item, ' row--earlier')),
    );
  }
  async function load() {
    const { lastVisit } = store.state;
    const reply = await request('GET', '/timeline');
    if (!alive) return;
    // Un échec à la reprise ne chasse pas ce qui est déjà affiché.
    if (reply.kind === 'ok') last = { data: reply.data, lastVisit };
    else last ??= 'failed';
    render();
  }
  render();
  let seen = store.state.fleet;
  const unsubscribe = store.subscribe(() => {
    // Seule une nouvelle flotte change l'écran : le reste ne doit pas détacher une ligne touchée.
    if (store.state.fleet !== seen) (seen = store.state.fleet), render();
  });
  void load();
  addEventListener('idema:resume', load);
  return () => ((alive = false), unsubscribe(), removeEventListener('idema:resume', load));
}
