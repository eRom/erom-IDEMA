import { afterEach, beforeEach, describe, expect, test } from 'bun:test';
import { mountAbsence, phrases } from '../js/absence.js';
import { createStore } from '../js/store.js';

const MIN = 60_000;
/** Un instant local : l'heure affichée ne dépend pas du fuseau de la machine de test. @param {number} h @param {number} m */
const at = (h, m) => new Date(2026, 10, 5, h, m).getTime();

/** @param {number} time @param {string} session @param {string} from @param {string} to @param {boolean} [finished] */
const event = (time, session, from, to, finished = false) => ({ at: time, session, from, to, finished });
const text = (/** @type {any} */ result) => result.items.map((/** @type {any} */ item) => item.text);

describe('phrases', () => {
  const label = (/** @type {string} */ session) => ({ a: 'Codex', b: 'Claude' })[session] ?? "L'agent";

  test('a wait that ended with a resume', () => {
    const result = phrases([event(at(15, 50), 'a', 'running', 'waiting'), event(at(16, 2), 'a', 'waiting', 'running')], at(8, 0), at(15, 0), label);
    expect(text(result)).toEqual(['Codex a attendu 12 min, puis a repris (16:02)']);
  });

  test('an error, and the end of a long run', () => {
    const result = phrases([event(at(16, 20), 'b', 'running', 'error'), event(at(16, 31), 'a', 'running', 'idle', true)], at(8, 0), at(15, 0), label);
    expect(text(result)).toEqual(['Codex a fini (16:31)', 'Claude est passé en erreur (16:20)']);
  });

  test('a wait still open says since when', () => {
    const result = phrases([event(at(16, 2), 'a', 'running', 'waiting')], at(8, 0), at(15, 0), label);
    expect(text(result)).toEqual(['Codex attend depuis 16:02']);
  });

  test('a wait that ended some other way, and a resume whose start was before the visit', () => {
    const ended = phrases([event(at(16, 0), 'a', 'idle', 'waiting'), event(at(16, 3), 'a', 'waiting', 'idle')], at(8, 0), at(15, 0), label);
    expect(text(ended)).toEqual(['Codex a attendu 3 min (16:03)']);
    const orphan = phrases([event(at(16, 3), 'a', 'waiting', 'running')], at(8, 0), at(15, 0), label);
    expect(text(orphan)).toEqual(['Codex a repris (16:03)']);
  });

  test('a wait that turns into an error tells both', () => {
    const result = phrases([event(at(16, 0), 'b', 'running', 'waiting'), event(at(16, 5), 'b', 'waiting', 'error')], at(8, 0), at(15, 0), label);
    expect(text(result)).toEqual(['Claude est passé en erreur (16:05)', 'Claude a attendu 5 min (16:05)']);
  });

  test('waits of two sessions that interleave stay apart', () => {
    const result = phrases([event(at(16, 0), 'a', 'running', 'waiting'), event(at(16, 1), 'b', 'running', 'waiting'), event(at(16, 11), 'b', 'waiting', 'running'), event(at(16, 20), 'a', 'waiting', 'running')], at(8, 0), at(15, 0), label);
    expect(text(result)).toEqual(['Codex a attendu 20 min, puis a repris (16:20)', 'Claude a attendu 10 min, puis a repris (16:11)']);
  });

  test('events from before the last visit are left out', () => {
    const result = phrases([event(at(14, 0), 'a', 'running', 'error')], at(8, 0), at(15, 0), label);
    expect(result.items).toEqual([]);
  });

  test('the head tells that IdemA restarted since the last visit', () => {
    expect(phrases([], at(9, 12), at(8, 0), label).head).toBe("Depuis le démarrage d'IdemA à 09:12");
    expect(phrases([], at(9, 12), at(10, 0), label).head).toBeNull();
    expect(phrases([], at(9, 12), null, label).head).toBe("Depuis le démarrage d'IdemA à 09:12");
  });

  test('keeps the session of each phrase and its instant', () => {
    const [item] = phrases([event(at(16, 20), 'b', 'running', 'error')], at(8, 0), at(15, 0), label).items;
    expect(item).toMatchObject({ session: 'b', at: at(16, 20) });
  });

  test('without any label the subject is the agent', () => {
    expect(text(phrases([event(at(16, 20), 'b', 'running', 'error')], at(8, 0), at(15, 0)))).toEqual(["L'agent est passé en erreur (16:20)"]);
  });
});

describe('mountAbsence', () => {
  /** @type {HTMLElement} */
  let root;
  beforeEach(() => {
    localStorage.clear();
    location.hash = '#/absence';
    root = document.createElement('div');
    document.body.replaceChildren(root);
  });
  afterEach(() => document.body.replaceChildren());
  const flush = () => new Promise((resolve) => setTimeout(resolve, 0));

  /** @param {any} reply */
  function mount(reply) {
    const store = createStore(localStorage);
    store.setFleet({ mac: 'Mac', generated_at: 1, badge: 0, projects: [], sessions: [{ name: 'idema-aaaa1111', display_name: 'refonte', state: 'idle', agents: [{ label: 'Codex', logo: null }], panes: [{ id: 0, active: true, state: 'idle' }] }] });
    store.touchVisit(at(15, 0));
    /** @type {string[]} */
    const paths = [];
    const request = async (/** @type {string} */ _method, /** @type {string} */ path) => (paths.push(path), reply);
    const unmount = mountAbsence(root, /** @type {any} */ ({ store, request }));
    return { paths, unmount };
  }

  test('asks the timeline since the last visit and tells it in phrases; a row opens its session', async () => {
    const { paths } = mount({ kind: 'ok', data: { started_at: at(8, 0), events: [event(at(16, 20), 'idema-aaaa1111', 'running', 'error')] } });
    await flush();
    expect(paths).toEqual([`/timeline?since=${at(15, 0)}`]);
    const row = /** @type {HTMLElement} */ (root.querySelector('.row'));
    expect(row.textContent).toContain('Codex est passé en erreur (16:20)');
    expect(row.textContent).toContain('refonte');
    row.click();
    expect(location.hash).toBe('#/s/idema-aaaa1111/0');
  });

  test('says when nothing happened, and when the Mac does not answer', async () => {
    mount({ kind: 'ok', data: { started_at: at(8, 0), events: [] } });
    await flush();
    expect(root.textContent).toContain('Rien à signaler');
    mount({ kind: 'offline' });
    await flush();
    expect(root.textContent).toContain('Le Mac ne répond pas');
  });

  test('the restart head shows above the phrases', async () => {
    mount({ kind: 'ok', data: { started_at: at(15, 30), events: [] } });
    await flush();
    expect(root.textContent).toContain("Depuis le démarrage d'IdemA à 15:30");
  });

  test('a session closed since is shown but does not open', async () => {
    mount({ kind: 'ok', data: { started_at: at(8, 0), events: [event(at(16, 20), 'idema-gone0000', 'running', 'error')] } });
    await flush();
    const row = /** @type {HTMLElement} */ (root.querySelector('.row'));
    row.click();
    expect(location.hash).toBe('#/absence');
    expect(row.textContent).toContain('est passé en erreur');
  });
});
