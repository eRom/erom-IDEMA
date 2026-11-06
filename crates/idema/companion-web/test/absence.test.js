import { afterEach, beforeEach, describe, expect, test } from 'bun:test';
import { EARLIER_MAX, earlierPhrases, mergeRepeats, mountAbsence, phrases, REPEAT_WINDOW_MS, sentenceUnder } from '../js/absence.js';
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

describe('earlierPhrases', () => {
  test('tells what is before the last visit, with the waits paired over the whole buffer', () => {
    const events = [event(at(14, 0), 'a', 'running', 'waiting'), event(at(14, 30), 'a', 'waiting', 'running'), event(at(14, 40), 'b', 'running', 'waiting'), event(at(14, 50), 'c', 'running', 'waiting'), event(at(16, 0), 'b', 'waiting', 'running')];
    // L'attente de b se clôt après la visite : elle n'est plus ouverte, sa fin est du neuf. Celle de c dure encore.
    expect(text({ items: earlierPhrases(events, at(15, 0)) })).toEqual(["L'agent attend depuis 14:50", "L'agent a attendu 30 min, puis a repris (14:30)"]);
  });

  test('a first visit has nothing older: everything is new', () => {
    expect(earlierPhrases([event(at(14, 0), 'a', 'running', 'error')], null)).toEqual([]);
  });
});

describe('sentenceUnder', () => {
  test('drops the subject the caption already names and starts with the verb', () => {
    expect(sentenceUnder('Opus 5.5', 'Opus 5.5', 'Opus 5.5 a attendu 3 min (08:22)')).toBe('A attendu 3 min (08:22)');
    expect(sentenceUnder('Opus 5.5', 'Opus 5.5', 'Opus 5.5 est passé en erreur (08:30)')).toBe('Est passé en erreur (08:30)');
  });

  test('keeps the subject when the caption names something else', () => {
    expect(sentenceUnder('refonte', 'Codex', 'Codex a fini (16:20)')).toBe('Codex a fini (16:20)');
    // Un en-tête qui n'est qu'un début du nom de l'agent n'est pas le sujet.
    expect(sentenceUnder('Opus', 'Opus 5.5', 'Opus 5.5 a fini (16:20)')).toBe('Opus 5.5 a fini (16:20)');
  });
});

