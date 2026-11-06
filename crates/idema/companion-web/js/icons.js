// @ts-check
// Les icônes : SVG en ligne, trait de 1,5 px, bouts ronds, dans l'esprit des Material Symbols Rounded de graisse 300.
// Les tracés sont des constantes de l'app : aucune donnée du Mac n'entre ici.

const NAMESPACE = 'http://www.w3.org/2000/svg';

const PATHS = {
  fleet: ['M9 6h11M9 12h11M9 18h11M4.5 6h.01M4.5 12h.01M4.5 18h.01'],
  absence: ['M20 14.5A8 8 0 0 1 9.5 4a8 8 0 1 0 10.5 10.5z'],
  notes: [
    'M6 3.5h8l4 4V20a.5.5 0 0 1-.5.5h-11a.5.5 0 0 1-.5-.5v-16a.5.5 0 0 1 .5-.5z',
    'M14 3.5V8h4M9 13h6M9 16.5h4',
  ],
  settings: ['M12 7a2 2 0 1 1 4 0a2 2 0 1 1-4 0z', 'M6 17a2 2 0 1 1 4 0a2 2 0 1 1-4 0z', 'M4 7h8M16 7h4M4 17h2M10 17h10'],
  share: ['M12 15V4M8 7.5l4-4 4 4', 'M8.5 11H6a1.5 1.5 0 0 0-1.5 1.5v6A1.5 1.5 0 0 0 6 20h12a1.5 1.5 0 0 0 1.5-1.5v-6A1.5 1.5 0 0 0 18 11h-2.5'],
  addToHome: ['M5 4h14a1 1 0 0 1 1 1v14a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1V5a1 1 0 0 1 1-1z', 'M12 8.5v7M8.5 12h7'],
  back: ['M14.5 5.5L8 12l6.5 6.5'],
  wrap: ['M4 7h16M4 12h11.5a3 3 0 0 1 0 6H12', 'M14 15.5L11.5 18l2.5 2.5M4 17h4'],
  open: ['M8 5.5h8a1.5 1.5 0 0 1 1.5 1.5v10A1.5 1.5 0 0 1 16 18.5H8A1.5 1.5 0 0 1 6.5 17V7A1.5 1.5 0 0 1 8 5.5z', 'M11 15.5h2'],
};

/** @param {keyof typeof PATHS} name */
export function icon(name) {
  const svg = document.createElementNS(NAMESPACE, 'svg');
  svg.setAttribute('viewBox', '0 0 24 24');
  svg.setAttribute('aria-hidden', 'true');
  svg.setAttribute('class', 'icon');
  for (const d of PATHS[name]) {
    const path = document.createElementNS(NAMESPACE, 'path');
    path.setAttribute('d', d);
    svg.appendChild(path);
  }
  return svg;
}
