// @ts-check
// L'onglet Absence : ce qui s'est passé depuis la dernière visite, en phrases.

import { el } from './dom.js';
import { sessionHash } from './router.js';
import { ageFr } from './time.js';
import { targetPane } from './triage.js';

/** « 16:02 », heure locale. @param {number} ms */
function clock(ms) {
  const d = new Date(ms);
  return `${String(d.getHours()).padStart(2, '0')}:${String(d.getMinutes()).padStart(2, '0')}`;
}

/**
 * Les transitions en phrases, la plus récente d'abord. Une attente se dit à sa fin, ou « attend depuis » tant qu'elle
 * dure. La tête signale un redémarrage d'IdemA depuis la dernière visite : le tampon d'avant est perdu.
 * @param {{at: number, session: string, from: string, to: string, finished: boolean}[]} events chronologiques
 * @param {number} startedAt @param {number | null} lastVisit @param {(session: string) => string} [labelOf]
 */
export function phrases(events, startedAt, lastVisit, labelOf = () => "L'agent") {
  /** @type {Map<string, number>} */
  const waits = new Map();
  /** @type {{session: string, at: number, text: string}[]} */
  const items = [];
  for (const { at, session, from, to, finished } of events) {
    if (at < (lastVisit ?? 0)) continue;
    const say = (/** @type {string} */ text) => items.push({ session, at, text: `${labelOf(session)} ${text}` });
    const since = waits.get(session);
    if (to === 'waiting') waits.set(session, at);
    else if (from === 'waiting') {
      waits.delete(session);
      if (since !== undefined) say(`a attendu ${ageFr(since, at)}${to === 'running' ? ', puis a repris' : ''} (${clock(at)})`);
      else if (to === 'running') say(`a repris (${clock(at)})`);
    }
    if (to === 'error') say(`est passé en erreur (${clock(at)})`);
    if (finished) say(`a fini (${clock(at)})`);
  }
  for (const [session, at] of waits) items.push({ session, at, text: `${labelOf(session)} attend depuis ${clock(at)}` });
  items.reverse().sort((a, b) => b.at - a.at); // à égalité, la dernière dite passe devant
  return { head: startedAt > (lastVisit ?? 0) ? `Depuis le démarrage d'IdemA à ${clock(startedAt)}` : null, items };
}

/** @type {import('./router.js').View} */
export function mountAbsence(root, { store, request }) {
  let alive = true;
  /** @param {...import('./dom.js').Child} body */
  const page = (...body) => root.replaceChildren(el('div', { class: 'page' }, el('h1', null, 'Absence'), ...body));
  async function load() {
    const { lastVisit, fleet } = store.state;
    const reply = await request('GET', `/timeline?since=${lastVisit ?? 0}`);
    if (!alive) return;
    if (reply.kind !== 'ok') return page(el('p', { class: 'muted' }, "Le Mac ne répond pas : impossible de lire l'absence."));
    const find = (/** @type {string} */ name) => fleet?.sessions.find((s) => s.name === name);
    const { head, items } = phrases(reply.data.events, reply.data.started_at, lastVisit, (n) => find(n)?.agents[0]?.label ?? find(n)?.display_name ?? "L'agent");
    page(
      head && el('p', { class: 'muted' }, head),
      items.length
        ? items.map(({ session, text }) => {
            const s = find(session);
            return el('button', { class: 'row row--phrase', type: 'button', onclick: () => s && void (location.hash = sessionHash(s.name, targetPane(s))) }, el('span', { class: 'row__caption' }, s?.display_name ?? 'Session fermée'), text);
          })
        : el('p', { class: 'muted' }, 'Rien à signaler depuis votre dernière visite.'),
    );
  }
  void load();
  addEventListener('idema:resume', load);
  return () => ((alive = false), removeEventListener('idema:resume', load));
}
