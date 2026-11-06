import { afterEach, beforeEach, describe, expect, test } from 'bun:test';
import { groupFleet, mountFleet, summary } from '../js/fleet.js';
import { createStore } from '../js/store.js';

const NOW = 1_800_000_000_000;
const MIN = 60_000;

/** @param {string} name @param {string} state @param {object} [extra] */
function session(name, state, extra = {}) {
  return { name, display_name: name, project: '/p/a', ideation: false, state, state_since: NOW - 3 * MIN, seen: true, agents: [], panes: [{ id: 0, active: true, state }], ...extra };
}

/** @param {any[]} sessions @param {object} [extra] */
const fleet = (sessions, extra = {}) => ({ mac: 'Mac de Romain', generated_at: NOW, badge: 0, ideation_label: null, projects: [{ path: '/p/a', name: 'alpha' }], sessions, ...extra });

describe('summary', () => {
  test('says nothing is going on when every session is idle', () => {
    expect(summary(fleet([session('a', 'idle')]))).toBe('Tout est calme');
    expect(summary(fleet([]))).toBe('Tout est calme');
  });

  test('agrees in the singular and omits zeroes', () => {
    expect(summary(fleet([session('a', 'waiting')]))).toBe('1 attend');
    expect(summary(fleet([session('a', 'error')]))).toBe('1 erreur');
    expect(summary(fleet([session('a', 'running')]))).toBe('1 en cours');
  });

  test('agrees in the plural, in the order waiting, error, running', () => {
    const sessions = [session('a', 'running'), session('b', 'error'), session('c', 'waiting'), session('d', 'waiting'), session('e', 'error'), session('f', 'running'), session('g', 'running')];
    expect(summary(fleet(sessions))).toBe('2 attendent · 2 erreurs · 3 en cours');
  });
});

describe('groupFleet', () => {
  test('puts the broths first, then the projects in the order received', () => {
    const sessions = [session('p1', 'idle', { project: '/p/b' }), session('br', 'idle', { project: '', ideation: true }), session('p2', 'idle', { project: '/p/a' })];
    const groups = groupFleet(fleet(sessions, { ideation_label: 'Bouillons', projects: [{ path: '/p/b', name: 'beta' }, { path: '/p/a', name: 'alpha' }] }));
    expect(groups.map((g) => g.title)).toEqual(['Bouillons', 'beta', 'alpha']);
    expect(groups.map((g) => g.sessions.map((s) => s.name))).toEqual([['br'], ['p1'], ['p2']]);
  });

  test('keeps an empty project, drops the broths section when ideation is off', () => {
    const groups = groupFleet(fleet([], { projects: [{ path: '/p/a', name: 'alpha' }] }));
    expect(groups).toEqual([{ title: 'alpha', sessions: [] }]);
  });

  test('a session of an unknown project still shows, under the folder name', () => {
    const groups = groupFleet(fleet([session('x', 'idle', { project: '/old/gamma' })]));
    expect(groups.map((g) => g.title)).toEqual(['alpha', 'gamma']);
    expect(groups[1].sessions.map((s) => s.name)).toEqual(['x']);
  });
});

describe('mountFleet', () => {
  /** @type {HTMLElement} */
  let root;
  beforeEach(() => {
    localStorage.clear();
    location.hash = '#/';
    root = document.createElement('div');
    document.body.replaceChildren(root);
  });
  afterEach(() => document.body.replaceChildren());

  const timers = { setInterval: () => 1, clearInterval() {}, setTimeout: () => 1, clearTimeout() {} };

  /** @param {any} data */
  function mount(data) {
    const store = createStore(localStorage);
    if (data) store.setFleet(data, NOW);
    const unmount = mountFleet(root, /** @type {any} */ ({ store, timers }));
    return { store, unmount: /** @type {() => void} */ (unmount) };
  }

  test('shows the Mac, the summary, the rows with their state and duration, an agent without logo by its name', () => {
    const sessions = [
      session('idema-aaaa1111', 'waiting', { display_name: 'refonte', agents: [{ label: 'Claude', logo: 'claude' }, { label: 'Aider', logo: null }] }),
      session('idema-bbbb2222', 'idle', { display_name: 'calme' }),
    ];
    mount(fleet(sessions));
    expect(root.querySelector('h1')?.textContent).toBe('Mac de Romain');
    expect(root.querySelector('.fleet__summary')?.textContent).toBe('1 attend');
    const rows = [...root.querySelectorAll('.row')];
    expect(rows.length).toBe(2);
    expect(rows[0].querySelector('.dot--waiting')).not.toBeNull();
    expect(rows[1].querySelector('.dot--idle')).not.toBeNull();
    expect(rows[0].querySelector('img')?.getAttribute('src')).toBe('/logos/claude.png');
    expect(rows[0].textContent).toContain('Aider');
    expect(rows[0].querySelector('.row__state')?.textContent).toContain('attend');
    expect(rows[0].classList.contains('row--hot')).toBe(true);
    expect(rows[1].classList.contains('row--hot')).toBe(false);
  });

  test('a row opens its session on the waiting pane, never acknowledging anything', () => {
    const waiting = session('idema-aaaa1111', 'waiting', { panes: [{ id: 0, active: true, state: 'idle' }, { id: 2, active: false, state: 'waiting' }] });
    mount(fleet([waiting]));
    /** @type {HTMLElement} */ (root.querySelector('.row')).click();
    expect(location.hash).toBe('#/s/idema-aaaa1111/2');
  });

  test('the summary starts the triage on the first waiting session; the gear opens the settings', () => {
    mount(fleet([session('idema-cccc3333', 'error'), session('idema-dddd4444', 'waiting')]));
    /** @type {HTMLElement} */ (root.querySelector('.fleet__summary')).click();
    expect(location.hash).toBe('#/s/idema-dddd4444/0');
    /** @type {HTMLElement} */ (root.querySelector('button[aria-label="Réglages"]')).click();
    expect(location.hash).toBe('#/settings');
  });

  test('the summary does nothing when everything is calm', () => {
    mount(fleet([session('a', 'idle')]));
    const button = /** @type {HTMLButtonElement} */ (root.querySelector('.fleet__summary'));
    expect(button.disabled).toBe(true);
  });

  test('follows the store: a new fleet and the connection dot', () => {
    const { store } = mount(fleet([session('a', 'idle')]));
    expect(root.querySelector('.fleet__head .dot--running')).not.toBeNull();
    store.setConnection('offline');
    expect(root.querySelector('.fleet__head .dot--error')).not.toBeNull();
    store.setFleet(fleet([session('a', 'running'), session('b', 'running')]), NOW);
    expect(root.querySelectorAll('.row').length).toBe(2);
    expect(root.querySelector('.fleet__summary')?.textContent).toBe('2 en cours');
  });

  test('without any fleet yet it says it is reading, and a text from the Mac stays text', () => {
    const { store, unmount } = mount(null);
    expect(root.textContent).toContain('Lecture de la flotte');
    store.setFleet(fleet([session('a', 'idle', { display_name: '<img src=x onerror=alert(1)>' })]), NOW);
    expect(root.querySelector('.row img')).toBeNull();
    expect(root.querySelector('.row__name')?.textContent).toBe('<img src=x onerror=alert(1)>');
    unmount();
    store.setFleet(fleet([]), NOW);
    expect(root.querySelector('.row')).not.toBeNull(); // plus abonné : le DOM démonté ne bouge plus
  });
});
