//! Intégration zsh (spec §3.5) : Idema lance tmux avec `ZDOTDIR` pointé sur son propre dossier. Son `.zshenv`
//! rend la main aux fichiers de l'utilisateur, puis branche les marqueurs OSC 133. Rien n'est écrit chez lui.
//! Écrit pour Idema, sur le principe de Ghostty (dont le script zsh, tiré de Kitty, est sous GPLv3).

use std::io;
use std::path::Path;

/// `.zshenv` d'Idema. Un zsh de login ouvert dans un terminal part sans `ZDOTDIR` : on fait de même, et le
/// `.zshenv` de l'utilisateur le pose s'il le veut. Un `ZDOTDIR` hérité d'Idema (lancé depuis un shell)
/// ferait sauter `~/.zshenv`, et avec lui le `PATH` et les clés que l'utilisateur y exporte.
pub const ZSHENV: &str = r#"# Idema : lu seulement parce qu'Idema lance tmux avec ZDOTDIR pointé ici. Réécrit à chaque lancement.
'builtin' 'typeset' _idema_dir="${ZDOTDIR}"
'builtin' 'unset' 'ZDOTDIR'
{
    'builtin' 'typeset' _idema_file="${HOME}/.zshenv"
    [[ ! -r "$_idema_file" ]] || 'builtin' 'source' '--' "$_idema_file"
} always {
    if [[ -o 'interactive' && -r "${_idema_dir}/idema-integration.zsh" ]]; then
        'builtin' 'source' '--' "${_idema_dir}/idema-integration.zsh"
    fi
    'builtin' 'unset' '_idema_file' '_idema_dir'
}
"#;

/// Marqueurs OSC 133 : A au prompt (depuis `precmd`, avant que `PS1` soit développé et dessiné), B quand ZLE
/// prend la main sur la ligne (crochet `zle-line-init`, jamais par le `PS1` de l'utilisateur), C au lancement
/// d'une commande, D;<code> à sa fin. A seul ne dit pas que le tty est prêt à recevoir une saisie : un prompt
/// lent (git à l'expansion) laisse le noyau faire l'écho avant ZLE ; B est le signe que ZLE a la ligne.
/// Les crochets s'installent au premier prompt, après le `.zshrc` de l'utilisateur.
pub const INTEGRATION: &str = r#"# Idema : marqueurs OSC 133. Réécrit à chaque lancement.
_idema_precmd() {
    'builtin' 'local' -i _idema_status=$?
    'builtin' 'emulate' -L zsh -o no_aliases
    # Un greffon qui rappelle precmd depuis zle (rafraîchissement du prompt) ne clôt pas de commande.
    if ! 'builtin' 'zle' && (( ${+_idema_running} )); then
        'builtin' 'printf' '\e]133;D;%s\a' "$_idema_status"
        'builtin' 'unset' '_idema_running'
    fi
    'builtin' 'printf' '\e]133;A\a'
}
_idema_preexec() {
    'builtin' 'emulate' -L zsh -o no_aliases
    'builtin' 'typeset' -g _idema_running=1
    'builtin' 'printf' '\e]133;C\a'
}
_idema_line_init() {
    'builtin' 'printf' '\e]133;B\a'
}
_idema_install() {
    'builtin' 'emulate' -L zsh -o no_aliases
    # Sans ZLE (shell non interactif ou module absent), ou sans add-zle-hook-widget (zsh < 5.3) : pas de B.
    if [[ -o 'zle' ]] && 'builtin' 'autoload' '-Uz' 'add-zle-hook-widget' 2>/dev/null; then
        add-zle-hook-widget 'line-init' _idema_line_init 2>/dev/null
    fi
    precmd_functions=(${precmd_functions:#_idema_install} _idema_precmd)
    preexec_functions+=(_idema_preexec)
    _idema_precmd
}
'builtin' 'typeset' -ga precmd_functions preexec_functions
precmd_functions+=(_idema_install)
"#;

/// Écrit `.zshenv` et `idema-integration.zsh` dans `dir` (créé au besoin), qui devient le `ZDOTDIR` de tmux.
pub fn write_zsh_integration(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(dir.join(".zshenv"), ZSHENV)?;
    std::fs::write(dir.join("idema-integration.zsh"), INTEGRATION)
}
