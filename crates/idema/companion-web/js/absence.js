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
 * Ce qui s'est passé avant la dernière visite et reste dans le tampon, la plus récente d'abord, `EARLIER_MAX` phrases au
 * plus. Les attentes s'apparient sur tout le tampon. Sans visite précédente, tout est « nouveau » : rien d'antérieur.
 * @param {TimelineEvent[]} events chronologiques
 * @param {number | null} lastVisit @param {(session: string) => string} [labelOf]
 */
export function earlierPhrases(events, lastVisit, labelOf = () => "L'agent") {
  if (!lastVisit) return [];
  return tell(events, labelOf, { pairFrom: 0, from: 0, until: lastVisit }).slice(0, EARLIER_MAX);
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
    const { head, items } = phrases(data.events, data.started_at, lastVisit, labelOf);
    const earlier = earlierPhrases(data.events, lastVisit, labelOf);
    /** @param {{session: string, text: string}} phrase @param {string} [extra] */
    const row = ({ session, text }, extra = '') => {
      const s = find(session);
      return el('button', { class: `row row--phrase${extra}`, type: 'button', onclick: () => s && void (location.hash = sessionHash(s.name, targetPane(s))) }, el('span', { class: 'row__caption' }, s?.display_name ?? 'Session fermée'), text);
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