describe('mergeRepeats', () => {
  const phrase = (/** @type {number} */ time, /** @type {string} */ session, /** @type {string} */ words) => {
    const d = new Date(time);
    const clock = `${String(d.getHours()).padStart(2, '0')}:${String(d.getMinutes()).padStart(2, '0')}`;
    return { session, at: time, text: `${words} (${clock})` };
  };

  test('close identical phrases of one session become one, with the count and the latest time', () => {
    const items = [
      phrase(at(8, 24), 'a', 'Opus a attendu moins d’une minute'),
      phrase(at(8, 23), 'a', 'Opus a attendu moins d’une minute'),
      phrase(at(8, 22), 'a', 'Opus a attendu moins d’une minute'),
      phrase(at(8, 22), 'a', 'Opus a attendu 3 min'),
    ];
    expect(mergeRepeats(items).map((item) => item.text)).toEqual([
      'Opus a attendu moins d’une minute, 3 fois (08:24)',
      'Opus a attendu 3 min (08:22)',
    ]);
  });

  test('phrases far apart, of another session, or with another text stay apart', () => {
    const far = [phrase(at(14, 0), 'a', 'Codex est passé en erreur'), phrase(at(14, 0) - REPEAT_WINDOW_MS - MIN, 'a', 'Codex est passé en erreur')];
    expect(mergeRepeats(far)).toHaveLength(2);
    const sessions = [phrase(at(8, 24), 'a', 'X a fini'), phrase(at(8, 23), 'b', 'X a fini')];
    expect(mergeRepeats(sessions)).toHaveLength(2);
    const interleaved = [phrase(at(8, 24), 'a', 'X a fini'), phrase(at(8, 23), 'a', 'X a repris'), phrase(at(8, 22), 'a', 'X a fini')];
    expect(mergeRepeats(interleaved)).toHaveLength(3);
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
    return { store, paths, unmount };
  }

  test('asks the timeline since the last visit and tells it in phrases; a row opens its session', async () => {
    const { paths } = mount({ kind: 'ok', data: { started_at: at(8, 0), events: [event(at(16, 20), 'idema-aaaa1111', 'running', 'error')] } });
    await flush();
    // Tout le tampon est demandé : la page range elle-même le neuf et l'ancien autour de la dernière visite.
    expect(paths).toEqual(['/timeline']);
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


  test('news first, then the older events under "Plus tôt", dimmed and most recent first', async () => {
    mount({ kind: 'ok', data: { started_at: at(8, 0), events: [event(at(13, 0), 'idema-aaaa1111', 'running', 'error'), event(at(14, 0), 'idema-aaaa1111', 'running', 'error'), event(at(16, 20), 'idema-aaaa1111', 'running', 'error')] } });
    await flush();
    const rows = [...root.querySelectorAll('.row')].map((row) => row.textContent);
    expect(rows[0]).toContain('(16:20)');
    expect(rows.slice(1).map((row) => row?.match(/\(\d\d:\d\d\)/)?.[0])).toEqual(['(14:00)', '(13:00)']);
    const kids = [...(root.querySelector('.page')?.children ?? [])];
    const title = kids.findIndex((node) => node.textContent === 'Plus tôt');
    expect(title).toBeGreaterThan(0);
    expect(kids.slice(title + 1).every((node) => node.classList.contains('row--earlier'))).toBe(true);
    expect(kids.slice(0, title).some((node) => node.classList.contains('row--earlier'))).toBe(false);
    expect(root.textContent).not.toContain('Rien à signaler');
  });

  test('nothing new but older events: the sentence stays first and "Plus tôt" follows', async () => {
    mount({ kind: 'ok', data: { started_at: at(8, 0), events: [event(at(14, 0), 'idema-aaaa1111', 'running', 'error')] } });
    await flush();
    const kids = [...(root.querySelector('.page')?.children ?? [])].map((node) => node.textContent ?? '');
    const sentence = kids.findIndex((text) => text.includes('Rien à signaler depuis votre dernière visite.'));
    const title = kids.findIndex((text) => text === 'Plus tôt');
    expect(sentence).toBeGreaterThan(-1);
    expect(title).toBeGreaterThan(sentence);
    expect(kids[title + 1]).toContain('Codex est passé en erreur (14:00)');
  });

  test('an empty buffer keeps the sentence alone', async () => {
    mount({ kind: 'ok', data: { started_at: at(8, 0), events: [] } });
    await flush();
    expect(root.textContent).toContain('Rien à signaler depuis votre dernière visite.');
    expect(root.textContent).not.toContain('Plus tôt');
    expect(root.querySelector('.row')).toBeNull();
  });

  test('"Plus tôt" shows 30 lines at most', async () => {
    // Six minutes d'écart : au-delà de la fenêtre des répétitions, chaque erreur garde sa ligne.
    const events = Array.from({ length: 45 }, (_, n) => event(at(10, 0) + n * 6 * MIN, 'idema-aaaa1111', 'running', 'error'));
    mount({ kind: 'ok', data: { started_at: at(8, 0), events } });
    await flush();
    const rows = root.querySelectorAll('.row--earlier');
    expect(rows.length).toBe(EARLIER_MAX);
    expect(rows[0]?.textContent).toContain('(14:24)');
  });


  test('the restart head shows above the phrases', async () => {
    mount({ kind: 'ok', data: { started_at: at(15, 30), events: [] } });
    await flush();
    expect(root.textContent).toContain("Depuis le démarrage d'IdemA à 15:30");
  });

  test('says it is reading until the first reply', async () => {
    mount(new Promise(() => {}));
    expect(root.textContent).toContain('Lecture…');
  });

  test('a fleet that arrives after the mount names the sessions and makes the rows open', async () => {
    localStorage.clear();
    const store = createStore(localStorage);
    const request = async () => ({ kind: 'ok', data: { started_at: at(8, 0), events: [event(at(16, 20), 'idema-aaaa1111', 'running', 'error')] } });
    mountAbsence(root, /** @type {any} */ ({ store, request }));
    await flush();
    expect(root.querySelector('.row')?.textContent).toContain('Session fermée');
    expect(root.querySelector('.row')?.textContent).toContain("L'agent est passé en erreur");
    store.setFleet({ mac: 'Mac', generated_at: 1, badge: 0, projects: [], sessions: [{ name: 'idema-aaaa1111', display_name: 'refonte', state: 'idle', agents: [{ label: 'Codex', logo: null }], panes: [{ id: 3, active: true, state: 'idle' }] }] });
    const row = /** @type {HTMLElement} */ (root.querySelector('.row'));
    expect(row.textContent).toContain('refonte');
    expect(row.textContent).toContain('Codex est passé en erreur');
    row.click();
    expect(location.hash).toBe('#/s/idema-aaaa1111/3');
  });

  test('a notification that does not bring a new fleet leaves the rows alone', async () => {
    const { store } = mount({ kind: 'ok', data: { started_at: at(8, 0), events: [event(at(16, 20), 'idema-aaaa1111', 'running', 'error')] } });
    await flush();
    const row = root.querySelector('.row');
    store.setConnection('offline');
    expect(root.querySelector('.row')).toBe(row);
  });

  test('a session named after its agent reads "Opus 5.5" once per row, and close repeats are merged', async () => {
    const store = createStore(localStorage);
    store.setFleet({ mac: 'Mac', generated_at: 1, badge: 0, projects: [], sessions: [{ name: 'idema-bbbb2222', display_name: 'Opus 5.5', state: 'idle', agents: [{ label: 'Opus 5.5', logo: null }], panes: [{ id: 0, active: true, state: 'idle' }] }] });
    store.touchVisit(at(15, 0));
    const events = [
      event(at(16, 20), 'idema-bbbb2222', 'running', 'waiting'),
      event(at(16, 20) + 20_000, 'idema-bbbb2222', 'waiting', 'idle'),
      event(at(16, 21), 'idema-bbbb2222', 'running', 'waiting'),
      event(at(16, 21) + 20_000, 'idema-bbbb2222', 'waiting', 'idle'),
    ];
    const request = async () => ({ kind: 'ok', data: { started_at: at(8, 0), events } });
    mountAbsence(root, /** @type {any} */ ({ store, request }));
    await flush();
    const rows = [...root.querySelectorAll('.row')];
    expect(rows).toHaveLength(1);
    const caption = rows[0].querySelector('.row__caption')?.textContent;
    expect(caption).toBe('Opus 5.5');
    const sentence = rows[0].textContent?.slice(caption?.length ?? 0);
    expect(sentence).toBe("A attendu moins d'une minute, 2 fois (16:21)");
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
