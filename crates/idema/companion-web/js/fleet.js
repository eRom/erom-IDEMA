// @ts-check
// L'onglet Flotte : le Mac, sa connexion et le résumé (qui lance le tri), puis les projets et leurs sessions.

import { el } from './dom.js';
import { icon } from './icons.js';
import { ageFr, realTimers } from './time.js';
import { openHash, triageHash } from './triage.js';

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
function row(s, now) {
  const agents = s.agents.map((/** @type {{label: string, logo: string | null}} */ a) =>
    a.logo ? el('img', { class: 'row__logo', src: `/logos/${encodeURIComponent(a.logo)}.png`, alt: a.label }) : el('span', { class: 'row__agent' }, a.label),
  );
  const hot = s.state === 'waiting' || s.state === 'error';
  return el(
    'button',
    { class: hot ? 'row row--hot' : 'row', type: 'button', onclick: () => void (location.hash = openHash(s)) },
    el('span', { class: `dot dot--${s.state}` }),
    el('span', { class: 'row__name' }, s.display_name),
    el('span', { class: 'row__agents' }, agents),
    el('span', { class: 'row__state' }, `${STATE_TEXT[s.state] ?? s.state} · ${ageFr(s.state_since, now)}`),
  );
}

/** @type {import('./router.js').View} */
export function mountFleet(root, { store, timers = realTimers }) {
  function render() {
    const { fleet, connection } = store.state;
    if (!fleet) return root.replaceChildren(el('p', { class: 'muted fleet__empty' }, 'Lecture de la flotte…'));
    const next = triageHash(fleet);
    const now = Date.now();
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
          el('button', { class: 'fleet__summary', type: 'button', disabled: !next, onclick: () => next && void (location.hash = next) }, summary(fleet)),
        ),
        groupFleet(fleet).map(({ title, sessions }) =>
          el('section', { class: 'group' }, el('h2', { class: 'group__title' }, title), sessions.length ? sessions.map((s) => row(s, now)) : el('p', { class: 'group__empty' }, 'Aucune session')),
        ),
      ),
    );
  }
  render();
  const unsubscribe = store.subscribe(render);
  const tick = timers.setInterval(render, 30_000); // les durées avancent sans que la flotte change
  return () => (unsubscribe(), timers.clearInterval(tick));
}
