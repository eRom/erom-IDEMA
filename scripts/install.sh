#!/usr/bin/env bash
# Installeur d'IdemA : curl -fsSL https://erom.cloud/install_idema.sh | bash
#
# Télécharge le DMG de la dernière release, vérifie son empreinte, copie IdemA.app dans /Applications
# (ou $IDEMA_APP_DIR), puis propose d'installer IdnmA. Relancé, il remplace l'app par la dernière version.
# Le DMG passe par curl, qui ne pose pas l'attribut de quarantaine : pas d'alerte Gatekeeper au premier lancement.
#
# Variables :
#   IDEMA_VERSION   tag à installer (v0.2.1) ; défaut : la dernière release
#   IDEMA_APP_DIR   dossier de destination ; défaut : /Applications
#   IDEMA_RELEASES  base des releases ; défaut : https://github.com/eRom/erom-IDEMA/releases
#   IDEMA_IDNMA     y ou n pour répondre d'avance à la question IdnmA (utile sans terminal)

# Tout vit dans main, appelée à la dernière ligne : un téléchargement coupé n'exécute rien.
set -euo pipefail

readonly MIN_MACOS_MAJOR=26
readonly IDNMA_INSTALLER="https://erom.cloud/install_idnma.sh"

if [ -t 1 ]; then
  BOLD=$'\033[1m' DIM=$'\033[2m' RED=$'\033[31m' GREEN=$'\033[32m' YELLOW=$'\033[33m' RESET=$'\033[0m'
else
  BOLD="" DIM="" RED="" GREEN="" YELLOW="" RESET=""
fi

step() { printf '%s==>%s %s\n' "$BOLD" "$RESET" "$*"; }
note() { printf '    %s%s%s\n' "$DIM" "$*" "$RESET"; }
warn() { printf '%swarning:%s %s\n' "$YELLOW" "$RESET" "$*" >&2; }
die() {
  printf '%serror:%s %s\n' "$RED" "$RESET" "$1" >&2
  shift
  for line in "$@"; do printf '       %s\n' "$line" >&2; done
  exit 1
}

check_platform() {
  [ "$(uname -s)" = "Darwin" ] || die "IdemA is a macOS app."
  # hw.optional.arm64 dit vrai même sous Rosetta, là où uname -m répondrait x86_64.
  [ "$(sysctl -n hw.optional.arm64 2>/dev/null || echo 0)" = "1" ] || die "IdemA needs an Apple Silicon Mac."
  local macos major
  macos=$(sw_vers -productVersion)
  major=${macos%%.*}
  ((10#$major >= MIN_MACOS_MAJOR)) || die "IdemA needs macOS $MIN_MACOS_MAJOR or later (found $macos)."
}

# Dernier tag de release, lu dans la redirection de /releases/latest : pas d'API GitHub, pas de limite de débit.
latest_tag() {
  local url
  url=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "$1/latest" 2>/dev/null) || return 0
  case "$url" in
    */releases/tag/*) printf '%s\n' "${url##*/}" ;;
  esac
}

# Répond y ou n à une question, lue sur le terminal : sous curl | bash, l'entrée standard est le script lui-même.
ask_yes_no() {
  local question=$1 preset=${2:-} answer
  case "$preset" in
    [yY]*) return 0 ;;
    [nN]*) return 1 ;;
  esac
  if ! { exec 3</dev/tty; } 2>/dev/null; then
    note "No terminal to ask on; skipped. Set IDEMA_IDNMA=y to answer in advance."
    return 1
  fi
  printf '%s?%s %s [y/N] ' "$BOLD" "$RESET" "$question"
  read -r answer <&3 || answer=""
  exec 3<&-
  case "$answer" in
    [yY]*) return 0 ;;
    *) return 1 ;;
  esac
}

