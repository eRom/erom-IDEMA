import { describe, expect, test } from 'bun:test';
import { createPresence } from '../js/presence.js';

/** Une horloge à la main : l'intervalle ne tourne que quand le test le dit. */
function harness() {
  /** @type {any[]} */
  const sent = [];
  /** @type {boolean[]} */
  const keepalives = [];
  /** @type {(() => void) | null} */
  let tick = null;
  const presence = createPresence((body, keepalive) => (sent.push(body), keepalives.push(keepalive)), 15000, {
    setInterval: (fn) => ((tick = fn), 1),
    clearInterval: () => {
      tick = null;
    },
  });
  return { presence, sent, keepalives, tick: () => tick?.(), hasTimer: () => tick !== null };
}

describe('createPresence', () => {
  test('a session screen declares itself at once and every 15 s', () => {
    const { presence, sent, tick, hasTimer } = harness();
    presence.setTarget({ session: 'idema-1', pane: 2 });
    expect(sent).toEqual([{ session: 'idema-1', pane: 2 }]);
    expect(hasTimer()).toBe(true);
    tick();
    expect(sent.length).toBe(2);
  });

  test('changing screen declares the new one, leaving sends null once', () => {
    const { presence, sent, hasTimer } = harness();
    presence.setTarget({ session: 'a', pane: 1 });
    presence.setTarget({ session: 'a', pane: 2 });
    presence.setTarget(null);
    presence.setTarget(null);
    expect(sent).toEqual([{ session: 'a', pane: 1 }, { session: 'a', pane: 2 }, null]);
    expect(hasTimer()).toBe(false);
  });

  test('hiding the page sends null and silences the heartbeat, showing it again declares anew', () => {
    const { presence, sent, hasTimer } = harness();
    presence.setTarget({ session: 'a', pane: 1 });
    presence.setVisible(false);
    expect(sent.at(-1)).toBeNull();
    expect(hasTimer()).toBe(false);
    presence.setVisible(false);
    expect(sent.length).toBe(2);
    presence.setVisible(true);
    expect(sent.at(-1)).toEqual({ session: 'a', pane: 1 });
    expect(hasTimer()).toBe(true);
  });

  test('only the null sent on hide asks to survive the page being frozen', () => {
    const { presence, sent, keepalives } = harness();
    presence.setTarget({ session: 'a', pane: 1 });
    presence.setVisible(false);
    expect(sent).toEqual([{ session: 'a', pane: 1 }, null]);
    expect(keepalives).toEqual([false, true]);
  });

  test('nothing to declare and nothing sent while the page is on another screen', () => {
    const { presence, sent } = harness();
    presence.setVisible(false);
    presence.setVisible(true);
    expect(sent).toEqual([]);
  });
});
