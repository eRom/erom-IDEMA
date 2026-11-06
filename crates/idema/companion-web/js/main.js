// @ts-check
// Point d'entrée : la coque (shell.js) sur la page.

import { mountSettings } from './notify.js';
import { browserEnv, registerWorker } from './push.js';
import { registerView } from './router.js';
import { createShell } from './shell.js';
import { store } from './store.js';

registerView('settings', mountSettings);
void registerWorker(browserEnv());

void createShell({ root: /** @type {HTMLElement} */ (document.getElementById('app')), store }).boot();