install_app() {
  local releases=$1 tag=$2 app_dir=$3 work=$4
  local version=${tag#v}
  local dmg="IdemA-$version-arm64.dmg"
  local dest="$app_dir/IdemA.app"

  # Remplacer le bundle d'une app ouverte la ferait planter à la prochaine ressource chargée.
  if pgrep -qf "^$dest/Contents/MacOS/" 2>/dev/null; then
    die "IdemA is running from $dest." "Quit it (Cmd-Q), then run this installer again."
  fi
  [ -d "$app_dir" ] || die "$app_dir does not exist." "Create it, or pick another folder with IDEMA_APP_DIR."
  [ -w "$app_dir" ] || die "$app_dir is not writable by $(id -un)." \
    "Pick a folder you own, for example: curl -fsSL https://erom.cloud/install_idema.sh | IDEMA_APP_DIR=~/Applications bash"

  step "Downloading IdemA $version"
  curl -fL --progress-bar -o "$work/$dmg" "$releases/download/$tag/$dmg" \
    || die "Could not download $dmg." "Check that $tag exists: $releases"
  curl -fsSL -o "$work/$dmg.sha256" "$releases/download/$tag/$dmg.sha256" \
    || die "Could not download $dmg.sha256."
  (cd "$work" && shasum -a 256 -c -s "$dmg.sha256") \
    || die "Checksum mismatch for $dmg: the download is corrupt or was tampered with."

  step "Installing into $dest"
  local mount="$work/mount"
  mkdir "$mount"
  hdiutil attach -quiet -nobrowse -readonly -noautoopen -mountpoint "$mount" "$work/$dmg" \
    || die "Could not mount $dmg."
  # Copie à côté puis échange : une copie coupée ne laisse jamais une app à moitié écrite à la place de l'ancienne.
  local staged="$app_dir/.IdemA.app.installing"
  rm -rf "$staged"
  if ! ditto "$mount/IdemA.app" "$staged"; then
    rm -rf "$staged"
    hdiutil detach -quiet "$mount" || true
    die "Could not copy IdemA.app into $app_dir."
  fi
  hdiutil detach -quiet "$mount" || warn "could not unmount $mount; it disappears at the next reboot."
  if [ -e "$dest" ]; then
    mv "$dest" "$work/IdemA.app.old"
  fi
  mv "$staged" "$dest"
  codesign --verify --strict "$dest" 2>/dev/null || warn "the signature of $dest does not verify."

  local installed
  installed=$(plutil -extract CFBundleShortVersionString raw "$dest/Contents/Info.plist" 2>/dev/null || echo "?")
  printf '\n%sIdemA %s installed%s in %s\n' "$GREEN" "$installed" "$RESET" "$dest"
}

check_runtime_requirements() {
  command -v tmux >/dev/null 2>&1 || warn "tmux is missing: every IdemA terminal runs in it. Install it with: brew install tmux"
  # Police par défaut des terminaux ; IdemA retombe sur une autre police si elle manque, sans icônes.
  if ! compgen -G "$HOME/Library/Fonts/HackNerdFontMono*" >/dev/null && ! compgen -G "/Library/Fonts/HackNerdFontMono*" >/dev/null; then
    note "Tip: the default terminal font is Hack Nerd Font Mono: brew install --cask font-hack-nerd-font"
  fi
}

offer_idnma() {
  local preset=$1 question="Install IdnmA, the agent launcher used by IdemA menus?"
  command -v idnma >/dev/null 2>&1 && question="Update IdnmA ($(idnma --version 2>/dev/null || echo installed))?"
  printf '\n'
  if ask_yes_no "$question" "$preset"; then
    printf '\n'
    # Installeur d'IdnmA en sous-processus : son échec n'efface pas l'installation d'IdemA, déjà faite.
    curl -fsSL "$IDNMA_INSTALLER" | bash || warn "IdnmA did not install; IdemA works without it. Retry with: curl -fsSL $IDNMA_INSTALLER | bash"
  else
    note "Run it later with: curl -fsSL $IDNMA_INSTALLER | bash"
  fi
}

main() {
  local releases=${IDEMA_RELEASES:-https://github.com/eRom/erom-IDEMA/releases}
  local app_dir=${IDEMA_APP_DIR:-/Applications}
  app_dir=${app_dir%/}
  check_platform

  local tag=${IDEMA_VERSION:-}
  if [ -z "$tag" ]; then
    tag=$(latest_tag "$releases")
    [ -n "$tag" ] || die "Could not find the latest release of IdemA." "Check your network, or set IDEMA_VERSION."
  fi
  case "$tag" in v*) ;; *) tag="v$tag" ;; esac

  local work
  work=$(mktemp -d)
  # Le dossier de travail (DMG, point de montage, ancienne app) disparaît à la sortie, succès ou échec.
  # shellcheck disable=SC2064 # $work est figé ici volontairement
  trap "hdiutil detach -quiet '$work/mount' 2>/dev/null || true; rm -rf '$work'" EXIT

  install_app "$releases" "$tag" "$app_dir" "$work"
  check_runtime_requirements
  offer_idnma "${IDEMA_IDNMA:-}"

  printf '\nNext steps:\n'
  printf '  1. Launch it:          open -a "%s/IdemA.app"\n' "$app_dir"
  printf '  2. Shell command:      menu IdemA > Installer la commande idema et l'"'"'action du Finder\n'
  printf '\nDocs: https://github.com/eRom/erom-IDEMA\n'
}

main "$@"
