// Modèle d'une capture (tâche 16) : capture.sh remplace __OUT__ (dossier), __NAME__ (écran) et __PREFIX__, puis le passe à
// `playwright-cli run-code --filename`. La capture est prise en pixels de l'appareil (430 x 932 CSS, donc 1290 x 2796) ;
// le résultat est { points, csp, errors } : les points à échantillonner (pixels CSS, lus par colors.py), les violations de
// CSP et les erreurs de la page relevées depuis son chargement (posées par capture.sh dans window.__csp et __errors).
async (page) => {
  await page.waitForTimeout(600); // les polices et les transitions de 150 ms se posent avant la prise
  // Le relevé précède la capture : la prise de playwright tente elle-même d'injecter un <style> (la CSP le refuse et le
  // signale, mesuré : aucune violation avant, une après), ce qui n'est pas une faute de la page.
  const report = JSON.stringify(
    await page.evaluate((name) => {
      const withReport = (points) => ({ points, csp: window.__csp ?? ['hook missing'], errors: window.__errors ?? ['hook missing'] });
      const box = (selector) => {
        const node = document.querySelector(selector);
        if (!node) throw new Error(`point: ${selector} introuvable sur ${name}`);
        return node.getBoundingClientRect();
      };
      /** Le centre du nœud, décalé de (dx, dy) pixels CSS. */
      const centre = (selector, dx = 0, dy = 0) => {
        const r = box(selector);
        return [r.left + r.width / 2 + dx, r.top + r.height / 2 + dy];
      };
      /** Un point à (dx, dy) du coin haut gauche du nœud. */
      const corner = (selector, dx, dy) => {
        const r = box(selector);
        return [r.left + dx, r.top + dy];
      };
      /** Hors de toute carte : la marge de 6 px à gauche, à hauteur du centre du nœud. */
      const margin = (selector) => [6, box(selector).top + box(selector).height / 2];
      const byScreen = {
        fleet: () => ({
          bg: margin('.row'),
          card: corner('.row', 6, box('.row').height / 2),
          bar: corner('.tabs', 6, 6),
          'dot-waiting': centre('.dot--waiting'),
          'dot-running': centre('.dot--running'),
          'dot-error': centre('.dot--error'),
        }),
        session: () => ({
          bar: corner('.session__head', 2, 24),
          card: corner('.choice:not(.is-selected)', 8, 22),
          screen: corner('.term', 3, 40),
          accent: corner('.choice.is-selected', 0.75, 22),
        }),
        absence: () => ({ bg: margin('.row'), card: corner('.row', 6, box('.row').height / 2), bar: corner('.tabs', 6, 6) }),
        notes: () => ({ bg: margin('.notes__text'), card: corner('.notes__text', 6, 60), bar: corner('.tabs', 6, 6) }),
        pairing: () => ({ bg: [4, 400] }),
        install: () => ({ bg: [4, 400], card: corner('.steps', 6, box('.steps').height / 2) }),
        offline: () => ({ bg: margin('.row'), card: corner('.row', 6, box('.row').height / 2), banner: corner('.banner', 6, 6) }),
      };
      return withReport(byScreen[name]());
    }, '__NAME__'),
  );
  await page.screenshot({ path: '__OUT__/__PREFIX____NAME__.png', scale: 'device', animations: 'allow', caret: 'initial' });
  // Les violations nées de la prise elle-même ne comptent pour aucun écran suivant.
  await page.evaluate(() => {
    window.__csp = [];
  });
  return report;
}
