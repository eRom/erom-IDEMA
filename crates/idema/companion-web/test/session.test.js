import { afterEach, beforeEach, describe, expect, test } from 'bun:test';
import { createStore } from '../js/store.js';
import { mountSession, outcomeMessage } from '../js/session.js';
import { startTriage, stopTriage } from '../js/triage.js';

const NOW = 1_800_000_000_000;
const ESC = '\x1b';
const BASE = '/sessions/idema-aaaa1111/panes/1';

describe('outcomeMessage', () => {
  /** @param {number} status @param {string} error */
  const http = (status, error) => ({ kind: /** @type {const} */ ('http'), status, error });

  test('sent and duplicate are plain successes', () => {
    expect(outcomeMessage({ kind: 'ok', data: { outcome: 'sent' } })).toEqual({ text: 'Envoyé', tone: 'ok', closed: false });
    expect(outcomeMessage({ kind: 'ok', data: { outcome: 'duplicate' } })).toEqual({ text: 'Envoyé', tone: 'ok', closed: false });
  });

  test('sent without Enter says the Enter did not leave', () => {
    const message = outcomeMessage({ kind: 'ok', data: { outcome: 'sent_without_enter' } });
    expect(message.tone).toBe('warn');
    expect(message.text).toContain('Entrée');
    expect(message.text).toContain("vérifiez l'écran");
  });

  test('each refusal has its own sentence', () => {
    expect(outcomeMessage(http(409, 'no_program')).text).toBe('Au prompt du shell : rien à répondre');
    expect(outcomeMessage(http(409, 'read_only')).text).toContain('lecture seule');
    expect(outcomeMessage(http(503, 'disconnected')).text).toContain('déconnectée');
    expect(outcomeMessage(http(503, 'expired')).text).toContain("vérifiez l'écran");
    expect(outcomeMessage(http(400, 'bad_request')).text).toContain('refusé');
    const texts = new Set(['no_program', 'read_only'].map((e) => outcomeMessage(http(409, e)).text));
    expect(texts.size).toBe(2);
  });

  test('a 404 closes the session screen', () => {
    expect(outcomeMessage(http(404, 'not_found'))).toEqual({ text: 'Session fermée', tone: 'error', closed: true });
  });

  test('a network failure says the sending is uncertain, in the formal register of the app', () => {
    for (const kind of /** @type {const} */ (['offline', 'timeout'])) {
      expect(outcomeMessage({ kind })).toEqual({ text: "Envoi incertain : vérifiez l'écran", tone: 'error', closed: false });
    }
  });

  test('an unknown refusal never shows the raw code', () => {
    const message = outcomeMessage(http(500, 'secret_internal_code'));
    expect(message.text).not.toContain('secret_internal_code');
    expect(message.tone).toBe('error');
  });
});

/** @param {string} name @param {string} state @param {object} [extra] */
const session = (name, state, extra = {}) => ({ name, display_name: name, project: '/p/a', ideation: false, state, state_since: NOW, seen: true, agents: [], panes: [{ id: 0, active: true, state }], ...extra });
/** @param {any[]} sessions */
const fleet = (sessions) => ({ mac: 'Mac', generated_at: NOW, badge: 0, ideation_label: null, projects: [{ path: '/p/a', name: 'alpha' }], sessions });

/** Un écran de Claude Code avec son menu. */
const MENU_TEXT = ['Edit src/main.rs', '', ' Do you want to make this edit?', ` ${ESC}[34m❯ 1. Yes${ESC}[0m`, '   2. Yes, allow all edits', '   3. No, tell Claude what to do', ''].join('\n');

