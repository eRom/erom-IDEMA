// @ts-check
// Toutes les requêtes de la page passent ici : préfixe, JSON, délai de 4 s, et un résultat typé au lieu d'exceptions.

/**
 * @typedef {{kind: 'ok', data: any}
 *   | {kind: 'unpaired'}
 *   | {kind: 'offline'}
 *   | {kind: 'timeout'}
 *   | {kind: 'http', status: number, error: string}} ApiResult
 */

export const API_PREFIX = '/idema-api';
export const TIMEOUT_MS = 4000;

/**
 * La cause d'un échec réseau : un abandon est un délai dépassé, tout le reste (un `TypeError` de `fetch`) un échec
 * immédiat.
 * @param {unknown} error
 * @param {boolean} aborted le délai a-t-il déclenché l'abandon ?
 * @returns {'offline' | 'timeout'}
 */
export function classifyFailure(error, aborted) {
  const abandoned = error instanceof DOMException && error.name === 'AbortError';
  return aborted || abandoned ? 'timeout' : 'offline';
}

/**
 * @param {string} method
 * @param {string} path chemin sous `/idema-api`, avec son `/` initial
 * @param {unknown} [body] sérialisé en JSON s'il est défini (`null` compris)
 * @param {number} [timeoutMs]
 * @param {boolean} [keepalive] la requête survit à la fermeture de la page (le `null` de présence au masquage)
 * @returns {Promise<ApiResult>}
 */
export async function api(method, path, body, timeoutMs = TIMEOUT_MS, keepalive = false) {
  const controller = new AbortController();
  let aborted = false;
  const timer = setTimeout(() => {
    aborted = true;
    controller.abort();
  }, timeoutMs);
  /** @type {RequestInit} */
  const init = { method, signal: controller.signal, credentials: 'same-origin', cache: 'no-store' };
  if (keepalive) init.keepalive = true;
  if (body !== undefined) {
    init.headers = { 'Content-Type': 'application/json' };
    init.body = JSON.stringify(body);
  }
  try {
    const response = await fetch(API_PREFIX + path, init);
    const text = await response.text();
    if (response.status === 401) return { kind: 'unpaired' };
    if (response.ok) {
      if (text === '') return { kind: 'ok', data: null };
      try {
        return { kind: 'ok', data: JSON.parse(text) };
      } catch {
        return { kind: 'http', status: response.status, error: 'bad_response' };
      }
    }
    return { kind: 'http', status: response.status, error: errorCode(text) };
  } catch (error) {
    return { kind: classifyFailure(error, aborted) };
  } finally {
    clearTimeout(timer);
  }
}

/** Le code d'un corps d'erreur `{"ok":false,"error":"<code>"}`, `unknown` s'il n'y en a pas. @param {string} text */
function errorCode(text) {
  try {
    const { error } = JSON.parse(text);
    return typeof error === 'string' ? error : 'unknown';
  } catch {
    return 'unknown';
  }
}
