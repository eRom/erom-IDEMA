// @ts-check
// Hors de l'écran d'accueil (onglet Safari) : seulement comment ajouter IdemA. Le push n'existe qu'en app installée.

import { el } from './dom.js';
import { icon } from './icons.js';

/** La page tourne-t-elle comme app installée ? `navigator.standalone` est celui d'iOS Safari. */
export function isStandalone() {
  const ios = /** @type {{standalone?: boolean}} */ (navigator).standalone === true;
  return ios || matchMedia('(display-mode: standalone)').matches;
}

/** @param {ReturnType<typeof icon>} glyph @param {string} text */
function step(glyph, text) {
  return el('li', { class: 'step' }, el('span', { class: 'step__icon' }, glyph), el('span', null, text));
}

/** @param {HTMLElement} root */
export function renderInstall(root) {
  root.replaceChildren(
    el(
      'main',
      { class: 'screen' },
      el('img', { class: 'screen__logo', src: '/icons/icon-180.png', alt: '', width: 72, height: 72 }),
      el('h1', null, "Ajoutez IdemA à l'écran d'accueil"),
      el(
        'p',
        { class: 'muted' },
        "Les notifications n'existent que dans l'app installée. Depuis Safari, trois gestes :",
      ),
      el(
        'ol',
        { class: 'steps card' },
        step(icon('share'), 'Touchez Partager, dans la barre de Safari.'),
        step(icon('addToHome'), "Choisissez « Sur l'écran d'accueil », puis Ajouter."),
        step(icon('open'), "Ouvrez IdemA depuis votre écran d'accueil."),
      ),
    ),
  );
}
