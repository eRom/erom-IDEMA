// @ts-check
// Point d'entrée : la coque (shell.js) sur la page.

import { mountAbsence } from './absence.js';
import { mountFleet } from './fleet.js';
import { mountNotes } from './notes.js';
import { mountSettings } from './notify.js';
import { browserEnv, registerWorker } from './push.js';
import { registerView } from './router.js';
import { createShell } from './shell.js';
import { store } from './store.js';

registerView('fleet', mountFleet);
registerView('absence', mountAbsence);
registerView('notes', mountNotes);
registerView('settings', mountSettings);
void registerWorker(browserEnv());

void createShell({ root: /** @type {HTMLElement} */ (document.getElementById('app')), store }).boot();
