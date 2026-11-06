// @ts-check
// Les cartes de notification : « Activer » après l'appairage, et le petit écran Réglages (activation et test).

import { el } from './dom.js';
import { enableMessage } from './push.js';

/** @typedef {ReturnType<typeof import('./push.js').createPushController>} PushController */

/** La ligne du dernier résultat, en vert ou en rouge selon l'issue. @param {import('./push.js').PushState} state */
function messageLine(state) {
  return el('p', { class: state.message?.ok ? 'status status--ok' : 'status', role: 'status' }, state.message?.text ?? '');
}

/** @param {PushController} push @param {string} label */
function activateButton(push, label) {
  return el(
    'button',
    { class: 'btn btn--primary', type: 'button', disabled: push.state.busy, onclick: () => void push.enable() },
    label,
  );
}

/**
 * La carte sous la barre du haut tant que l'appareil n'est pas abonné. Rend son démontage.
 * @param {HTMLElement} container
 * @param {PushController} push
 */
export function mountActivationCard(container, push) {
  function render() {
    const { state } = push;
    container.hidden = !state.supported || state.subscribed;
    if (container.hidden) return;
    container.replaceChildren(
      el(
        'section',
        { class: 'panel card' },
        el('h2', null, 'Notifications'),
        el('p', { class: 'muted' }, "Soyez prévenu quand une session attend ou passe en erreur, même app fermée."),
        // Refusée : plus de bouton (iOS ne redemande pas) ; la phrase du refus vient du résultat, ou ici à l'ouverture.
        state.permission === 'denied'
          ? state.message
            ? null
            : el('p', { class: 'muted' }, enableMessage('denied'))
          : activateButton(push, 'Activer les notifications'),
        messageLine(state),
      ),
    );
  }
  render();
  return push.subscribe(render);
}

/** @param {import('./push.js').PushState} state */
function stateText({ supported, permission, subscribed }) {
  if (!supported) return "Indisponibles ici : ouvrez IdemA depuis l'écran d'accueil (iOS 16.4 ou plus).";
  if (permission === 'denied') return "Refusées dans les réglages de l'iPhone.";
  return subscribed ? 'Activées sur cet appareil.' : 'Désactivées sur cet appareil.';
}

/** L'écran Réglages (`#/settings`) : état, activation, test, et la limite de la pastille. @type {import('./router.js').View} */
export function mountSettings(root, { push }) {
  function render() {
    const { state } = push;
    const canActivate = state.supported && !state.subscribed && state.permission !== 'denied';
    root.replaceChildren(
      el(
        'section',
        { class: 'screen' },
        el('h1', null, 'Réglages'),
        el(
          'div',
          { class: 'panel card' },
          el('h2', null, 'Notifications'),
          el('p', { class: 'muted' }, stateText(state)),
          canActivate ? activateButton(push, 'Activer les notifications') : null,
          el(
            'button',
            { class: 'btn', type: 'button', disabled: state.busy || !state.supported, onclick: () => void push.test() },
            'Envoyer une notification de test',
          ),
          messageLine(state),
        ),
        el(
          'p',
          { class: 'muted' },
          "La pastille de l'icône peut rester en retard tant que l'app n'a pas été ouverte : iOS ne permet pas de la mettre à jour en silence.",
        ),
      ),
    );
  }
  render();
  void push.refresh();
  return push.subscribe(render);
}
