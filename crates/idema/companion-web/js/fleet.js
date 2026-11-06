// @ts-check
// L'onglet Flotte : le Mac, sa connexion et le résumé (qui lance le tri), puis les projets et leurs sessions.

import { el } from './dom.js';
import { icon } from './icons.js';
import { realTimers, relativeFr } from './time.js';
import { openHash, startTriage, stopTriage, triageHash } from './triage.js';

/** « 2 attendent · 1 erreur · 3 en cours », zéros omis. @param {{sessions: {state: string}[]}} fleet */
export function summary({ sessions }) {
  const n = (/** @type {string} */ state) => sessions.filter((s) => s.state === state).length;
  const [w, e, r] = [n('waiting'), n('error'), n('running')];
  const parts = [w && `${w} ${w > 1 ? 'attendent' : 'attend'}`, e && `${e} erreur${e > 1 ? 's' : ''}`, r && `${r} en cours`].filter(Boolean);
  return parts.length ? parts.join(' · ') : 'Tout est calme';
}

/**
 * Bouillons en tête (`ideation_label` présent), puis les projets dans l'ordre reçu, même vides ; une session d'un
 * projet inconnu reste visible sous le nom de son dossier.
 * @param {import('./store.js').Fleet} fleet
 * @returns {{title: string, sessions: any[]}[]}
 */
export function groupFleet({ sessions, projects, ideation_label: label }) {
  const broths = sessions.filter((s) => s.ideation);
  const groups = label != null || broths.length ? [{ title: label ?? 'Bouillons', sessions: broths }] : [];
  for (const { path, name } of projects) groups.push({ title: name, sessions: sessions.filter((s) => !s.ideation && s.project === path) });
  for (const path of new Set(sessions.filter((s) => !s.ideation && !projects.some((p) => p.path === s.project)).map((s) => s.project))) {
    groups.push({ title: path.split('/').filter(Boolean).pop() ?? path, sessions: sessions.filter((s) => !s.ideation && s.project === path) });
  }
  return groups;
}

const STATE_TEXT = /** @type {Record<string, string>} */ ({ idle: 'au repos', running: 'en cours', waiting: 'attend', error: 'erreur' });
const LINK_DOT = /** @type {const} */ ({ ok: 'running', timeout: 'waiting', offline: 'error' });

/** @param {any} s @param {number} now */
const stateText = (s, now) => `${STATE_TEXT[s.state] ?? s.state} · ${relativeFr(s.state_since, now)}`;

/** @param {any} s @param {[HTMLElement, any][]} ages les durées, remises à jour sur place par la minuterie */
function row(s, ages) {
  const agents = s.agents.map((/** @type {{label: string, logo: string | null}} */ a) => {
    if (!a.logo) return el('span', { class: 'row__agent' }, a.label);
    const logo = el('img', { class: 'row__logo', src: `/logos/${encodeURIComponent(a.logo)}.png`, alt: a.label });
    // Les logos viennent du Mac et ne sont pas dans le cache du service worker : sans lui, le nom de l'agent les remplace.
    logo.addEventListener('error', () => logo.replaceWith(el('span', { class: 'row__agent' }, a.label)), { once: true });
    return logo;
  });
  const hot = s.state === 'waiting' || s.state === 'error';
  const state = el('span', { class: 'row__state' }, stateText(s, Date.now()));
  ages.push([state, s]);
  return el(
    'button',
    { class: hot ? 'row row--hot' : 'row', type: 'button', onclick: () => void (location.hash = openHash(s)) },
    el('span', { class: `dot dot--${s.state}` }),
    el('span', { class: 'row__name' }, s.display_name),
    el('span', { class: 'row__agents' }, agents),
    state,
  );
}

/** @type {import('./router.js').View} */
export function mountFleet(root, { store, timers = realTimers }) {
  // Revenir à la flotte met fin au tri ; le résumé le relance.
  stopTriage();
  /** @type {[HTMLElement, any][]} */
  let ages = [];
  let drawn = /** @type {any} */ (null);
  let drawnLink = '';
  function render() {
    const { fleet, connection } = store.state;
    // Une notification qui ne change rien ne refait pas l'écran : un bouton touché ne doit pas être détaché.
    if (drawn !== null && fleet === drawn && connection === drawnLink) return;
    drawn = fleet;
    drawnLink = connection;
    ages = [];
    if (!fleet) return root.replaceChildren(el('p', { class: 'muted fleet__empty' }, 'Lecture de la flotte…'));
    const next = triageHash(fleet);
    root.replaceChildren(
      el(
        'div',
        null,
        el(
          'header',
          { class: 'fleet__head' },
          el('h1', null, fleet.mac),
          el('span', { class: `dot dot--${LINK_DOT[connection]}`, role: 'img', 'aria-label': connection === 'ok' ? 'Connecté' : 'Connexion perdue' }),
          el('button', { class: 'fleet__gear', type: 'button', 'aria-label': 'Réglages', onclick: () => void (location.hash = '#/settings') }, icon('settings')),
          el('button', { class: 'fleet__summary', type: 'button', disabled: !next, onclick: () => next && (startTriage(), void (location.hash = next)) }, summary(fleet)),
        ),
        groupFleet(fleet).map(({ title, sessions }) =>
          el('section', { class: 'group' }, el('h2', { class: 'group__title' }, title), sessions.length ? sessions.map((s) => row(s, ages)) : el('p', { class: 'group__empty' }, 'Aucune session')),
        ),
      ),
    );
  }
  render();
  const unsubscribe = store.subscribe(render);
  // Les durées avancent sans que la flotte change : on réécrit leur texte, sans toucher aux boutons.
  const tick = timers.setInterval(() => ages.forEach(([span, s]) => void (span.textContent = stateText(s, Date.now()))), 30_000);
  return () => (unsubscribe(), timers.clearInterval(tick));
}
