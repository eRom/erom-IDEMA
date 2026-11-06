// @ts-check
// La présence : le pane que le téléphone montre au premier plan, redite toutes les 15 s (le Mac l'oublie après 30 s).

import { realTimers } from './time.js';

export const HEARTBEAT_MS = 15_000;

/** @typedef {{session: string, pane: number} | null} PresenceBody */

/**
 * @param {(body: PresenceBody, keepalive: boolean) => void} send `keepalive` : le `null` du masquage doit partir même si la page gèle
 * @param {number} [intervalMs]
 * @param {import('./time.js').IntervalTimers} [timers]
 */
export function createPresence(send, intervalMs = HEARTBEAT_MS, timers = realTimers) {
  /** @type {{session: string, pane: number} | null} */
  let target = null;
  let visible = true;
  /** @type {any} */
  let timer = null;
  /** Le dernier envoi était-il un `null` ? Deux `null` de suite ne servent à rien. */
  let lastWasNull = true;

  function push() {
    const body = visible && target ? { session: target.session, pane: target.pane } : null;
    if (body === null && lastWasNull) return;
    lastWasNull = body === null;
    send(body, !visible);
  }

  function sync() {
    if (timer !== null) timers.clearInterval(timer);
    timer = null;
    push();
    if (visible && target) timer = timers.setInterval(push, intervalMs);
  }

  return {
    /** @param {{session: string, pane: number} | null} next l'écran de session ouvert, ou `null` */
    setTarget(next) {
      target = next;
      sync();
    },
    /** @param {boolean} next la page est-elle au premier plan ? */
    setVisible(next) {
      if (next === visible) return;
      visible = next;
      sync();
    },
  };
}
