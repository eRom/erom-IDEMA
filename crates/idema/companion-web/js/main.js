// @ts-check
// Point d'entrée : la coque (shell.js) sur la page.

import { createShell } from './shell.js';
import { store } from './store.js';

void createShell({ root: /** @type {HTMLElement} */ (document.getElementById('app')), store }).boot();
