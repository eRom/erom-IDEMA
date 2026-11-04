//! Déplacer et dupliquer les lignes de la sélection, comme Zed et VS Code (`⌥↑` `⌥↓`, `⇧⌥↑` `⇧⌥↓`), sans fenêtre :
//! la règle rend l'édition à faire, le volet l'applique à l'éditeur en un seul pas d'annulation.

use std::ops::Range;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Up,
    Down,
}

/// Une édition de l'éditeur : `range` (octets du texte d'avant) devient `text`, puis la sélection devient `selection`
/// (octets du texte d'après).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineEdit {
    pub range: Range<usize>,
    pub text: String,
    pub selection: Range<usize>,
}

/// Les lignes que touche la sélection, sans le saut de ligne final. Une sélection qui finit en tête de ligne n'emporte
/// pas cette ligne-là (trois lignes sélectionnées au triple clic restent trois).
fn line_block(text: &str, selection: &Range<usize>) -> Range<usize> {
    let start = text[..selection.start]
        .rfind('\n')
        .map_or(0, |index| index + 1);
    let last = if selection.end > selection.start && text[..selection.end].ends_with('\n') {
        selection.end - 1
    } else {
        selection.end
    };
    let end = text[last..]
        .find('\n')
        .map_or(text.len(), |index| last + index);
    start..end
}

fn ordered(selection: Range<usize>) -> Range<usize> {
    selection.start.min(selection.end)..selection.start.max(selection.end)
}

fn shifted(selection: &Range<usize>, by: isize) -> Range<usize> {
    selection.start.saturating_add_signed(by)..selection.end.saturating_add_signed(by)
}

/// Échange les lignes de la sélection avec leur voisine ; `None` au bord du texte. La sélection suit ses lignes.
pub fn move_lines(text: &str, selection: Range<usize>, direction: Direction) -> Option<LineEdit> {
    let selection = ordered(selection);
    let block = line_block(text, &selection);
    let lines = &text[block.clone()];
    match direction {
        Direction::Up => {
            if block.start == 0 {
                return None;
            }
            let above = text[..block.start - 1]
                .rfind('\n')
                .map_or(0, |index| index + 1);
            let neighbor = &text[above..block.start - 1];
            Some(LineEdit {
                range: above..block.end,
                text: format!("{lines}\n{neighbor}"),
                selection: shifted(&selection, -(neighbor.len() as isize + 1)),
            })
        }
        Direction::Down => {
            if block.end == text.len() {
                return None;
            }
            let after = block.end + 1;
            let below = text[after..]
                .find('\n')
                .map_or(text.len(), |index| after + index);
            let neighbor = &text[after..below];
            // Une sélection qui emportait le saut de ligne final le perd quand ses lignes deviennent les dernières :
            // elle s'arrête à la fin du texte.
            let moved = shifted(&selection, neighbor.len() as isize + 1);
            let end = moved.end.min(text.len());
            Some(LineEdit {
                range: block.start..below,
                text: format!("{neighbor}\n{lines}"),
                selection: moved.start.min(end)..end,
            })
        }
    }
}

/// Copie les lignes de la sélection juste dessous (`Down` : la sélection passe sur la copie) ou juste dessus (`Up` : la
/// sélection reste en haut, sur la copie).
pub fn duplicate_lines(text: &str, selection: Range<usize>, direction: Direction) -> LineEdit {
    let selection = ordered(selection);
    let block = line_block(text, &selection);
    let lines = &text[block.clone()];
    match direction {
        Direction::Up => LineEdit {
            range: block.start..block.start,
            text: format!("{lines}\n"),
            selection,
        },
        Direction::Down => LineEdit {
            range: block.end..block.end,
            text: format!("\n{lines}"),
            selection: shifted(&selection, lines.len() as isize + 1),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Applique l'édition et rend le texte d'après avec la sélection marquée `[` `]`.
    fn apply(text: &str, edit: &LineEdit) -> String {
        let mut after = text.to_string();
        after.replace_range(edit.range.clone(), &edit.text);
        after.insert(edit.selection.end, ']');
        after.insert(edit.selection.start, '[');
        after
    }

    /// `[` et `]` marquent la sélection dans `marked` : rend le texte nu et la plage.
    fn parse(marked: &str) -> (String, Range<usize>) {
        let start = marked.find('[').unwrap();
        let end = marked.find(']').unwrap() - 1;
        (marked.replace(['[', ']'], ""), start..end)
    }

    fn moved(marked: &str, direction: Direction) -> Option<String> {
        let (text, selection) = parse(marked);
        move_lines(&text, selection, direction).map(|edit| apply(&text, &edit))
    }

    fn duplicated(marked: &str, direction: Direction) -> String {
        let (text, selection) = parse(marked);
        apply(&text, &duplicate_lines(&text, selection, direction))
    }

    #[test]
    fn a_caret_line_moves_up_and_down_with_its_caret() {
        assert_eq!(moved("a\nb[]b\nc", Direction::Up).unwrap(), "b[]b\na\nc");
        assert_eq!(moved("a\nb[]b\nc", Direction::Down).unwrap(), "a\nc\nb[]b");
    }

    #[test]
    fn a_selection_moves_every_line_it_touches() {
        assert_eq!(
            moved("x\na[a\nb]b\ny", Direction::Up).unwrap(),
            "a[a\nb]b\nx\ny"
        );
        assert_eq!(
            moved("x\na[a\nb]b\ny", Direction::Down).unwrap(),
            "x\ny\na[a\nb]b"
        );
    }

    #[test]
    fn a_selection_ending_at_a_line_start_leaves_that_line() {
        assert_eq!(
            moved("[a\nb\n]c\nd", Direction::Down).unwrap(),
            "c\n[a\nb\n]d"
        );
    }

    #[test]
    fn a_selection_moved_to_the_end_stops_at_the_end_of_the_text() {
        assert_eq!(moved("a\n[b\n]", Direction::Down).unwrap(), "a\n\n[b]");
    }

    #[test]
    fn nothing_moves_past_the_edges() {
        assert_eq!(moved("a[]a\nb", Direction::Up), None);
        assert_eq!(moved("a\nb[]b", Direction::Down), None);
        // Un texte fini par un saut de ligne a une dernière ligne vide : la ligne d'avant peut descendre.
        assert_eq!(moved("a[]a\n", Direction::Down).unwrap(), "\na[]a");
    }

    #[test]
    fn duplicating_down_puts_the_selection_on_the_copy() {
        assert_eq!(duplicated("a\nb[]b\nc", Direction::Down), "a\nbb\nb[]b\nc");
        assert_eq!(
            duplicated("a[a\nb]b\nc", Direction::Down),
            "aa\nbb\na[a\nb]b\nc"
        );
    }

    #[test]
    fn duplicating_up_keeps_the_selection_on_the_upper_copy() {
        assert_eq!(duplicated("a\nb[]b\nc", Direction::Up), "a\nb[]b\nbb\nc");
    }

    #[test]
    fn multibyte_lines_keep_their_bytes() {
        assert_eq!(moved("é\nà[]ç", Direction::Up).unwrap(), "à[]ç\né");
    }
}
