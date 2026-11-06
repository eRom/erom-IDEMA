// @ts-check
// Les durées lues par l'oreille : « 3 min », « 2 h », « hier ».

const MINUTE = 60_000;
const HOUR = 60 * MINUTE;
const DAY = 24 * HOUR;

/** Minutes, heures ou jours entiers d'une durée de plus d'une minute. @param {number} diff */
function coarse(diff) {
  if (diff < HOUR) return `${Math.floor(diff / MINUTE)} min`;
  if (diff < DAY) return `${Math.floor(diff / HOUR)} h`;
  return `${Math.floor(diff / DAY)} j`;
}

/**
 * Il y a combien de temps, en un mot : « à l'instant », « 3 min », « 2 h », « hier », « 3 j ».
 * @param {number} fromMs instant passé (millisecondes Unix)
 * @param {number} nowMs maintenant
 */
export function relativeFr(fromMs, nowMs) {
  const diff = Math.max(0, nowMs - fromMs);
  if (diff < MINUTE) return "à l'instant";
  return diff >= DAY && diff < 2 * DAY ? 'hier' : coarse(diff);
}

/**
 * L'âge d'une donnée, pour une phrase qui continue « il y a » : « moins d'une minute », « 5 min », « 3 h », « 2 j ».
 * @param {number} fromMs
 * @param {number} nowMs
 */
export function ageFr(fromMs, nowMs) {
  const diff = Math.max(0, nowMs - fromMs);
  return diff < MINUTE ? "moins d'une minute" : coarse(diff);
}

/**
 * @typedef {object} Timers
 * @property {(fn: () => void, ms: number) => any} setInterval
 * @property {(id: any) => void} clearInterval
 */

/**
 * Les vraies minuteries, enveloppées : `{ setInterval }` appelé comme méthode d'un objet perd son `this` et Safari
 * refuse (« Can only call Window.setInterval on instances of Window »). Les tests en injectent de fausses.
 * @type {Timers}
 */
export const realTimers = {
  setInterval: (fn, ms) => setInterval(fn, ms),
  clearInterval: (id) => clearInterval(id),
};