/** Une horloge à la main : chaque setTimeout attend que le test le déclenche. */
function fakeTimers() {
  /** @type {{id: number, fn: () => void, ms: number}[]} */
  const pending = [];
  let next = 1;
  return {
    pending,
    setTimeout: (/** @type {() => void} */ fn, /** @type {number} */ ms) => {
      pending.push({ id: next, fn, ms });
      return next++;
    },
    clearTimeout: (/** @type {number} */ id) => {
      const at = pending.findIndex((t) => t.id === id);
      if (at >= 0) pending.splice(at, 1);
    },
    setInterval: () => 0,
    clearInterval() {},
    /** Déclenche la minuterie la plus ancienne, si l'une attend ce délai (ou n'importe lequel). @param {number} [ms] */
    fire(ms) {
      const at = pending.findIndex((t) => ms === undefined || t.ms === ms);
      if (at < 0) return false;
      const [timer] = pending.splice(at, 1);
      timer.fn();
      return true;
    },
  };
}

const flush = async () => {
  for (let i = 0; i < 8; i++) await Promise.resolve();
  await new Promise((resolve) => setTimeout(resolve, 0));
};

/**
 * @typedef {{method: string, path: string, body: any, headers?: Record<string, string>}} Call
 */

describe('mountSession', () => {
  /** @type {HTMLElement} */
  let root;
  /** @type {Call[]} */
  let calls;
  /** @type {Record<string, (call: Call) => any>} */
  let routes;
  let timers = fakeTimers();
  /** @type {(() => void) | void} */
  let unmount;

  beforeEach(() => {
    localStorage.clear();
    location.hash = '#/s/idema-aaaa1111/1';
    root = document.createElement('div');
    document.body.replaceChildren(root);
    calls = [];
    timers = fakeTimers();
    stopTriage();
    const screen = { text: MENU_TEXT, cols: 80, rows: 24, accepts_input: true, state: 'waiting' };
    routes = {
      [`GET ${BASE}/screen`]: () => ({ kind: 'ok', data: screen, etag: '"v1"' }),
      [`POST ${BASE}/ack`]: () => ({ kind: 'ok', data: null }),
      [`POST ${BASE}/input`]: () => ({ kind: 'ok', data: { outcome: 'sent' } }),
    };
  });
  /** @param {'visible' | 'hidden'} value */
  const setVisibility = (value) => Object.defineProperty(document, 'visibilityState', { configurable: true, get: () => value });
  afterEach(() => {
    setVisibility('visible');
    unmount?.();
    unmount = undefined;
    stopTriage();
    document.body.replaceChildren();
  });

  /** @param {{fleet?: any, route?: any}} [options] */
  function mount({ fleet: data = fleet([session('idema-aaaa1111', 'waiting', { panes: [{ id: 0, active: true, state: 'idle' }, { id: 1, active: false, state: 'waiting' }] })]), route = { name: 'session', session: 'idema-aaaa1111', pane: 1 } } = {}) {
    const store = createStore(localStorage);
    store.setFleet(data, NOW);
    const request = async (/** @type {string} */ method, /** @type {string} */ path, /** @type {any} */ body, /** @type {any} */ _keepalive, /** @type {any} */ _timeout, /** @type {any} */ headers) => {
      calls.push({ method, path, body, headers });
      const handler = routes[`${method} ${path.split('?')[0]}`];
      if (!handler) throw new Error(`unexpected ${method} ${path}`);
      return handler({ method, path, body, headers });
    };
    unmount = mountSession(root, /** @type {any} */ ({ route, store, request, timers }));
    return { store, request };
  }

  const buttons = (/** @type {string} */ selector) => [...root.querySelectorAll(selector)];
  /** @param {string} label */
  const tap = (label) => {
    const found = buttons('button').find((b) => b.textContent?.trim() === label || b.getAttribute('aria-label') === label);
    if (!found) throw new Error(`no button ${label}`);
    /** @type {HTMLElement} */ (found).click();
  };
  const inputs = () => calls.filter((c) => c.path.endsWith('/input'));

  test('shows the screen read from the Mac, colours included, and acknowledges the pane once', async () => {
    mount();
    await flush();
    expect(calls[0]).toMatchObject({ method: 'GET', path: `${BASE}/screen?history=200` });
    expect(root.querySelector('.term__text')?.textContent).toContain('Do you want to make this edit?');
    expect(calls.filter((c) => c.path.endsWith('/ack'))).toHaveLength(1);
    const blue = /** @type {HTMLElement} */ ([...root.querySelectorAll('.term__text span')].find((s) => s.textContent === '❯ 1. Yes'));
    expect(blue.style.color).not.toBe('');
  });

  test('the empty rows under a short screen are not drawn, so the text stays in view', async () => {
    routes[`GET ${BASE}/screen`] = () => ({ kind: 'ok', data: { text: `$ claude\n\x1b[41m  \x1b[0m\nDo you want?\n${'\n'.repeat(30)}`, accepts_input: true, state: 'idle' }, etag: '"x"' });
    mount();
    await flush();
    // Les lignes vides du bas partent ; celle qui porte un fond, et celles du milieu, restent.
    expect(root.querySelector('.term__text')?.textContent).toBe('$ claude\n  \nDo you want?');
  });

  test('a capture with markup shows it as text', async () => {
    routes[`GET ${BASE}/screen`] = () => ({ kind: 'ok', data: { text: '<img src=x onerror=alert(1)>', accepts_input: true, state: 'idle' }, etag: '"x"' });
    mount();
    await flush();
    expect(root.querySelector('img')).toBeNull();
    expect(root.querySelector('.term__text')?.textContent).toBe('<img src=x onerror=alert(1)>');
  });

  test('the choices read on screen become buttons that send the digit alone', async () => {
    mount();
    await flush();
    const labels = buttons('.choices button').map((b) => b.textContent);
    expect(labels).toEqual(['1 · Yes', '2 · Yes, allow all edits', '3 · No, tell Claude what to do']);
    expect(buttons('.choices button')[0].classList.contains('is-selected')).toBe(true);
    expect(buttons('.choices button')[1].classList.contains('is-selected')).toBe(false);
    tap('2 · Yes, allow all edits');
    await flush();
    const [sent] = inputs();
    expect(sent.method).toBe('POST');
    expect(sent.path).toBe(`${BASE}/input`);
    expect(sent.body.key).toBe('2');
    expect(sent.body.enter).toBeFalsy();
    expect(sent.body.command_id).toMatch(/^[0-9a-f-]{36}$/);
  });

  test('a yes/no prompt gives y and n buttons that send the letter then Enter', async () => {
    routes[`GET ${BASE}/screen`] = () => ({ kind: 'ok', data: { text: 'Overwrite? [y/N]', accepts_input: true, state: 'waiting' }, etag: '"yn"' });
    mount();
    await flush();
    expect(buttons('.choices button').map((b) => b.textContent)).toEqual(['y · Oui', 'n · Non']);
    tap('n · Non');
    await flush();
    expect(inputs()[0].body).toMatchObject({ key: 'n', enter: true });
  });

  test('the keys send their name, each with a fresh command id', async () => {
    mount();
    await flush();
    for (const [label, key] of [['Entrée', 'enter'], ['Échap', 'escape'], ['Tab', 'tab'], ['Flèche haut', 'up'], ['Flèche bas', 'down'], ['y', 'y'], ['n', 'n'], ['7', '7']]) {
      tap(label);
      await flush();
      expect(inputs().at(-1)?.body.key).toBe(key);
    }
    expect(new Set(inputs().map((c) => c.body.command_id)).size).toBe(inputs().length);
  });

  test('Ctrl+C needs a second tap within three seconds', async () => {
    mount();
    await flush();
    tap('Interrompre (⌃C)');
    await flush();
    expect(inputs()).toHaveLength(0);
    expect(root.querySelector('.key--armed')).not.toBeNull();
    // Trois secondes plus tard, la confirmation tombe : un nouvel appui ne part pas non plus.
    expect(timers.fire(3000)).toBe(true);
    expect(root.querySelector('.key--armed')).toBeNull();
    tap('Interrompre (⌃C)');
    await flush();
    expect(inputs()).toHaveLength(0);
    // Deux appuis rapprochés : la touche part, une fois.
    tap('Confirmer ⌃C');
    await flush();
    expect(inputs().map((c) => c.body.key)).toEqual(['ctrl_c']);
    expect(root.querySelector('.key--armed')).toBeNull();
  });

  test('the text goes with Enter, the field is emptied on success and kept on a refusal', async () => {
    mount();
    await flush();
    const field = /** @type {HTMLInputElement} */ (root.querySelector('.reply__input'));
    field.value = 'applique le patch';
    tap('Envoyer');
    await flush();
    expect(inputs()[0].body).toMatchObject({ text: 'applique le patch', enter: true });
    expect(field.value).toBe('');
    expect(root.querySelector('.bar__status')?.textContent).toBe('Envoyé');

    routes[`POST ${BASE}/input`] = () => ({ kind: 'http', status: 409, error: 'read_only' });
    field.value = 'encore';
    tap('Envoyer');
    await flush();
    expect(field.value).toBe('encore');
    expect(root.querySelector('.bar__status')?.textContent).toContain('lecture seule');
  });

  test('a text the Mac would refuse is stopped here, in French, without a request', async () => {
    mount();
    await flush();
    const field = /** @type {HTMLInputElement} */ (root.querySelector('.reply__input'));
    field.value = 'deux\tmots';
    tap('Envoyer');
    await flush();
    expect(inputs()).toHaveLength(0);
    expect(root.querySelector('.bar__status')?.textContent).toContain('caractère de contrôle');
    field.value = 'é'.repeat(600);
    tap('Envoyer');
    await flush();
    expect(inputs()).toHaveLength(0);
    expect(root.querySelector('.bar__status')?.textContent).toContain('1 024');
    field.value = '   ';
    tap('Envoyer');
    await flush();
    expect(inputs()).toHaveLength(0);
  });

  test('a network error says the sending is uncertain, reads the screen again and replays nothing', async () => {
    mount();
    await flush();
    const before = calls.filter((c) => c.path.includes('/screen')).length;
    routes[`POST ${BASE}/input`] = () => ({ kind: 'timeout' });
    tap('1 · Yes');
    await flush();
    expect(root.querySelector('.bar__status')?.textContent).toBe("Envoi incertain : vérifiez l'écran");
    expect(inputs()).toHaveLength(1);
    expect(calls.filter((c) => c.path.includes('/screen')).length).toBe(before + 1);
  });

  test('a duplicate is a success', async () => {
    routes[`POST ${BASE}/input`] = () => ({ kind: 'ok', data: { outcome: 'duplicate' } });
    mount();
    await flush();
    tap('1 · Yes');
    await flush();
    expect(root.querySelector('.bar__status')?.textContent).toBe('Envoyé');
  });

  test('a send reads the screen again at once', async () => {
    mount();
    await flush();
    const before = calls.filter((c) => c.path.includes('/screen')).length;
    tap('Entrée');
    await flush();
    expect(calls.filter((c) => c.path.includes('/screen')).length).toBe(before + 1);
  });

  test('at the shell prompt the bar says there is nothing to answer and offers no key', async () => {
    routes[`GET ${BASE}/screen`] = () => ({ kind: 'ok', data: { text: '$ ', accepts_input: false, state: 'idle' }, etag: '"sh"' });
    mount();
    await flush();
    expect(root.querySelector('.bar__idle')?.textContent).toBe('Au prompt du shell : rien à répondre');
    expect(/** @type {HTMLElement} */ (root.querySelector('.bar__controls')).hidden).toBe(true);
    expect(root.querySelector('.bar__idle')?.hasAttribute('hidden')).toBe(false);
  });

  test('a 409 no_program on a send says so and keeps the typed text', async () => {
    routes[`POST ${BASE}/input`] = () => ({ kind: 'http', status: 409, error: 'no_program' });
    mount();
    await flush();
    const field = /** @type {HTMLInputElement} */ (root.querySelector('.reply__input'));
    field.value = 'bonjour';
    tap('Envoyer');
    await flush();
    expect(root.querySelector('.bar__status')?.textContent).toBe('Au prompt du shell : rien à répondre');
    expect(field.value).toBe('bonjour');
  });

  test('an unchanged screen is asked with its ETag and keeps its nodes on a 304', async () => {
    mount();
    await flush();
    const text = root.querySelector('.term__text');
    expect(timers.fire(5000)).toBe(true);
    routes[`GET ${BASE}/screen`] = () => ({ kind: 'http', status: 304, error: 'unknown' });
    await flush();
    const second = calls.filter((c) => c.path.includes('/screen'))[1];
    expect(second.headers).toEqual({ 'If-None-Match': '"v1"' });
    expect(root.querySelector('.term__text')).toBe(text);
  });

  test('the next read comes in 1.5 s while the pane runs, 5 s otherwise', async () => {
    routes[`GET ${BASE}/screen`] = () => ({ kind: 'ok', data: { text: 'x', accepts_input: true, state: 'running' }, etag: '"r"' });
    mount();
    await flush();
    expect(timers.pending.map((t) => t.ms)).toContain(1500);
    expect(timers.pending.map((t) => t.ms)).not.toContain(5000);
    routes[`GET ${BASE}/screen`] = () => ({ kind: 'ok', data: { text: 'x', accepts_input: true, state: 'idle' }, etag: '"i"' });
    timers.fire(1500);
    await flush();
    expect(timers.pending.map((t) => t.ms)).toContain(5000);
  });

  test('a state change received by the stream reads the screen at once', async () => {
    const { store } = mount();
    await flush();
    const before = calls.filter((c) => c.path.includes('/screen')).length;
    store.setFleet(fleet([session('idema-aaaa1111', 'running', { panes: [{ id: 0, active: true, state: 'idle' }, { id: 1, active: false, state: 'running' }] })]), NOW + 1);
    await flush();
    expect(calls.filter((c) => c.path.includes('/screen')).length).toBe(before + 1);
    // Une flotte où ce pane ne change pas d'état ne relit rien.
    store.setFleet(fleet([session('idema-aaaa1111', 'running', { panes: [{ id: 0, active: true, state: 'waiting' }, { id: 1, active: false, state: 'running' }] })]), NOW + 2);
    await flush();
    expect(calls.filter((c) => c.path.includes('/screen')).length).toBe(before + 1);
  });

  test('"Plus d\'historique" reads 2 000 lines, without the ETag of the short read', async () => {
    mount();
    await flush();
    tap("Plus d'historique");
    await flush();
    const last = calls.filter((c) => c.path.includes('/screen')).at(-1);
    expect(last?.path).toBe(`${BASE}/screen?history=2000`);
    expect(last?.headers).toBeUndefined();
    expect(buttons('button').some((b) => b.textContent === "Plus d'historique")).toBe(false);
  });

  test('a closed session says so and goes back to the fleet', async () => {
    routes[`GET ${BASE}/screen`] = () => ({ kind: 'http', status: 404, error: 'not_found' });
    mount();
    await flush();
    expect(root.querySelector('.bar__status')?.textContent).toBe('Session fermée');
    expect(location.hash).toBe('#/s/idema-aaaa1111/1');
    expect(timers.fire(1500)).toBe(true);
    expect(location.hash).toBe('#/');
  });

  test('a 404 on a send does the same', async () => {
    mount();
    await flush();
    routes[`POST ${BASE}/input`] = () => ({ kind: 'http', status: 404, error: 'not_found' });
    tap('Entrée');
    await flush();
    expect(root.querySelector('.bar__status')?.textContent).toBe('Session fermée');
    timers.fire(1500);
    expect(location.hash).toBe('#/');
  });

  test('the pane selector shows only with several panes and opens the other one', async () => {
    mount();
    await flush();
    const segments = buttons('.seg button');
    expect(segments.map((b) => b.textContent)).toEqual(['Pane 0', 'Pane 1']);
    expect(segments.map((b) => b.getAttribute('aria-pressed'))).toEqual(['false', 'true']);
    /** @type {HTMLElement} */ (segments[0]).click();
    expect(location.hash).toBe('#/s/idema-aaaa1111/0');
  });

  test('a single pane has no selector', async () => {
    mount({ fleet: fleet([session('idema-aaaa1111', 'idle')]) });
    await flush();
    expect(root.querySelector('.seg')?.hasAttribute('hidden') ?? true).toBe(true);
  });

  test('the wrap switch is remembered', async () => {
    const { store } = mount();
    await flush();
    const screen = /** @type {HTMLElement} */ (root.querySelector('.term'));
    expect(screen.classList.contains('term--nowrap')).toBe(false);
    tap('Retour à la ligne');
    expect(screen.classList.contains('term--nowrap')).toBe(true);
    expect(store.getPref('wrap', true)).toBe(false);
    unmount?.();
    mount();
    await flush();
    expect(/** @type {HTMLElement} */ (root.querySelector('.term')).classList.contains('term--nowrap')).toBe(true);
  });

  test('a read that fails keeps the last screen and says it may be old', async () => {
    mount();
    await flush();
    routes[`GET ${BASE}/screen`] = () => ({ kind: 'timeout' });
    timers.fire(5000);
    await flush();
    expect(root.querySelector('.term__text')?.textContent).toContain('Do you want');
    expect(/** @type {HTMLElement} */ (root.querySelector('.session__stale')).hidden).toBe(false);
    routes[`GET ${BASE}/screen`] = () => ({ kind: 'ok', data: { text: 'neuf', accepts_input: true, state: 'idle' }, etag: '"n"' });
    timers.fire(5000);
    await flush();
    expect(/** @type {HTMLElement} */ (root.querySelector('.session__stale')).hidden).toBe(true);
    expect(root.querySelector('.term__text')?.textContent).toBe('neuf');
  });

  /** Tous les contrôles d'envoi : choix, touches, champ et bouton. */
  const controlNodes = () => [...root.querySelectorAll('.bar__controls button, .bar__controls input')].map((node) => /** @type {HTMLButtonElement} */ (node));

  test('the reply controls are disabled after a failed read and come back with the next good one', async () => {
    mount();
    await flush();
    expect(controlNodes().length).toBeGreaterThan(10);
    expect(controlNodes().every((node) => !node.disabled)).toBe(true);
    routes[`GET ${BASE}/screen`] = () => ({ kind: 'offline' });
    timers.fire(5000);
    await flush();
    expect(controlNodes().every((node) => node.disabled)).toBe(true);
    // Un clic sur un bouton désactivé ne part pas non plus.
    calls.length = 0;
    controlNodes()[0].click();
    await flush();
    expect(inputs()).toHaveLength(0);
    routes[`GET ${BASE}/screen`] = () => ({ kind: 'ok', data: { text: 'neuf', accepts_input: true, state: 'idle' }, etag: '"n"' });
    timers.fire(5000);
    await flush();
    expect(controlNodes().every((node) => !node.disabled)).toBe(true);
  });

  test('the reply controls are disabled while the app says the Mac is unreachable, and the choices drawn then are too', async () => {
    const { store } = mount();
    await flush();
    store.setConnection('offline');
    expect(controlNodes().every((node) => node.disabled)).toBe(true);
    // De nouveaux choix dessinés pendant la coupure naissent désactivés.
    routes[`GET ${BASE}/screen`] = () => ({ kind: 'ok', data: { text: MENU_TEXT.replace('what to do', 'what to do\n   4. Other'), accepts_input: true, state: 'waiting' }, etag: '"m"' });
    timers.fire(5000);
    await flush();
    expect(controlNodes().every((node) => node.disabled)).toBe(true);
    store.setConnection('ok');
    expect(controlNodes().every((node) => !node.disabled)).toBe(true);
  });

  test('a 304 read counts as a reachable Mac', async () => {
    mount();
    await flush();
    routes[`GET ${BASE}/screen`] = () => ({ kind: 'timeout' });
    timers.fire(5000);
    await flush();
    expect(controlNodes().every((node) => node.disabled)).toBe(true);
    routes[`GET ${BASE}/screen`] = () => ({ kind: 'http', status: 304, error: 'unknown' });
    timers.fire(5000);
    await flush();
    expect(controlNodes().every((node) => !node.disabled)).toBe(true);
  });

  test('a button being touched is not replaced by a read that changes nothing', async () => {
    mount();
    await flush();
    const button = root.querySelector('.choices button');
    timers.fire(5000);
    await flush();
    expect(root.querySelector('.choices button')).toBe(button);
  });

  test('resuming from the background acknowledges and reads again', async () => {
    mount();
    await flush();
    calls.length = 0;
    dispatchEvent(new Event('idema:resume'));
    await flush();
    expect(calls.some((c) => c.path.endsWith('/ack'))).toBe(true);
    expect(calls.some((c) => c.path.includes('/screen'))).toBe(true);
  });

  test('leaving the screen stops the reads and the listeners', async () => {
    mount();
    await flush();
    unmount?.();
    unmount = undefined;
    calls.length = 0;
    expect(timers.pending).toHaveLength(0);
    dispatchEvent(new Event('idema:resume'));
    await flush();
    expect(calls).toHaveLength(0);
  });

  test('triage: "Suivante" opens the next session still waiting, then says everything is handled', async () => {
    const data = fleet([
      session('idema-aaaa1111', 'waiting', { panes: [{ id: 0, active: true, state: 'idle' }, { id: 1, active: false, state: 'waiting' }] }),
      session('idema-bbbb2222', 'error'),
    ]);
    startTriage();
    mount({ fleet: data });
    await flush();
    tap('Suivante');
    expect(location.hash).toBe('#/s/idema-bbbb2222/0');
    unmount?.();
    unmount = undefined;
    // La seconde session traitée à son tour : plus rien.
    location.hash = '#/s/idema-bbbb2222/0';
    routes[`GET /sessions/idema-bbbb2222/panes/0/screen`] = () => ({ kind: 'ok', data: { text: 'boom', accepts_input: true, state: 'error' }, etag: '"e"' });
    routes[`POST /sessions/idema-bbbb2222/panes/0/ack`] = () => ({ kind: 'ok', data: null });
    mount({ fleet: data, route: { name: 'session', session: 'idema-bbbb2222', pane: 0 } });
    await flush();
    const done = buttons('button').find((b) => b.textContent === 'Tout est traité');
    expect(done).toBeDefined();
    /** @type {HTMLElement} */ (done).click();
    expect(location.hash).toBe('#/');
  });

  test('outside the triage there is no "Suivante"', async () => {
    mount();
    await flush();
    expect(buttons('button').some((b) => b.textContent === 'Suivante')).toBe(false);
  });

  test('the back button returns to the fleet', async () => {
    mount();
    await flush();
    tap('Retour à la flotte');
    expect(location.hash).toBe('#/');
  });

  test('every key is on screen at once: the digits on one row, the named keys and y/n on the other', async () => {
    mount();
    await flush();
    const rows = buttons('.keys__row').map((row) => [...row.querySelectorAll('button')].map((b) => b.getAttribute('aria-label')));
    expect(rows[0]).toEqual(['1', '2', '3', '4', '5', '6', '7', '8', '9']);
    expect(rows[1]).toEqual(['Flèche haut', 'Flèche bas', 'Entrée', 'Échap', 'Tab', 'Interrompre (⌃C)', 'y', 'n']);
    expect(rows).toHaveLength(2);
  });

  test('arming Ctrl+C says what to do in the status line and clears it when the confirmation lapses', async () => {
    mount();
    await flush();
    tap('Interrompre (⌃C)');
    expect(root.querySelector('.bar__status')?.textContent).toBe('Touchez encore ⌃C pour confirmer');
    timers.fire(3000);
    expect(root.querySelector('.bar__status')?.textContent).toBe('');
  });

  test('a send that throws does not poison the next ones, and the text field is released', async () => {
    mount();
    await flush();
    const field = /** @type {HTMLInputElement} */ (root.querySelector('.reply__input'));
    const real = crypto.randomUUID;
    crypto.randomUUID = () => {
      throw new Error('no uuid');
    };
    field.value = 'premier';
    tap('Envoyer');
    await flush();
    crypto.randomUUID = real;
    expect(inputs()).toHaveLength(0);
    expect(root.querySelector('.bar__status')?.textContent).toContain("L'envoi a échoué");
    expect(field.value).toBe('premier');
    expect(/** @type {HTMLButtonElement} */ (buttons('.reply .btn')[0]).disabled).toBe(false);
    tap('Envoyer');
    await flush();
    expect(inputs()).toHaveLength(1);
    expect(inputs()[0].body.text).toBe('premier');
    tap('Entrée');
    await flush();
    expect(inputs()).toHaveLength(2);
  });

  test('the field is kept when it changed while the text was on its way', async () => {
    /** @type {(value: any) => void} */
    let answer = () => {};
    routes[`POST ${BASE}/input`] = () => new Promise((resolve) => (answer = resolve));
    mount();
    await flush();
    const field = /** @type {HTMLInputElement} */ (root.querySelector('.reply__input'));
    field.value = 'envoyé';
    tap('Envoyer');
    await flush();
    field.value = 'envoyé puis la suite';
    answer({ kind: 'ok', data: { outcome: 'sent' } });
    await flush();
    expect(field.value).toBe('envoyé puis la suite');
  });

  test('after a send the screen is read at once, then once more about 400 ms later', async () => {
    mount();
    await flush();
    const reads = () => calls.filter((c) => c.path.includes('/screen')).length;
    const before = reads();
    tap('Entrée');
    await flush();
    expect(reads()).toBe(before + 1);
    expect(timers.fire(400)).toBe(true);
    await flush();
    expect(reads()).toBe(before + 2);
    expect(timers.pending.some((t) => t.ms === 400)).toBe(false);
  });

  test('a late 404 from the acknowledgement after leaving the screen sends nobody back to the fleet', async () => {
    /** @type {(value: any) => void} */
    let answer = () => {};
    routes[`POST ${BASE}/ack`] = () => new Promise((resolve) => (answer = resolve));
    mount();
    await flush();
    unmount?.();
    unmount = undefined;
    location.hash = '#/absence';
    answer({ kind: 'http', status: 404, error: 'not_found' });
    await flush();
    expect(timers.pending).toHaveLength(0);
    timers.fire();
    expect(location.hash).toBe('#/absence');
  });

  test('while the page is hidden the screen is not read and nothing is acknowledged; back in front, both resume', async () => {
    setVisibility('hidden');
    mount();
    await flush();
    expect(calls.some((c) => c.path.endsWith('/ack'))).toBe(false);
    calls.length = 0;
    expect(timers.fire(5000)).toBe(true);
    await flush();
    expect(calls).toHaveLength(0);
    // La minuterie se réarme : la lecture reprendra seule au retour.
    expect(timers.pending.map((t) => t.ms)).toContain(5000);
    setVisibility('visible');
    expect(timers.fire(5000)).toBe(true);
    await flush();
    expect(calls.some((c) => c.path.includes('/screen'))).toBe(true);
    dispatchEvent(new Event('idema:resume'));
    await flush();
    expect(calls.some((c) => c.path.endsWith('/ack'))).toBe(true);
  });
});
