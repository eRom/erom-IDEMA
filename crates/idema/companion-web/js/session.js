// @ts-check
// L'écran d'une session : le pane tel que le Mac le capture, et la barre pour lui répondre.

import { parseSgr, renderScreen } from './ansi.js';
import { detectChoices } from './choices.js';
import { el } from './dom.js';
import { icon } from './icons.js';
import { sessionHash } from './router.js';
import { realTimers } from './time.js';
import { handledSessions, isTriage, markHandled, triageHash } from './triage.js';

const FAST_MS = 1500;
const SLOW_MS = 5000;
/** Délai pour confirmer ⌃C, durée du « Envoyé », attente avant de quitter une session fermée. */
const CONFIRM_MS = 3000;
const NOTICE_MS = 2000;
const CLOSED_MS = 1500;
const HISTORY = 200;
const HISTORY_MAX = 2000;
/** Ce que le Mac accepte d'un texte (§3.5) : 1 à 1 024 octets, aucun caractère de contrôle. */
const TEXT_MAX_BYTES = 1024;
/** À moins de cette distance du bas, l'écran reste collé en bas. */
const STICK_PX = 24;

/** @typedef {{text: string, tone: 'ok' | 'warn' | 'error', closed: boolean}} Outcome */

/**
 * Ce qu'on dit de la réponse du Mac à un envoi.
 * @param {import('./api.js').ApiResult} result
 * @returns {Outcome}
 */
export function outcomeMessage(result) {
  /** @param {string} text @param {Outcome['tone']} [tone] @returns {Outcome} */
  const say = (text, tone = 'error') => ({ text, tone, closed: false });
  if (result.kind === 'ok') {
    return result.data?.outcome === 'sent_without_enter' ? say("Texte envoyé, mais Entrée n'est pas partie : vérifiez l'écran", 'warn') : say('Envoyé', 'ok');
  }
  if (result.kind === 'unpaired') return say('');
  if (result.kind !== 'http') return say("Envoi incertain : vérifiez l'écran");
  if (result.status === 404) return { text: 'Session fermée', tone: 'error', closed: true };
  switch (result.error) {
    case 'no_program':
      return say('Au prompt du shell : rien à répondre');
    case 'read_only':
      return say("Ce pane est en lecture seule : rien n'a été envoyé");
    case 'disconnected':
      return say("La session est déconnectée du Mac : rien n'a été envoyé");
    case 'expired':
      return say("Le Mac a tardé : l'envoi a peut-être échoué, vérifiez l'écran");
    default:
      return say(result.status === 400 ? 'Le Mac a refusé ce texte (caractère de contrôle ou plus de 1 024 octets)' : "Le Mac a refusé l'envoi");
  }
}

/** @param {string} raw @returns {{ok: true, text: string} | {ok: false, error: string}} */
function validateText(raw) {
  const text = raw.trim();
  if (text === '') return { ok: false, error: '' };
  if (/[\u0000-\u001f\u007f-\u009f]/.test(text)) return { ok: false, error: 'Le texte contient une tabulation ou un caractère de contrôle : retirez-le puis réessayez.' };
  const bytes = new TextEncoder().encode(text).length;
  return bytes > TEXT_MAX_BYTES ? { ok: false, error: `Texte trop long : ${bytes} octets sur 1 024 au plus.` } : { ok: true, text };
}

/** Les touches, ⌃C à part (il demande une confirmation). `y` et `n` ici n'envoient pas Entrée. */
const KEYS = [
  ['↑', 'Flèche haut', 'up'],
  ['↓', 'Flèche bas', 'down'],
  ['Entrée', 'Entrée', 'enter'],
  ['Échap', 'Échap', 'escape'],
  ['Tab', 'Tab', 'tab'],
  ['y', 'y', 'y'],
  ['n', 'n', 'n'],
  ...Array.from({ length: 9 }, (_, i) => [String(i + 1), String(i + 1), String(i + 1)]),
];

const visible = () => document.visibilityState === 'visible';

