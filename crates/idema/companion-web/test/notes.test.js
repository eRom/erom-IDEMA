import { afterEach, beforeEach, describe, expect, test } from 'bun:test';
import { DRAFT_KEY } from '../js/notes-sync.js';
import { mountNotes } from '../js/notes.js';
import { createStore } from '../js/store.js';

const flush = () => new Promise((resolve) => setTimeout(resolve, 0));
/** @param {any} data */
const ok = (data) => ({ kind: /** @type {const} */ ('ok'), data });
/** @param {string} text @param {number | null} [modifiedAt] @param {boolean} [editable] */
const mac = (text, modifiedAt = 5, editable = true) => ok({ text, modified_at: modifiedAt, editable });

function deferred() {
  /** @type {(value: any) => void} */
  let resolve = () => {};
  const promise = new Promise((r) => (resolve = r));
  return { promise, resolve };
}

describe('mountNotes', () => {
  /** @type {HTMLElement} */
  let root;
  /** @type {(() => void)[]} */
  let unmounts;
  beforeEach(() => {
    localStorage.clear();
    root = document.createElement('div');
    document.body.replaceChildren(root);
    unmounts = [];
  });
  afterEach(() => {
    for (const unmount of unmounts) unmount();
    document.body.replaceChildren();
  });

  /** @param {(method: string, path: string, body?: any, keepalive?: boolean) => any} handler */
  function mount(handler) {
    const store = createStore(localStorage);
    /** @type {{method: string, path: string, body: any, keepalive: boolean}[]} */
    const calls = [];
    /** @type {(() => void)[]} */
    const queue = [];
    /** @type {(() => void)[]} */
    const intervals = [];
    const timers = /** @type {any} */ ({
      setTimeout: (/** @type {() => void} */ fn) => queue.push(fn),
      clearTimeout: () => {},
      setInterval: (/** @type {() => void} */ fn) => intervals.push(fn),
      clearInterval: () => {},
    });
    const request = async (/** @type {string} */ method, /** @type {string} */ path, /** @type {any} */ body, /** @type {boolean} */ keepalive = false) => (calls.push({ method, path, body, keepalive }), handler(method, path, body, keepalive));
    const unmount = /** @type {() => void} */ (mountNotes(root, /** @type {any} */ ({ store, request, timers })));
    unmounts.push(unmount);
    /** Laisse tomber le délai de sauvegarde, puis les réponses. */
    const fire = async () => {
      for (const fn of queue.splice(0)) fn();
      await flush();
    };
    /** Un tour de la minuterie d'âge (30 s). */
    const tick = () => intervals.forEach((fn) => fn());
    return { store, calls, unmount, fire, tick };
  }

  const area = () => /** @type {HTMLTextAreaElement} */ (root.querySelector('textarea'));
  const status = () => root.querySelector('.notes__status')?.textContent;
  const keys = () => [...root.querySelectorAll('.notes__key')].map((k) => /** @type {HTMLButtonElement} */ (k));
  const key = (/** @type {string} */ label) => /** @type {HTMLButtonElement} */ (keys().find((k) => k.textContent === label));
  /** @param {string} value */
  function type(value) {
    area().value = value;
    area().dispatchEvent(new Event('input', { bubbles: true }));
  }
  function focus() {
    area().dispatchEvent(new Event('focus'));
  }

  test('the note is a textarea with the Mac text, never parsed as HTML', async () => {
    mount(() => mac('<img src=x onerror=alert(1)>'));
    await flush();
    expect(area().value).toBe('<img src=x onerror=alert(1)>');
    expect(root.querySelector('img')).toBeNull();
  });

  test('the textarea does not zoom or autocorrect', async () => {
    mount(() => mac('x'));
    await flush();
    expect(area().getAttribute('autocapitalize')).toBe('off');
    expect(area().getAttribute('autocorrect')).toBe('off');
    expect(area().getAttribute('spellcheck')).toBe('false');
  });

  test('the status says each state in French', async () => {
    const saved = mount(() => mac('x', Date.now() - 5 * 60_000));
    await flush();
    expect(status()).toBe('Enregistré · il y a 5 min');
    type('xy');
    expect(status()).toBe('Enregistrement…');
    saved.unmount();

    /** @type {any} */
    const put = { kind: 'offline' };
    const offline = mount((method) => (method === 'PUT' ? put : mac('x')));
    await flush();
    type('xy');
    await offline.fire();
    expect(status()).toBe('● Pas enregistré · Mac injoignable');
    expect(root.querySelector('.notes__status')?.className).toContain('warn');
  });

  test('a refused write shows the reason in the error colour', async () => {
    const { fire } = mount((method) => (method === 'PUT' ? { kind: 'http', status: 400, error: 'control_char' } : mac('x')));
    await flush();
    type('x\0');
    await fire();
    expect(status()).toBe('● Refusé : caractère NUL dans la note');
    expect(root.querySelector('.notes__status')?.className).toContain('error');
  });

  test('a Mac that never answered locks the note with its copy; an unreadable note says why', async () => {
    mount(() => ({ kind: 'offline' }));
    await flush();
    expect(root.textContent).toContain("Le Mac ne répond pas : la note s'ouvrira à son retour.");
    expect(area().readOnly).toBe(true);
    root.replaceChildren();
    mount(() => mac('€', 5, false));
    await flush();
    expect(root.textContent).toContain('Note illisible en UTF-8 : modifiez-la sur le Mac.');
    expect(area().readOnly).toBe(true);
  });

  test('the toolbar has the five keys in order and applies them to the selection', async () => {
    const { calls, fire } = mount(() => mac('a\nb'));
    await flush();
    expect(keys().map((k) => k.textContent)).toEqual(['#', '-', '⇤', '⇥', '```']);
    area().setSelectionRange(0, 3);
    key('⇥').click();
    expect(area().value).toBe('  a\n  b');
    expect([area().selectionStart, area().selectionEnd]).toEqual([2, 7]);
    await fire();
    expect(calls.find((c) => c.method === 'PUT')?.body.text).toBe('  a\n  b');
    key('⇤').click();
    expect(area().value).toBe('a\nb');
    area().setSelectionRange(0, 0);
    key('```').click();
    expect(area().value).toBe('```\n\n```\na\nb');
    expect(area().selectionStart).toBe(4);
  });

  test('with execCommand the edit goes through it, once, and the selection is set afterwards', async () => {
    mount(() => mac('ab'));
    await flush();
    /** @type {unknown[][]} */
    const commands = [];
    let inputs = 0;
    area().addEventListener('input', () => inputs++);
    const original = document.execCommand;
    document.execCommand = (/** @type {string} */ command, /** @type {boolean} */ _ui, /** @type {string} */ value) => {
      commands.push([command, value]);
      const insert = command === 'delete' ? '' : value;
      area().setRangeText(insert, area().selectionStart, area().selectionEnd, 'end');
      area().dispatchEvent(new Event('input', { bubbles: true }));
      return true;
    };
    try {
      area().setSelectionRange(1, 1);
      key('#').click();
      expect(area().value).toBe('a#b');
      expect(commands).toEqual([['insertText', '#']]);
      expect(inputs).toBe(1);
      area().setSelectionRange(0, 3);
      type('  a');
      inputs = 0;
      commands.length = 0;
      area().setSelectionRange(2, 2);
      key('⇤').click();
      expect(commands).toEqual([['delete', undefined]]);
      expect(area().value).toBe('a');
      expect(inputs).toBe(1);
    } finally {
      document.execCommand = original;
    }
  });

  test('toolbar keys keep the focus', async () => {
    mount(() => mac('x'));
    await flush();
    for (const type of ['pointerdown', 'mousedown']) {
      const event = new Event(type, { bubbles: true, cancelable: true });
      key('#').dispatchEvent(event);
      expect(event.defaultPrevented).toBe(true);
    }
  });

  test('the toolbar shows only while the textarea has the focus, and there is no check button', async () => {
    mount(() => mac('x'));
    await flush();
    expect(root.querySelector('.notes__bar')?.hasAttribute('hidden')).toBe(true);
    focus();
    expect(root.querySelector('.notes__bar')?.hasAttribute('hidden')).toBe(false);
    area().dispatchEvent(new Event('blur'));
    expect(root.querySelector('.notes__bar')?.hasAttribute('hidden')).toBe(true);
    // Le ✓ du clavier iOS suffit : aucun bouton rond dans la note.
    expect(root.querySelector('.notes__done')).toBeNull();
    expect([...root.querySelectorAll('button')].some((b) => b.textContent === '✓')).toBe(false);
    expect(root.querySelector('.notes__side')?.children.length).toBe(1);
  });

  test('focus freezes the page scroll on the document element, blur and unmount release it', async () => {
    const { unmount } = mount(() => mac('x'));
    await flush();
    const html = document.documentElement;
    expect(html.classList.contains('notes-editing')).toBe(false);
    focus();
    expect(html.classList.contains('notes-editing')).toBe(true);
    area().dispatchEvent(new Event('blur'));
    expect(html.classList.contains('notes-editing')).toBe(false);
    focus();
    expect(html.classList.contains('notes-editing')).toBe(true);
    unmount();
    expect(html.classList.contains('notes-editing')).toBe(false);
  });

  test('the conflict banner offers both choices', async () => {
    let writes = 0;
    const { calls, fire } = mount((method, _path, body) => {
      if (method === 'PUT' && !body.force) return { kind: 'http', status: 409, error: 'note_changed' };
      if (method === 'PUT') return (writes++, ok({ modified_at: 9 }));
      return mac(writes === 0 ? 'ailleurs' : 'x');
    });
    await flush();
    type('mon brouillon');
    await fire();
    expect(root.textContent).toContain('La note a changé sur le Mac');
    const buttons = [...root.querySelectorAll('.notes__banner button')];
    expect(buttons.map((b) => b.textContent)).toEqual(['Reprendre la note du Mac', 'Écraser avec mon brouillon']);
    expect(buttons.some((b) => b.className.includes('primary'))).toBe(false); // aucun choix n'est mis en avant
    /** @type {HTMLElement} */ (buttons[1]).click();
    await flush();
    expect(calls.some((c) => c.method === 'PUT' && c.body.force === true && c.body.text === 'mon brouillon')).toBe(true);
    expect(root.textContent).not.toContain('La note a changé sur le Mac');
  });

  test('taking the Mac note replaces the screen and drops the draft', async () => {
    let typed = false; // le Mac ne porte l'autre note qu'une fois le brouillon tapé
    const { fire } = mount((method) => (method === 'PUT' ? { kind: 'http', status: 409, error: 'note_changed' } : mac(typed ? 'ailleurs' : 'x', typed ? 7 : 5)));
    await flush();
    type('mon brouillon');
    typed = true;
    await fire();
    expect(area().value).toBe('mon brouillon');
    expect(localStorage.getItem(DRAFT_KEY)).not.toBeNull();
    /** @type {HTMLElement} */ (root.querySelectorAll('.notes__banner button')[0]).click();
    await flush();
    expect(area().value).toBe('ailleurs');
    expect(root.textContent).not.toContain('La note a changé sur le Mac');
    expect(localStorage.getItem(DRAFT_KEY)).toBeNull();
  });

  test('losing the focus saves what is pending now, without keepalive', async () => {
    const { calls } = mount(() => mac('x'));
    await flush();
    focus();
    type('xy');
    expect(calls.filter((c) => c.method === 'PUT')).toEqual([]); // le délai de sauvegarde n'est pas écoulé
    area().dispatchEvent(new Event('blur'));
    await flush();
    expect(calls.filter((c) => c.method === 'PUT').map((c) => [c.body.text, c.keepalive])).toEqual([['xy', false]]);
    expect(root.querySelector('.notes__bar')?.hasAttribute('hidden')).toBe(true);
  });

  test('losing the focus with nothing pending sends nothing', async () => {
    const { calls } = mount(() => mac('x'));
    await flush();
    focus();
    area().dispatchEvent(new Event('blur'));
    await flush();
    expect(calls.filter((c) => c.method === 'PUT')).toEqual([]);
  });

  test('hiding the page flushes with keepalive', async () => {
    const { calls } = mount(() => mac('x'));
    await flush();
    type('xy');
    Object.defineProperty(document, 'visibilityState', { value: 'hidden', configurable: true });
    try {
      document.dispatchEvent(new Event('visibilitychange'));
    } finally {
      Reflect.deleteProperty(document, 'visibilityState');
    }
    await flush();
    const put = calls.find((c) => c.method === 'PUT');
    expect(put?.keepalive).toBe(true);
    expect(put?.body.text).toBe('xy');
  });

  test('leaving the tab sends what is typed with keepalive, then stops', async () => {
    const { calls, unmount, fire } = mount(() => mac('x'));
    await flush();
    type('xy');
    unmount();
    await flush();
    expect(calls.filter((c) => c.method === 'PUT').map((c) => [c.body.text, c.keepalive])).toEqual([['xy', true]]);
    const before = calls.length;
    await fire();
    dispatchEvent(new Event('idema:resume'));
    await flush();
    expect(calls.length).toBe(before);
  });

  test('typing during a slow write leaves the text and the caret untouched', async () => {
    const slow = deferred();
    const { fire } = mount((method) => (method === 'PUT' ? slow.promise : mac('x')));
    await flush();
    type('xy');
    await fire(); // le délai tombe : le PUT est en vol
    type('xyz');
    area().setSelectionRange(2, 2);
    slow.resolve(ok({ modified_at: 9 }));
    await flush();
    expect(area().value).toBe('xyz');
    expect(area().selectionStart).toBe(2);
  });

  test('a read that comes back after a keystroke replaces nothing', async () => {
    let reply = /** @type {Promise<any> | any} */ (mac('un'));
    mount((method) => (method === 'PUT' ? ok({ modified_at: 9 }) : reply));
    await flush();
    const slow = deferred();
    reply = slow.promise;
    dispatchEvent(new Event('idema:resume'));
    type('un deux');
    area().setSelectionRange(3, 3);
    slow.resolve(mac('trois', 7));
    await flush();
    expect(area().value).toBe('un deux');
    expect(area().selectionStart).toBe(3);
  });

  test('reading again on resume shows the newer Mac note, even after typing once it was saved; a pending draft is never replaced', async () => {
    let note = 'un';
    const { fire } = mount((method) => (method === 'PUT' ? ok({ modified_at: 9 }) : mac(note)));
    await flush();
    note = 'deux';
    dispatchEvent(new Event('idema:resume'));
    await flush();
    expect(area().value).toBe('deux');
    type('deux et moi');
    await fire(); // enregistré : plus de brouillon
    note = 'trois';
    area().setSelectionRange(2, 2);
    dispatchEvent(new Event('idema:resume'));
    await flush();
    expect(area().value).toBe('trois');
    expect(area().selectionStart).toBeLessThanOrEqual('trois'.length);
    type('trois et moi'); // brouillon en attente
    note = 'quatre';
    dispatchEvent(new Event('idema:resume'));
    await flush();
    expect(area().value).toBe('trois et moi');
  });

  test('the 30 s age tick keeps the banner buttons alive, and the banner is a status region', async () => {
    const { fire, tick } = mount((method) => (method === 'PUT' ? { kind: 'http', status: 409, error: 'note_changed' } : mac('ailleurs', 7)));
    await flush();
    type('mon brouillon');
    await fire();
    const button = root.querySelector('.notes__banner button');
    expect(button).not.toBeNull();
    tick();
    expect(root.querySelector('.notes__banner button')).toBe(button);
    expect(root.querySelector('.notes__banner')?.getAttribute('role')).toBe('status');
  });

  test('the banner text follows a change of reason', async () => {
    const note = { text: 'x', editable: false };
    mount(() => mac(note.text, 5, note.editable));
    await flush();
    expect(root.querySelector('.notes__banner')?.textContent).toBe('Note illisible en UTF-8 : modifiez-la sur le Mac.');
    note.editable = true;
    dispatchEvent(new Event('idema:resume'));
    await flush();
    expect(root.querySelector('.notes__banner')?.hasAttribute('hidden')).toBe(true);
    expect(root.querySelector('.notes__banner')?.textContent).toBe('');
  });

  test('the bar goes away when the note turns read-only while focused', async () => {
    let editable = true;
    mount(() => mac('x', 5, editable));
    await flush();
    focus();
    expect(root.querySelector('.notes__bar')?.hasAttribute('hidden')).toBe(false);
    editable = false;
    dispatchEvent(new Event('idema:resume'));
    await flush();
    expect(area().readOnly).toBe(true);
    expect(root.querySelector('.notes__bar')?.hasAttribute('hidden')).toBe(true);
  });

  test('a locked note shows its banner without an empty editor card, and the editor comes back with the Mac', async () => {
    let up = false;
    const { store } = mount(() => (up ? mac('revenu') : { kind: 'offline' }));
    await flush();
    expect(root.textContent).toContain("Le Mac ne répond pas : la note s'ouvrira à son retour.");
    expect(area().hidden).toBe(true);
    up = true;
    store.setConnection('offline');
    store.setConnection('ok');
    await flush();
    expect(area().hidden).toBe(false);
    expect(area().value).toBe('revenu');
  });

  describe('keyboard geometry (visualViewport)', () => {
    /** @type {EventTarget & {height: number, offsetTop: number}} */
    let viewport;
    const section = () => /** @type {HTMLElement} */ (root.querySelector('.notes'));
    const room = () => parseFloat(section().style.getPropertyValue('--vv-h'));
    beforeEach(() => {
      viewport = Object.assign(new EventTarget(), { height: 400, offsetTop: 0 });
      Object.defineProperty(window, 'visualViewport', { value: viewport, configurable: true });
    });
    afterEach(() => void Reflect.deleteProperty(window, 'visualViewport'));

    test('while the textarea has the focus the editor is sized to the visible viewport, and released on blur', async () => {
      mount(() => mac('x'));
      await flush();
      expect(section().classList.contains('notes--typing')).toBe(false);
      focus();
      expect(section().classList.contains('notes--typing')).toBe(true);
      const first = room();
      expect(first).toBeGreaterThan(0);
      expect(first).toBeLessThanOrEqual(400);
      viewport.height = 300;
      viewport.dispatchEvent(new Event('resize'));
      expect(first - room()).toBe(100);
      viewport.offsetTop = 40;
      viewport.dispatchEvent(new Event('scroll'));
      expect(section().style.getPropertyValue('--vv-top')).toBe('40px');
      area().dispatchEvent(new Event('blur'));
      expect(section().classList.contains('notes--typing')).toBe(false);
      expect(section().style.getPropertyValue('--vv-h')).toBe('');
    });

    test('a resize while not focused leaves the layout alone', async () => {
      mount(() => mac('x'));
      await flush();
      viewport.height = 300;
      viewport.dispatchEvent(new Event('resize'));
      expect(section().classList.contains('notes--typing')).toBe(false);
    });
  });
});
