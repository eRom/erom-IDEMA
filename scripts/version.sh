#!/bin/bash
# Imprime la version d'IdemA, seule source : `version` de `[workspace.package]` dans le Cargo.toml racine.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
version="$(sed -n '/^\[workspace\.package\]/,/^\[/s/^version *= *"\([^"]*\)".*/\1/p' "$ROOT/Cargo.toml")"
if [ -z "$version" ]; then
  echo "version.sh: no version in [workspace.package] of $ROOT/Cargo.toml" >&2
  exit 1
fi
echo "$version"
