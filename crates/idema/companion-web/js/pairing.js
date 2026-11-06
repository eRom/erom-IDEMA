// @ts-check
// L'écran « Appairer avec IdemA » : six chiffres, clavier numérique, `POST /idema-api/pair`.

import { api } from './api.js';
import { el } from './dom.js';
import { bannerText } from './offline.js';

const CODE_LENGTH = 6;

/** Les quatre refus du Mac (403), chacun dit en clair. */
const REFUSALS = {
  wrong_code: "Ce code n'est pas le bon. Vérifiez les 6 chiffres affichés sur le Mac.",
  expired: 'Ce code a expiré. Demandez-en un nouveau dans IdemA, sur le Mac.',
  locked: "Trop d'essais : ce code est verrouillé. Demandez-en un nouveau dans IdemA, sur le Mac.",
  no_code: "Aucun code n'est affiché sur le Mac. Touchez « Appairer un appareil » dans IdemA, puis réessayez.",
};

/**
 * Ce que dit l'écran d'un résultat de `POST pair` qui n'a pas réussi.
 * @param {import('./api.js').ApiResult} result
 * @param {number} [nowMs]
 */
export function pairingMessage(result, nowMs = Date.now()) {
  if (result.kind === 'offline' || result.kind === 'timeout') return bannerText(result.kind, null, nowMs);
  if (result.kind === 'http' && Object.hasOwn(REFUSALS, result.error)) {
    return REFUSALS[/** @type {keyof typeof REFUSALS} */ (result.error)];
  }
  return 'IdemA a répondu par une erreur. Réessayez dans un instant.';
}

/**
 * @param {HTMLElement} root
 * @param {{onPaired: () => void}} options
 */
export function renderPairing(root, { onPaired }) {
  const cells = Array.from({ length: CODE_LENGTH }, () => el('span', { class: 'code__cell' }));
  const input = el('input', {
    class: 'code__input',
    type: 'text',
    inputmode: 'numeric',
    pattern: '[0-9]*',
    autocomplete: 'one-time-code',
    maxlength: CODE_LENGTH,
    'aria-label': `Code d'appairage à ${CODE_LENGTH} chiffres`,
  });
  const message = el('p', { class: 'error', role: 'alert' });
  const submit = el('button', { class: 'btn btn--primary', type: 'button', disabled: true }, 'Appairer');
  let busy = false;

  function paint() {
    cells.forEach((cell, index) => {
      cell.textContent = input.value[index] ?? '';
      cell.classList.toggle('is-current', index === Math.min(input.value.length, CODE_LENGTH - 1));
    });
    submit.disabled = busy || input.value.length !== CODE_LENGTH;
  }

  async function send() {
    if (busy || input.value.length !== CODE_LENGTH) return;
    busy = true;
    message.textContent = '';
    input.disabled = true;
    paint();
    const result = await api('POST', '/pair', { code: input.value });
    busy = false;
    if (result.kind === 'ok') {
      onPaired();
      return;
    }
    message.textContent = pairingMessage(result);
    input.disabled = false;
    input.value = '';
    paint();
    input.focus();
  }

  input.addEventListener('input', () => {
    input.value = input.value.replace(/\D/g, '').slice(0, CODE_LENGTH);
    paint();
    if (input.value.length === CODE_LENGTH) void send();
  });
  submit.addEventListener('click', () => void send());

  root.replaceChildren(
    el(
      'main',
      { class: 'screen' },
      el('img', { class: 'screen__logo', src: '/icons/icon-180.png', alt: '', width: 72, height: 72 }),
      el('h1', null, 'Appairer avec IdemA'),
      el(
        'p',
        { class: 'muted' },
        'Sur le Mac, ouvrez Personnaliser, section Compagnon, puis touchez « Appairer un appareil ». Saisissez ici le code à 6 chiffres.',
      ),
      el('div', { class: 'code' }, cells, input),
      message,
      submit,
    ),
  );
  paint();
}
