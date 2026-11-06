#!/bin/bash
# Porte visuelle de la PWA : capture.sh puis colors.py, non-zéro si l'un des deux échoue.
# À lancer depuis un dossier de travail hors de tout dépôt git (voir capture.sh).
# Usage : check.sh <dossier de sortie> [préfixe, « t16- » par défaut]
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
OUT="${1:?usage: check.sh <output folder> [prefix]}"
PREFIX="${2:-t16-}"

"$HERE/capture.sh" "$OUT" "$PREFIX" || { echo "FAIL : capture.sh a échoué"; exit 1; }
python3 -I "$HERE/colors.py" "$OUT" "$PREFIX" || { echo "FAIL : colors.py a échoué"; exit 1; }
echo "check: PASS"
