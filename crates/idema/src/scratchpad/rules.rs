//! Les règles pures du Scratchpad : où vit la note, ce que son menu contextuel permet, où le curseur retombe après
//! un rechargement. Rien ici ne touche le disque ni une fenêtre.

use crate::files::gestures::{EditorGesture, MenuLine, editor_menu};
use std::path::{Path, PathBuf};

/// Le nom de la note dans le dossier de configuration.
pub const FILE_NAME: &str = "scratchpad.md";

/// Le chemin de la note : un fichier unique du dossier de configuration (il suit `IDEMA_HOME`). Il n'est créé qu'à la
/// première écriture, jamais à l'ouverture du volet.
pub fn scratchpad_path(config_dir: &Path) -> PathBuf {
    config_dir.join(FILE_NAME)
}

/// Le menu contextuel du Scratchpad : celui de l'éditeur de Fichiers, mêmes lignes, libellés et raccourcis, sauf
/// « Copier vers l'agent ». Ici ce geste copie la sélection (il n'y a pas de fichier à référencer) : il lui faut donc
/// une sélection en plus d'une session.
pub fn scratchpad_menu(has_selection: bool, agent_available: bool) -> Vec<MenuLine<EditorGesture>> {
    let mut lines = editor_menu(has_selection, agent_available);
    for line in &mut lines {
        if let MenuLine::Item(item) = line
            && item.gesture == EditorGesture::CopyToAgent
        {
            item.enabled = has_selection && agent_available;
        }
    }
    lines
}

/// Ramène un offset d'octets dans le texte rechargé : au plus `text.len()`, puis reculé jusqu'à une frontière de
/// caractère (un offset au milieu d'un caractère UTF-8 ferait paniquer l'éditeur).
pub fn clamp_offset(offset: usize, text: &str) -> usize {
    let mut offset = offset.min(text.len());
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files::gestures::{EditorGesture, GestureItem, MenuLine, editor_menu};
    use std::path::Path;

    fn items(menu: &[MenuLine<EditorGesture>]) -> Vec<&GestureItem<EditorGesture>> {
        menu.iter()
            .filter_map(|line| match line {
                MenuLine::Item(item) => Some(item),
                MenuLine::Separator => None,
            })
            .collect()
    }

    fn enabled(menu: &[MenuLine<EditorGesture>], gesture: EditorGesture) -> bool {
        items(menu)
            .into_iter()
            .find(|item| item.gesture == gesture)
            .unwrap()
            .enabled
    }

    #[test]
    fn the_note_lives_in_the_config_dir() {
        assert_eq!(
            scratchpad_path(Path::new("/tmp/h")),
            Path::new("/tmp/h/scratchpad.md")
        );
    }

    #[test]
    fn copy_to_agent_needs_a_selection_and_a_session() {
        for has_selection in [false, true] {
            for agent_available in [false, true] {
                let menu = scratchpad_menu(has_selection, agent_available);
                assert_eq!(
                    enabled(&menu, EditorGesture::CopyToAgent),
                    has_selection && agent_available,
                    "selection {has_selection}, agent {agent_available}"
                );
                assert_eq!(enabled(&menu, EditorGesture::Cut), has_selection);
                assert_eq!(enabled(&menu, EditorGesture::Copy), has_selection);
                assert!(enabled(&menu, EditorGesture::Paste));
                assert!(enabled(&menu, EditorGesture::SelectAll));

                // Mêmes lignes, libellés, raccourcis et séparateurs que le menu de l'éditeur de Fichiers.
                let reference = editor_menu(has_selection, agent_available);
                assert_eq!(menu.len(), reference.len());
                for (line, expected) in menu.iter().zip(&reference) {
                    match (line, expected) {
                        (MenuLine::Separator, MenuLine::Separator) => {}
                        (MenuLine::Item(a), MenuLine::Item(b)) => {
                            assert_eq!(a.gesture, b.gesture);
                            assert_eq!(a.label, b.label);
                            assert_eq!(a.shortcut, b.shortcut);
                        }
                        _ => panic!("a menu line changed kind"),
                    }
                }
            }
        }
    }

    #[test]
    fn an_offset_past_the_end_lands_on_the_end() {
        assert_eq!(clamp_offset(50, "abc"), 3);
    }

    #[test]
    fn an_offset_inside_a_character_steps_back() {
        // `é` pèse 2 octets : 2 est une frontière, 1 n'en est pas une.
        assert_eq!(clamp_offset(1, "é!"), 0);
        assert_eq!(clamp_offset(2, "é!"), 2);
    }
}
