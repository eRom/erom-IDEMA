// Injecté au début du document, dans le monde de la page, cadre principal seulement.
// Enveloppe `window.fetch` pour retenir, au premier appel de l'app vers l'API des artifacts, les en-têtes de sa requête
// et son paramètre `org`. La requête de l'app n'est jamais modifiée : on lit, puis on rend la main au `fetch` d'origine.
(() => {
  if (window.__idemaArtifactsWrapped) return;
  window.__idemaArtifactsWrapped = true;
  const original = window.fetch;
  if (typeof original !== 'function') return;
  const MARKER = '/api/frame/artifacts';

  // Un objet `Headers`, une liste de paires ou un objet simple deviennent un objet simple (noms en minuscules pour
  // `Headers`, tels quels sinon).
  const toPlain = (headers) => {
    const plain = {};
    if (!headers) return plain;
    try {
      if (Array.isArray(headers)) {
        for (const [name, value] of headers) plain[name] = String(value);
      } else if (typeof headers.forEach === 'function') {
        headers.forEach((value, name) => { plain[name] = String(value); });
      } else {
        for (const name of Object.keys(headers)) plain[name] = String(headers[name]);
      }
    } catch (_) {}
    return plain;
  };

  window.fetch = function (input, init) {
    try {
      if (!window.__idemaArtifacts) {
        const raw = typeof input === 'string' ? input : (input && input.url) || String(input);
        if (raw.includes(MARKER)) {
          const headers = Object.assign({}, toPlain(input && input.headers), toPlain(init && init.headers));
          const org = new URL(raw, location.href).searchParams.get('org');
          window.__idemaArtifacts = { headers, org };
        }
      }
    } catch (_) {}
    return original.apply(this, arguments);
  };
})();
