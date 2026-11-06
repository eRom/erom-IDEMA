// @ts-check
// L'onglet Notes : le scratchpad du Mac tel quel, et une ligne à lui ajouter.

import { el, linkify } from './dom.js';
import { ageFr } from './time.js';

/**
 * Ce que le Mac accepte : 1 à 2 000 octets, sans caractère de contrôle (la tabulation en est un). Refusé ici, en français.
 * @param {string} raw
 * @returns {{ok: true, line: string} | {ok: false, error: string}}
 */
export function validateLine(raw) {
  const line = raw.trim();
  if (line === '') return { ok: false, error: '' };
  if (/[\u0000-\u001f\u007f-\u009f]/.test(line)) return { ok: false, error: 'La ligne contient une tabulation ou un caractère de contrôle : retirez-le puis réessayez.' };
  const bytes = new TextEncoder().encode(line).length;
  return bytes > 2000 ? { ok: false, error: `Ligne trop longue : ${bytes} octets sur 2 000 au plus.` } : { ok: true, line };
}

/** @type {import('./router.js').View} */
export function mountNotes(root, { store, request }) {
  let alive = true;
  const text = el('div', { class: 'notes__text' });
  const meta = el('p', { class: 'muted notes__meta' });
  const status = el('p', { class: 'status', role: 'status' });
  const input = el('input', { class: 'notes__input', type: 'text', placeholder: 'Ajouter une ligne', enterkeyhint: 'send', autocomplete: 'off', 'aria-label': 'Nouvelle ligne' });
  const add = el('button', { class: 'btn btn--primary', type: 'button', onclick: () => void send() }, 'Ajouter');
  const form = el('form', { class: 'notes__form', onsubmit: (/** @type {Event} */ e) => (e.preventDefault(), void send()) }, input, add);
  root.replaceChildren(el('section', { class: 'notes' }, el('h1', null, 'Notes'), meta, text, status, form));
  // Pas d'ajout tant que le Mac ne répond pas : la ligne resterait dans le champ.
  const syncButton = () => void (add.disabled = store.state.connection !== 'ok');

  async function load() {
    const reply = await request('GET', '/scratchpad');
    if (!alive) return;
    if (reply.kind !== 'ok') return void (meta.textContent = 'Le Mac ne répond pas : la note affichée peut être ancienne.');
    const { text: note = '', modified_at: at } = reply.data;
    meta.textContent = note === '' ? 'La note est vide.' : at ? `Modifiée il y a ${ageFr(at, Date.now())}` : '';
    text.replaceChildren(...linkify(note));
    text.scrollTop = text.scrollHeight;
  }

  async function send() {
    if (add.disabled) return;
    const checked = validateLine(input.value);
    status.textContent = checked.ok ? '' : checked.error;
    if (!checked.ok) return;
    add.disabled = true;
    const reply = await request('POST', '/scratchpad/append', { line: checked.line });
    if (!alive) return;
    syncButton();
    if (reply.kind !== 'ok') return void (status.textContent = `${reply.kind === 'http' ? 'Le Mac a refusé la ligne' : 'Mac injoignable'} : elle n'a pas été ajoutée.`);
    input.value = '';
    await load();
  }

  syncButton();
  const unsubscribe = store.subscribe(syncButton);
  void load();
  addEventListener('idema:resume', load);
  return () => ((alive = false), unsubscribe(), removeEventListener('idema:resume', load));
}
