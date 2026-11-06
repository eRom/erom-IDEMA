// @ts-check
// Le bandeau hors ligne et la reprise toutes les 5 s au premier plan.

import { ageFr, realTimers } from './time.js';

export const RETRY_MS = 5000;

const CAUSES = {
  offline: 'Mac injoignable : Tailscale est-il actif ?',
  timeout: 'Le Mac ne répond pas : en veille, ou IdemA fermé ?',
};

/**
 * Le texte du bandeau : la cause probable, puis l'âge des données affichées s'il y en a.
 * @param {'offline' | 'timeout'} cause échec immédiat, ou délai dépassé
 * @param {number | null} fleetAt instant de la dernière flotte reçue
 * @param {number} nowMs
 */
export function bannerText(cause, fleetAt, nowMs) {
  const age = fleetAt === null ? '' : ` Données d'il y a ${ageFr(fleetAt, nowMs)}.`;
  return CAUSES[cause] + age;
}

/**
 * Une reprise périodique, démarrable et arrêtable sans effet de bord en double.
 * @param {() => void} run
 * @param {number} [intervalMs]
 * @param {import('./time.js').IntervalTimers} [timers]
 */
export function createRetry(run, intervalMs = RETRY_MS, timers = realTimers) {
  /** @type {any} */
  let id = null;
  return {
    start() {
      if (id === null) id = timers.setInterval(run, intervalMs);
    },
    stop() {
      if (id === null) return;
      timers.clearInterval(id);
      id = null;
    },
  };
}
