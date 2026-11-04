//! La liste que l'arbre dessine quand un champ est ouvert en place (création ou renommage), sans fenêtre : où le
//! champ s'insère parmi les lignes, et quand il n'a plus de place (spec P2 §4.3).

use std::path::{Path, PathBuf};

use super::ops::NameError;
use super::tree::Row;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditKind {
    NewFile,
    NewFolder,
    Rename,
}

/// L'édition en cours. `anchor` est le dossier qui reçoit (la racine comprise) pour une création, ou l'entrée
/// renommée. `error` est le refus du dernier nom essayé, montré sous le champ.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditSlot {
    pub kind: EditKind,
    pub anchor: PathBuf,
    pub error: Option<NameError>,
}

/// Une ligne de la liste dessinée : une ligne de l'arbre (par indice dans `rows`), le champ, ou le message du refus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListItem {
    Row(usize),
    Field { depth: usize, is_dir: bool },
    Message { depth: usize },
}

/// La liste à dessiner. Sans édition, une `Row` par ligne. Une création met le champ juste après la ligne du dossier
/// qui reçoit, un cran plus profond (en tête, à la profondeur 0, pour la racine) ; un renommage met le champ à la
/// place de la ligne, à sa profondeur. Un refus de nom ajoute une ligne de message sous le champ. `None` quand
/// l'ancre n'a plus de place : ligne disparue, dossier replié ou absent, fichier là où il faut un dossier. L'appelant
/// annule alors l'édition. L'ancre se retrouve par son chemin, jamais par un indice : la liste peut avoir gagné des
/// lignes depuis l'ouverture du champ.
pub fn list_items(rows: &[Row], root: &Path, edit: Option<&EditSlot>) -> Option<Vec<ListItem>> {
    let rows_only = || (0..rows.len()).map(ListItem::Row);
    let Some(edit) = edit else {
        return Some(rows_only().collect());
    };
    let message = |depth: usize| edit.error.map(|_| ListItem::Message { depth });
    let mut items = Vec::with_capacity(rows.len() + 2);
    match edit.kind {
        EditKind::NewFile | EditKind::NewFolder => {
            let is_dir = edit.kind == EditKind::NewFolder;
            let (after, depth) = if edit.anchor == root {
                (None, 0)
            } else {
                let index = rows
                    .iter()
                    .position(|row| row.path == edit.anchor && row.is_dir && row.expanded)?;
                (Some(index), rows[index].depth + 1)
            };
            let field = [Some(ListItem::Field { depth, is_dir }), message(depth)];
            if after.is_none() {
                items.extend(field.into_iter().flatten());
            }
            for index in 0..rows.len() {
                items.push(ListItem::Row(index));
                if after == Some(index) {
                    items.extend(field.into_iter().flatten());
                }
            }
        }
        EditKind::Rename => {
            let index = rows.iter().position(|row| row.path == edit.anchor)?;
            for other in 0..rows.len() {
                if other == index {
                    let depth = rows[index].depth;
                    items.push(ListItem::Field {
                        depth,
                        is_dir: rows[index].is_dir,
                    });
                    items.extend(message(depth));
                } else {
                    items.push(ListItem::Row(other));
                }
            }
        }
    }
    Some(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(path: &str, depth: usize, is_dir: bool, expanded: bool) -> Row {
        Row {
            path: PathBuf::from(path),
            name: path.rsplit('/').next().unwrap().to_owned(),
            depth,
            is_dir,
            expanded,
            ignored: false,
        }
    }

    fn slot(kind: EditKind, anchor: &str) -> EditSlot {
        EditSlot {
            kind,
            anchor: PathBuf::from(anchor),
            error: None,
        }
    }

    /// `notes` (déplié, avec `a.md` et `b.md`), puis `readme.md` à la racine.
    fn rows() -> Vec<Row> {
        vec![
            row("/p/notes", 0, true, true),
            row("/p/notes/a.md", 1, false, false),
            row("/p/notes/b.md", 1, false, false),
            row("/p/readme.md", 0, false, false),
        ]
    }

    #[test]
    fn without_an_edit_every_row_is_listed_once() {
        let items = list_items(&rows(), Path::new("/p"), None).unwrap();
        assert_eq!(
            items,
            [
                ListItem::Row(0),
                ListItem::Row(1),
                ListItem::Row(2),
                ListItem::Row(3)
            ]
        );
    }

    #[test]
    fn the_creation_field_opens_first_in_its_folder() {
        let edit = slot(EditKind::NewFile, "/p/notes");
        let items = list_items(&rows(), Path::new("/p"), Some(&edit)).unwrap();
        assert_eq!(
            items,
            [
                ListItem::Row(0),
                ListItem::Field {
                    depth: 1,
                    is_dir: false
                },
                ListItem::Row(1),
                ListItem::Row(2),
                ListItem::Row(3),
            ]
        );
        // À la racine, le champ passe en tête, à la profondeur 0 ; un dossier à créer est marqué comme tel.
        let edit = slot(EditKind::NewFolder, "/p");
        let items = list_items(&rows(), Path::new("/p"), Some(&edit)).unwrap();
        assert_eq!(
            items[0],
            ListItem::Field {
                depth: 0,
                is_dir: true
            }
        );
        assert_eq!(items.len(), 5);
        assert_eq!(items[1], ListItem::Row(0));
        // Une racine vide a de la place aussi.
        let items = list_items(&[], Path::new("/p"), Some(&edit)).unwrap();
        assert_eq!(
            items,
            [ListItem::Field {
                depth: 0,
                is_dir: true
            }]
        );
    }

    #[test]
    fn the_rename_field_takes_the_rows_place() {
        let edit = slot(EditKind::Rename, "/p/notes/a.md");
        let items = list_items(&rows(), Path::new("/p"), Some(&edit)).unwrap();
        assert_eq!(
            items,
            [
                ListItem::Row(0),
                ListItem::Field {
                    depth: 1,
                    is_dir: false
                },
                ListItem::Row(2),
                ListItem::Row(3),
            ]
        );
        // Renommer un dossier déplié : le champ remplace sa ligne, ses entrées restent dessous.
        let edit = slot(EditKind::Rename, "/p/notes");
        let items = list_items(&rows(), Path::new("/p"), Some(&edit)).unwrap();
        assert_eq!(
            items[0],
            ListItem::Field {
                depth: 0,
                is_dir: true
            }
        );
        assert_eq!(items[1], ListItem::Row(1));
        assert_eq!(items.len(), 4);
    }

    #[test]
    fn a_refused_name_adds_a_message_line_under_the_field() {
        let mut edit = slot(EditKind::Rename, "/p/notes/a.md");
        edit.error = Some(NameError::Taken);
        let items = list_items(&rows(), Path::new("/p"), Some(&edit)).unwrap();
        assert_eq!(
            items[..4],
            [
                ListItem::Row(0),
                ListItem::Field {
                    depth: 1,
                    is_dir: false
                },
                ListItem::Message { depth: 1 },
                ListItem::Row(2),
            ]
        );
        let mut edit = slot(EditKind::NewFile, "/p");
        edit.error = Some(NameError::Invalid);
        let items = list_items(&rows(), Path::new("/p"), Some(&edit)).unwrap();
        assert_eq!(
            items[..3],
            [
                ListItem::Field {
                    depth: 0,
                    is_dir: false
                },
                ListItem::Message { depth: 0 },
                ListItem::Row(0)
            ]
        );
    }

    #[test]
    fn an_edit_whose_anchor_vanished_has_no_place() {
        // La ligne renommée a disparu.
        let edit = slot(EditKind::Rename, "/p/gone.md");
        assert_eq!(list_items(&rows(), Path::new("/p"), Some(&edit)), None);
        // Le dossier qui devait recevoir n'est pas dans l'arbre.
        let edit = slot(EditKind::NewFile, "/p/gone");
        assert_eq!(list_items(&rows(), Path::new("/p"), Some(&edit)), None);
        // Le dossier est replié : ses entrées ne se voient plus, le champ n'a pas de place.
        let mut folded = rows();
        folded[0].expanded = false;
        let edit = slot(EditKind::NewFile, "/p/notes");
        assert_eq!(list_items(&folded, Path::new("/p"), Some(&edit)), None);
        // Un fichier n'accueille pas de création.
        let edit = slot(EditKind::NewFile, "/p/readme.md");
        assert_eq!(list_items(&rows(), Path::new("/p"), Some(&edit)), None);
    }

    #[test]
    fn the_field_survives_a_refresh_that_adds_rows() {
        // Un agent écrit dans le dossier pendant la frappe : les mêmes éditions sur des lignes où un fichier
        // est apparu avant, puis après l'ancre, gardent le champ juste après la ligne du dossier.
        let creation = slot(EditKind::NewFile, "/p/notes");
        let rename = slot(EditKind::Rename, "/p/notes/b.md");
        let before_anchor = vec![
            row("/p/aaa", 0, true, false),
            row("/p/notes", 0, true, true),
            row("/p/notes/a.md", 1, false, false),
            row("/p/notes/b.md", 1, false, false),
        ];
        let after_anchor = vec![
            row("/p/notes", 0, true, true),
            row("/p/notes/a.md", 1, false, false),
            row("/p/notes/agent.md", 1, false, false),
            row("/p/notes/b.md", 1, false, false),
            row("/p/zzz.md", 0, false, false),
        ];
        let field = ListItem::Field {
            depth: 1,
            is_dir: false,
        };
        let items = list_items(&before_anchor, Path::new("/p"), Some(&creation)).unwrap();
        let folder = items.iter().position(|i| *i == ListItem::Row(1)).unwrap();
        assert_eq!(items[folder + 1], field);
        let items = list_items(&after_anchor, Path::new("/p"), Some(&creation)).unwrap();
        assert_eq!(items[..2], [ListItem::Row(0), field]);
        // Le renommage suit sa ligne, quel que soit le nombre de lignes ajoutées autour.
        let items = list_items(&before_anchor, Path::new("/p"), Some(&rename)).unwrap();
        assert_eq!(items[3], field);
        let items = list_items(&after_anchor, Path::new("/p"), Some(&rename)).unwrap();
        assert_eq!(items[3], field);
        assert_eq!(items.len(), after_anchor.len());
    }
}
