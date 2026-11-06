// @ts-check
// Le routeur par fragment (`#/`), et le registre des écrans que les autres tâches viennent remplir.

/**
 * @typedef {{name: 'fleet'} | {name: 'absence'} | {name: 'notes'} | {name: 'settings'} | {name: 'session', session: string, pane: number}} Route
 */

/**
 * Ce que reçoit un écran à son montage. `request` passe par `api` et mène à l'appairage sur un 401.
 * @typedef {object} ViewContext
 * @property {Route} route
 * @property {typeof import('./store.js').store} store
 * @property {import('./push.js').Request} request
 * @property {ReturnType<typeof import('./push.js').createPushController>} push les notifications de l'appareil
 */

/** @typedef {(root: HTMLElement, context: ViewContext) => (void | (() => void))} View */

/** @type {Map<Route['name'], View>} */
const views = new Map();

/**
 * Un écran se déclare ici (`fleet`, `absence`, `notes`, `settings`, `session`) ; il rend sa fonction de démontage au besoin.
 * @param {Route['name']} name
 * @param {View} view
 */
export function registerView(name, view) {
  views.set(name, view);
}

/** @param {Route['name']} name */
export function viewFor(name) {
  return views.get(name);
}

/**
 * Le fragment d'une session : `#/s/<nom tmux encodé>/<pane>`.
 * @param {string} session
 * @param {number} pane
 */
export function sessionHash(session, pane) {
  return `#/s/${encodeURIComponent(session)}/${pane}`;
}

/**
 * La route d'un fragment. Tout ce qui est illisible retombe sur la flotte.
 * @param {string} hash
 * @returns {Route}
 */
export function parseRoute(hash) {
  const parts = hash.replace(/^#\/?/, '').split('/');
  if (parts.length === 1 && parts[0] === 'absence') return { name: 'absence' };
  if (parts.length === 1 && parts[0] === 'notes') return { name: 'notes' };
  if (parts.length === 1 && parts[0] === 'settings') return { name: 'settings' };
  if (parts.length === 3 && parts[0] === 's' && parts[1] !== '' && /^\d+$/.test(parts[2])) {
    try {
      return { name: 'session', session: decodeURIComponent(parts[1]), pane: Number(parts[2]) };
    } catch {
      return { name: 'fleet' };
    }
  }
  return { name: 'fleet' };
}

/**
 * Appelle `onRoute` pour la route d'aujourd'hui puis à chaque changement de fragment qui change de route (`#` et
 * `#/` sont la même : pas de second rendu). Rend l'arrêt.
 * @param {(route: Route) => void} onRoute
 */
export function startRouter(onRoute) {
  let last = '';
  const run = () => {
    const route = parseRoute(location.hash);
    const key = JSON.stringify(route);
    if (key === last) return;
    last = key;
    onRoute(route);
  };
  addEventListener('hashchange', run);
  run();
  return () => removeEventListener('hashchange', run);
}
