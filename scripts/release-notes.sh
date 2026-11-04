#!/bin/bash
# release-notes.sh <tag> : vérifie que le tag (v0.2.0, ou v0.2.0-rc.1 pour une pré-version) porte la version de
# Cargo.toml, puis imprime la section `## [<version>]` de CHANGELOG.md. Sort en erreur, sans rien imprimer sur la
# sortie standard, si le tag ne correspond pas ou si la section manque : la release s'arrête avant le build.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
tag="${1:?usage: release-notes.sh <tag>}"
VERSION="$(scripts/version.sh)"
case "$tag" in
  v*) ;;
  *)
    echo "release-notes.sh: tag '$tag' must start with v" >&2
    exit 1
    ;;
esac
base="${tag#v}"
base="${base%%-*}"
if [ "$base" != "$VERSION" ]; then
  echo "release-notes.sh: tag $tag does not match the Cargo.toml version $VERSION" >&2
  exit 1
fi
notes="$(awk -v heading="## [$VERSION]" '
  /^## \[/ { if (found) exit; if (index($0, heading) == 1) { found = 1; next } }
  found { print }
' CHANGELOG.md)"
if [ -z "$(printf '%s' "$notes" | tr -d '[:space:]')" ]; then
  echo "release-notes.sh: CHANGELOG.md has no section ## [$VERSION]" >&2
  exit 1
fi
printf '%s\n' "$notes"