/** @type {import('./router.js').View} */
export function mountSession(root, { route, store, request, timers = realTimers }) {
  if (route.name !== 'session') return;
  const { session, pane } = route;
  const base = `/sessions/${encodeURIComponent(session)}/panes/${pane}`;
  let alive = true;
  let closing = false;
  let reading = false;
  let again = false;
  let history = HISTORY;
  let etag = /** @type {string | null} */ (null);
  let paneState = 'idle';
  let drawn = '';
  let choicesKey = '';
  let panesKey = '';
  let wrap = store.getPref('wrap', true);
  /** @type {any} */
  let readTimer = null;
  /** @type {any} */
  let noticeTimer = null;
  /** @type {any} */
  let armTimer = null;
  /** @type {any} */
  let closeTimer = null;
  let chain = Promise.resolve(true);
  let sending = false;
  const stateOfPane = () => store.state.fleet?.sessions.find((s) => s.name === session)?.panes.find((/** @type {{id: number}} */ p) => p.id === pane)?.state;
  let fleetState = stateOfPane();

  // --- Les éléments
  const title = el('h1', { class: 'session__title' }, session);
  const dot = el('span', { class: 'dot dot--idle' });
  const wrapButton = el('button', { class: 'iconbtn', type: 'button', 'aria-label': 'Retour à la ligne', onclick: toggleWrap }, icon('wrap'));
  const seg = el('div', { class: 'seg', role: 'group', 'aria-label': 'Panes', hidden: true });
  const stale = el('p', { class: 'session__stale', role: 'status', hidden: true }, "Le Mac ne répond pas : l'écran peut être ancien.");
  const more = el('button', { class: 'term__more', type: 'button', onclick: loadHistory }, "Plus d'historique");
  const screen = el('div', { class: 'term', role: 'region', 'aria-label': 'Écran du pane', tabindex: 0 }, el('p', { class: 'muted' }, "Lecture de l'écran…"));
  const status = el('span', { class: 'bar__status', role: 'status' });
  const next = el('button', { class: 'bar__next', type: 'button', hidden: true, onclick: goNext });
  const choices = el('div', { class: 'choices', hidden: true });
  const ctrl = el('button', { class: 'key', type: 'button', 'aria-label': 'Interrompre (⌃C)', onclick: pressCtrl }, '⌃C');
  const keys = el(
    'div',
    { class: 'keys' },
    KEYS.slice(0, 5).map(([label, name, key]) => keyButton(label, name, key)),
    ctrl,
    KEYS.slice(5).map(([label, name, key]) => keyButton(label, name, key)),
  );
  const field = el('input', { class: 'reply__input', type: 'text', placeholder: 'Répondre…', enterkeyhint: 'send', autocomplete: 'off', autocapitalize: 'off', 'aria-label': 'Réponse' });
  const submit = el('button', { class: 'btn btn--primary', type: 'button', onclick: () => void sendText() }, 'Envoyer');
  const reply = el('form', { class: 'reply', onsubmit: (/** @type {Event} */ e) => (e.preventDefault(), void sendText()) }, field, submit);
  const controls = el('div', { class: 'bar__controls' }, choices, keys, reply);
  const idle = el('p', { class: 'bar__idle', hidden: true }, 'Au prompt du shell : rien à répondre');
  root.replaceChildren(
    el(
      'section',
      { class: 'session' },
      el('header', { class: 'session__head' }, el('button', { class: 'iconbtn', type: 'button', 'aria-label': 'Retour à la flotte', onclick: () => void (location.hash = '#/') }, icon('back')), title, dot, wrapButton),
      seg,
      stale,
      screen,
      el('footer', { class: 'bar' }, el('div', { class: 'bar__meta' }, status, next), controls, idle),
    ),
  );
  setWrap(wrap);

  /** @param {string} label @param {string} name @param {string} key */
  function keyButton(label, name, key) {
    return el('button', { class: 'key', type: 'button', 'aria-label': name, onclick: () => void send({ key }) }, label);
  }

  // --- La barre : état, envois
  /** @param {Outcome} outcome */
  function say({ text, tone }) {
    if (noticeTimer !== null) timers.clearTimeout(noticeTimer);
    noticeTimer = null;
    status.textContent = text;
    status.className = `bar__status bar__status--${tone}`;
    if (tone === 'ok') noticeTimer = timers.setTimeout(() => void (status.textContent = ''), NOTICE_MS);
  }

  function close() {
    if (closing) return;
    closing = true;
    if (readTimer !== null) timers.clearTimeout(readTimer);
    say({ text: 'Session fermée', tone: 'error', closed: true });
    closeTimer = timers.setTimeout(() => void (location.hash = '#/'), CLOSED_MS);
  }

  /** Les envois partent dans l'ordre où on les touche. @param {Record<string, unknown>} body */
  function send(body) {
    chain = chain.then(() => post(body));
    return chain;
  }

  /** @param {Record<string, unknown>} body */
  async function post(body) {
    if (!alive || closing) return false;
    // Un envoi n'est jamais rejoué : un `command_id` neuf à chaque appui.
    const result = await request('POST', `${base}/input`, { ...body, command_id: crypto.randomUUID() });
    if (!alive) return false;
    const outcome = outcomeMessage(result);
    say(outcome);
    if (outcome.closed) {
      close();
      return false;
    }
    void refresh();
    return outcome.tone !== 'error';
  }

  async function sendText() {
    if (sending) return;
    const checked = validateText(field.value);
    if (!checked.ok) return void (checked.error && say({ text: checked.error, tone: 'error', closed: false }));
    sending = true;
    submit.disabled = true;
    const sent = await send({ text: checked.text, enter: true });
    sending = submit.disabled = false;
    if (sent && alive) field.value = '';
  }

  function disarm() {
    if (armTimer !== null) timers.clearTimeout(armTimer);
    armTimer = null;
    ctrl.classList.remove('key--armed');
    ctrl.textContent = '⌃C';
    ctrl.setAttribute('aria-label', 'Interrompre (⌃C)');
  }

  function pressCtrl() {
    if (armTimer !== null) {
      disarm();
      void send({ key: 'ctrl_c' });
      return;
    }
    ctrl.classList.add('key--armed');
    ctrl.textContent = 'Confirmer ⌃C';
    ctrl.setAttribute('aria-label', 'Confirmer ⌃C');
    armTimer = timers.setTimeout(disarm, CONFIRM_MS);
  }

  /** Les boutons de choix ne sont refaits que si les choix changent : un bouton touché ne doit pas être détaché. @param {import('./choices.js').Choices | null} found */
  function drawChoices(found) {
    const key = JSON.stringify(found);
    if (key === choicesKey) return;
    choicesKey = key;
    choices.hidden = !found;
    /** @type {{shown: string, body: Record<string, unknown>, selected: boolean}[]} */
    const options = found?.kind === 'menu' ? found.options.map(({ n, label, selected }) => ({ shown: `${n} · ${label}`, body: { key: String(n) }, selected })) : [];
    if (found?.kind === 'yesno') {
      options.push({ shown: 'y · Oui', body: { key: 'y', enter: true }, selected: false }, { shown: 'n · Non', body: { key: 'n', enter: true }, selected: false });
    }
    choices.replaceChildren(...options.map((o) => el('button', { class: o.selected ? 'choice is-selected' : 'choice', type: 'button', onclick: () => void send(o.body) }, o.shown)));
  }

  // --- L'écran
  /** @param {{text?: string, accepts_input?: boolean, state?: string}} data */
  function draw(data) {
    const text = String(data.text ?? '');
    const signature = JSON.stringify([text, data.accepts_input]);
    if (signature === drawn) return;
    drawn = signature;
    const lines = parseSgr(text);
    const gap = screen.scrollHeight - screen.scrollTop - screen.clientHeight;
    screen.replaceChildren(...(history < HISTORY_MAX ? [more] : []), renderScreen(lines));
    // Collé en bas tant qu'on n'est pas remonté ; sinon la distance au bas est gardée (l'historique s'ajoute au-dessus).
    screen.scrollTop = gap <= STICK_PX ? screen.scrollHeight : screen.scrollHeight - screen.clientHeight - gap;
    controls.hidden = data.accepts_input === false;
    idle.hidden = !controls.hidden;
    drawChoices(controls.hidden ? null : detectChoices(lines.map((line) => line.map((seg) => seg.text).join(''))));
  }

  function schedule() {
    if (!alive || closing) return;
    if (readTimer !== null) timers.clearTimeout(readTimer);
    readTimer = timers.setTimeout(() => {
      readTimer = null;
      if (visible()) void refresh();
      else schedule();
    }, paneState === 'running' ? FAST_MS : SLOW_MS);
  }

  /** Lit l'écran ; un appel qui tombe pendant une lecture en demande une autre juste après. */
  async function refresh() {
    if (!alive || closing) return;
    if (reading) {
      again = true;
      return;
    }
    reading = true;
    try {
      const result = await request('GET', `${base}/screen?history=${history}`, undefined, false, undefined, etag ? { 'If-None-Match': etag } : undefined);
      if (!alive) return;
      if (result.kind === 'ok') {
        stale.hidden = true;
        etag = result.etag ?? null;
        paneState = result.data.state ?? paneState;
        dot.className = `dot dot--${paneState}`;
        draw(result.data);
      } else if (result.kind === 'http' && result.status === 304) {
        stale.hidden = true;
      } else if (result.kind === 'http' && result.status === 404) {
        close();
      } else if (result.kind !== 'unpaired') {
        stale.hidden = false;
      }
    } finally {
      reading = false;
      if (alive && !closing) {
        if (again) {
          again = false;
          void refresh();
        } else {
          schedule();
        }
      }
    }
  }

  function loadHistory() {
    history = HISTORY_MAX;
    etag = null;
    more.remove();
    void refresh();
  }

  /** L'acquittement : ce pane est sous les yeux de Romain. */
  function ack() {
    if (!visible()) return;
    void request('POST', `${base}/ack`).then((result) => result.kind === 'http' && result.status === 404 && close());
  }

  /** @param {boolean} next */
  function setWrap(next) {
    wrap = next;
    screen.classList.toggle('term--nowrap', !wrap);
    wrapButton.setAttribute('aria-pressed', String(wrap));
  }

  function toggleWrap() {
    setWrap(!wrap);
    store.setPref('wrap', wrap);
  }

  // --- Le tri
  const nextHash = () => (store.state.fleet ? triageHash(store.state.fleet, handledSessions()) : null);

  function goNext() {
    location.hash = nextHash() ?? '#/';
  }

  // --- La flotte : titre, panes, « Suivante », et une lecture tout de suite quand l'état de ce pane change
  function onFleet() {
    const found = store.state.fleet?.sessions.find((s) => s.name === session);
    title.textContent = found?.display_name ?? session;
    const panes = found?.panes ?? [];
    const key = JSON.stringify(panes.map((/** @type {{id: number, state: string}} */ p) => [p.id, p.state]));
    if (key !== panesKey) {
      panesKey = key;
      seg.hidden = panes.length < 2;
      seg.replaceChildren(
        ...panes.map((/** @type {{id: number, state: string}} */ p) =>
          el('button', { class: 'seg__item', type: 'button', 'aria-pressed': String(p.id === pane), onclick: () => void (location.hash = sessionHash(session, p.id)) }, el('span', { class: `dot dot--${p.state}` }), `Pane ${p.id}`),
        ),
      );
    }
    next.hidden = !isTriage();
    if (isTriage()) next.textContent = nextHash() ? 'Suivante' : 'Tout est traité';
    const state = stateOfPane();
    if (state !== fleetState) {
      fleetState = state;
      void refresh();
    }
  }

  const onResume = () => {
    ack();
    void refresh();
  };
  addEventListener('idema:resume', onResume);
  const unsubscribe = store.subscribe(onFleet);
  markHandled(session);
  onFleet();
  void refresh();
  ack();

  return () => {
    alive = false;
    for (const timer of [readTimer, noticeTimer, armTimer, closeTimer]) if (timer !== null) timers.clearTimeout(timer);
    unsubscribe();
    removeEventListener('idema:resume', onResume);
  };
}
