// @ts-check
// L'onglet Notes : le scratchpad du Mac, toujours éditable, enregistré tout seul (le moteur est dans notes-sync.js).

import { el } from './dom.js';
import { addDash, indent, insertFence, insertHash, outdent } from './edits.js';
import { createNotesSync } from './notes-sync.js';
import { safeLocalStorage } from './store.js';
import { ageFr, realTimers } from './time.js';

const UNREADABLE = 'Note illisible en UTF-8 : modifiez-la sur le Mac.';
const AGE_REFRESH_MS = 30_000;
/** Les cinq touches de la barre, dans l'ordre ; la dernière colonne est la forme du glyphe (le style optique de app.css). */
const KEYS = /** @type {const} */ ([
  ['#', 'Dièse', insertHash, ''],
  ['-', 'Puce', addDash, ''],
  ['⇤', 'Désindenter', outdent, 'arrow'],
  ['⇥', 'Indenter', indent, 'arrow'],
  ['```', 'Bloc de code', insertFence, 'fence'],
]);

/** @type {import('./router.js').View} */
export function mountNotes(root, { store, request, timers = realTimers }) {
  const status = el('p', { class: 'notes__status', role: 'status' });
  const done = el('button', { class: 'notes__done', type: 'button', 'aria-label': 'Terminer', hidden: true, onclick: finish }, '✓');
  const banner = el('div', { class: 'notes__banner', role: 'status', hidden: true });
  const area = el('textarea', { class: 'notes__text', 'aria-label': 'Note', autocapitalize: 'off', autocorrect: 'off', spellcheck: 'false', autocomplete: 'off', oninput: onInput, onfocus: () => focusChanged(true), onblur: () => focusChanged(false) });
  const bar = el('div', { class: 'notes__bar', hidden: true }, KEYS.map(([label, name, transform, shape]) => el('button', { class: shape ? `notes__key notes__key--${shape}` : 'notes__key', type: 'button', 'aria-label': name, onclick: () => apply(transform) }, label)));
  // La touche ne doit pas voler le focus : sans cela le clavier se ferme au toucher.
  for (const control of [done, ...bar.children]) for (const type of ['pointerdown', 'mousedown']) control.addEventListener(type, (e) => e.preventDefault());
  const section = el('section', { class: 'notes' }, el('div', { class: 'notes__top' }, el('h1', null, 'Notes'), el('div', { class: 'notes__side' }, status, done)), banner, area, bar);
  root.replaceChildren(section);

  /** Le contenu du bandeau affiché (son état et sa raison). */
  let shownBanner = '';
  /** La textarea a le focus (barre et ✓ visibles tant que la note est éditable). */
  let focused = false;
  const sync = createNotesSync({ request, timers, storage: safeLocalStorage(), store, onChange: render });

  function render() {
    const { state, text, savedAt, reason } = sync.snapshot();
    // Le moteur ne change son texte que si rien n'a été tapé depuis le début de la lecture (chargement, reprise, reprise de la note du Mac) : l'écran le suit.
    if (state !== 'loading' && state !== 'locked' && area.value !== text.replace(/\r\n?/g, '\n')) showText(text);
    area.readOnly = state === 'loading' || state === 'locked' || state === 'readonly';
    area.hidden = state === 'locked'; // pas de grande carte vide sous le bandeau
    showBar(focused); // la note a pu passer en lecture seule pendant la saisie
    let words = '';
    let tone = '';
    if (state === 'saved') words = savedAt === null ? 'Enregistré' : `Enregistré · il y a ${ageFr(savedAt, Date.now())}`;
    else if (state === 'saving') words = 'Enregistrement…';
    else if (state === 'offline') [words, tone] = ['● Pas enregistré · Mac injoignable', 'warn'];
    else if (state === 'refused') [words, tone] = [`● Refusé : ${reason}`, 'error'];
    status.textContent = words;
    status.className = tone ? `notes__status notes__status--${tone}` : 'notes__status';
    const note = state === 'locked' ? reason : state === 'readonly' ? (reason ?? UNREADABLE) : '';
    // Le bandeau ne se reconstruit que si son contenu change : le tick de 30 s ne doit pas voler un bouton sous le doigt.
    const content = state === 'conflict' ? 'conflict' : note ? `note:${note}` : '';
    banner.hidden = content === '';
    if (content !== shownBanner) {
      shownBanner = content;
      banner.replaceChildren(
        ...(state === 'conflict'
          ? [
              el('p', null, 'La note a changé sur le Mac'),
              el('div', { class: 'notes__choices' }, el('button', { class: 'btn', type: 'button', onclick: () => void sync.takeMac() }, 'Reprendre la note du Mac'), el('button', { class: 'btn', type: 'button', onclick: () => void sync.overwrite() }, 'Écraser avec mon brouillon')),
            ]
          : note ? [el('p', null, note)] : []),
      );
    }
  }

  /** Pose le texte de la note ; le curseur reste là où il était, ramené dans le nouveau texte. @param {string} text */
  function showText(text) {
    const { selectionStart, selectionEnd } = area;
    area.value = text;
    if (document.activeElement === area) area.setSelectionRange(Math.min(selectionStart, area.value.length), Math.min(selectionEnd, area.value.length));
  }

  function onInput() {
    sync.edit(area.value);
  }

  // Barre et ✓ pendant l'édition ; la barre se colle au clavier par la fenêtre visible.
  const viewport = window.visualViewport;
  function placeBar() {
    const gap = viewport ? Math.max(0, window.innerHeight - viewport.height - viewport.offsetTop) : 0;
    bar.style.setProperty('--kb', `${gap}px`);
    fit();
  }
  /** Pendant la saisie, la note tient dans la fenêtre visible, au-dessus du clavier et de la capsule : sinon la dernière ligne tapée et le ✓ restent cachés. */
  function fit() {
    if (viewport && !bar.hidden) {
      section.classList.add('notes--typing');
      section.style.setProperty('--vv-top', `${viewport.offsetTop}px`);
      section.style.setProperty('--vv-h', `${Math.max(0, viewport.height - bar.offsetHeight - 16)}px`);
    } else {
      section.classList.remove('notes--typing');
      section.style.removeProperty('--vv-top');
      section.style.removeProperty('--vv-h');
    }
  }
  /** @param {boolean} on */
  function showBar(on) {
    on &&= !area.readOnly;
    bar.hidden = done.hidden = !on;
    area.style.paddingBottom = on && !viewport ? `${bar.offsetHeight + 16}px` : ''; // avec la fenêtre visible, la note s'arrête au-dessus de la capsule
    placeBar();
  }
  /** @param {boolean} on */
  function focusChanged(on) {
    focused = on;
    showBar(on);
  }
  viewport?.addEventListener('resize', placeBar);
  viewport?.addEventListener('scroll', placeBar);

  function finish() {
    area.blur();
    void sync.flushNow();
  }

  /** @param {(s: import('./edits.js').EditState) => import('./edits.js').EditState} transform */
  function apply(transform) {
    const { value, selectionStart: start, selectionEnd: end } = area;
    const next = transform({ text: value, start, end });
    if (next.text !== value) {
      // Seule la plage qui change est remplacée : l'annulation d'iOS garde un pas par touche.
      let from = 0;
      while (from < value.length && from < next.text.length && value[from] === next.text[from]) from++;
      let tail = 0;
      while (tail < value.length - from && tail < next.text.length - from && value[value.length - 1 - tail] === next.text[next.text.length - 1 - tail]) tail++;
      const insert = next.text.slice(from, next.text.length - tail);
      area.setSelectionRange(from, value.length - tail);
      let used = false;
      if (typeof document.execCommand === 'function') {
        try {
          used = insert === '' ? document.execCommand('delete') : document.execCommand('insertText', false, insert);
        } catch {
          // Refusé par le navigateur : repli ci-dessous.
        }
      }
      if (!used) {
        area.setRangeText(insert, from, value.length - tail, 'end');
        area.dispatchEvent(new Event('input', { bubbles: true })); // execCommand en émet un, setRangeText non
      }
    }
    area.setSelectionRange(next.start, next.end);
  }

  const resume = () => void sync.load();
  const hide = () => document.visibilityState === 'hidden' && void sync.flushNow(true);
  addEventListener('idema:resume', resume);
  document.addEventListener('visibilitychange', hide);
  const tick = timers.setInterval(render, AGE_REFRESH_MS);
  render();
  void sync.load();
  return () => {
    // Quitter l'onglet : le texte non enregistré part (keepalive) avant que le moteur ne se taise.
    void sync.flushNow(true);
    sync.dispose();
    timers.clearInterval(tick);
    removeEventListener('idema:resume', resume);
    document.removeEventListener('visibilitychange', hide);
    viewport?.removeEventListener('resize', placeBar);
    viewport?.removeEventListener('scroll', placeBar);
  };
}
