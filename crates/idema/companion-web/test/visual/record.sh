#!/bin/bash
# Enregistre les réponses réelles du service pour les captures (tâche 16, AC-20) : joue vm-record.sh dans une copie
# jetable de la VM Tart, rapatrie les réponses, les nettoie (nom du Mac, utilisateur et chemins de la VM) et les range
# dans fixtures/. Prérequis : `cargo build --release -p idema` sur l'hôte (la VM joue le binaire de l'hôte).
# Usage : record.sh            (depuis n'importe où ; une seule copie de VM à la fois)
# Option : RECORD_FROM=<dossier t16-fixtures déjà rapatrié> saute la VM et ne fait que le nettoyage.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$HERE/../../../../.." && pwd)"
FIXTURES="$HERE/fixtures"

if [ -n "${RECORD_FROM:-}" ]; then
  RAW="$RECORD_FROM"
else
  "$REPO/.claude/notes/vm-tart/scripts/idema-run-ac.sh" ../../crates/idema/companion-web/test/visual/vm-record.sh
  # La copie jetable la plus récente a rapatrié ses sorties sous tests/acceptance/out/vm/<copie>/out/.
  LATEST="$(ls -dt "$REPO"/tests/acceptance/out/vm/idema-run-*/ | head -1)"
  RAW="${LATEST}out/t16-fixtures"
fi
[ -f "$RAW/fleet.json" ] || { echo "record: $RAW/fleet.json missing" >&2; exit 1; }

mkdir -p "$FIXTURES"
# Le Mac et ses chemins : des noms de démonstration. Les écrans portent l'invite du shell de la VM (utilisateur@machine,
# chemin du dossier de travail).
DEMO_MAC="Mac de démo"
jq --arg mac "$DEMO_MAC" '
  .mac = $mac
  | .projects |= map(.path |= sub("^.*/demo-t16-[0-9]+/"; "/Users/demo/dev/"))
  | .sessions |= map(.project |= sub("^.*/demo-t16-[0-9]+/"; "/Users/demo/dev/"))' "$RAW/fleet.json" > "$FIXTURES/fleet.json"
jq '.device.name = "iPhone de démo"' "$RAW/me.json" > "$FIXTURES/me.json"
for name in timeline scratchpad health; do cp "$RAW/$name.json" "$FIXTURES/$name.json"; done
sed -e 's#admin@Manageds-Virtual-Machine#demo@mac#g' -e 's#/Users/admin/work/erom-tool-idema#/Users/demo/dev#g' "$RAW/screens.json" | jq -S . > "$FIXTURES/screens.json"

# Contrôle : rien de la VM ne subsiste.
if grep -l -E 'Manageds|/Users/admin|Virtual-Machine|demo-t16' "$FIXTURES"/*.json; then
  echo "record: leftover VM identity in the files above" >&2
  exit 1
fi
echo "record: fixtures written to $FIXTURES"
ls -l "$FIXTURES"
