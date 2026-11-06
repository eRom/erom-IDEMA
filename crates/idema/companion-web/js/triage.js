// @ts-check
// Le tri : les sessions qui demandent du monde, et le pane à ouvrir.

import { sessionHash } from './router.js';

/** @typedef {{name: string, state: string, panes: {id: number, active: boolean, state: string}[]}} Sess */

/** Attente puis erreur, chacune dans l'ordre de la barre. @template {Sess} S @param {{sessions: S[]}} fleet */
export const triageQueue = ({ sessions }) => ['waiting', 'error'].flatMap((state) => sessions.filter((s) => s.state === state));

/** Le pane qui attend ou échoue (celui de l'état de la session d'abord), sinon l'actif, sinon le premier. @param {Sess} session */
export function targetPane({ state, panes }) {
  const by = (/** @type {string} */ s) => panes.find((p) => p.state === s);
  return (by(state === 'error' ? 'error' : 'waiting') ?? by('waiting') ?? by('error') ?? panes.find((p) => p.active) ?? panes[0])?.id ?? 0;
}

/** @param {Sess} session */
export const openHash = (session) => sessionHash(session.name, targetPane(session));

/** Le fragment de la prochaine session à traiter hors `handled`, ou `null` (« Tout est traité »). @param {{sessions: Sess[]}} fleet @param {readonly string[]} [handled] */
export function triageHash(fleet, handled = []) {
  const next = triageQueue(fleet).find((s) => !handled.includes(s.name));
  return next ? openHash(next) : null;
}

// Le tri en cours : ouvert par le résumé de la flotte, fermé quand on revient à la flotte. Les sessions déjà vues y
// restent, pour que « Suivante » ne rouvre pas celle qu'on vient de quitter.
let running = false;
/** @type {string[]} */
let handled = [];

export const isTriage = () => running;
export const handledSessions = () => handled;

export function startTriage() {
  running = true;
  handled = [];
}

export function stopTriage() {
  running = false;
  handled = [];
}

/** @param {string} name */
export function markHandled(name) {
  if (running && !handled.includes(name)) handled.push(name);
}
