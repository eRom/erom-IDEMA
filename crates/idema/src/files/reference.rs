//! Les références envoyées à l'agent (spec P2 §4.3) : un chemin, avec ou sans lignes, écrit comme le fait
//! `copy-ref.ts --abs`. Aucune dépendance à GPUI : tout se teste sans fenêtre.

use std::ops::Range;
use std::path::Path;

/// Le chemin tel qu'on l'écrit : `~/<reste>` s'il est sous `home` (et n'est pas `home` lui-même), sinon inchangé.
/// Sert aussi à « Copier le chemin ».
pub fn display_path(path: &Path, home: Option<&Path>) -> String {
    // `strip_prefix` compare composant par composant : `/Users/mex` n'est pas sous `/Users/me`.
    let rest = home.and_then(|home| path.strip_prefix(home).ok());
    match rest {
        Some(rest) if !rest.as_os_str().is_empty() => format!("~/{}", rest.display()),
        _ => path.display().to_string(),
    }
}

/// Les lignes (base 1) du début et de la fin d'une sélection, décalages en octets bornés à `text.len()`. Une sélection
/// non vide dont la fin tombe en colonne 0 d'une ligne suivant celle du début s'arrête à la ligne d'avant : la ligne
/// d'arrivée n'est pas touchée (règle de `copy-ref.ts`).
pub fn selection_lines(text: &str, selection: Range<usize>) -> (usize, usize) {
    let bytes = text.as_bytes();
    let start = selection.start.min(bytes.len());
    let end = selection.end.min(bytes.len());
    // Ligne (base 1) d'un décalage : une de plus que le nombre de sauts de ligne avant lui.
    let line_at = |offset: usize| {
        1 + bytes[..offset]
            .iter()
            .filter(|&&byte| byte == b'\n')
            .count()
    };
    let first = line_at(start);
    let mut last = line_at(end);
    let ends_at_line_start = end > start && bytes[end - 1] == b'\n';
    if ends_at_line_start && last > first {
        last -= 1;
    }
    (first, last)
}

/// La référence envoyée à l'agent. `None` : le chemin seul ; une ligne : `<chemin>:L<a>` ; plusieurs :
/// `'<chemin>  #L<a>-L<b>'` (apostrophes, deux espaces). L'espace qui suit est ajoutée à l'envoi, pas ici.
pub fn agent_reference(path: &Path, home: Option<&Path>, lines: Option<(usize, usize)>) -> String {
    let shown = display_path(path, home);
    match lines {
        None => shown,
        Some((first, last)) if first == last => format!("{shown}:L{first}"),
        Some((first, last)) => format!("'{shown}  #L{first}-L{last}'"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn references_follow_copy_ref_abs() {
        let home = Path::new("/Users/me");
        let spec = Path::new("/Users/me/dev/idema/docs/spec.md");
        assert_eq!(
            agent_reference(spec, Some(home), Some((18, 18))),
            "~/dev/idema/docs/spec.md:L18"
        );
        assert_eq!(
            agent_reference(spec, Some(home), Some((18, 20))),
            "'~/dev/idema/docs/spec.md  #L18-L20'"
        );
        assert_eq!(
            agent_reference(spec, Some(home), None),
            "~/dev/idema/docs/spec.md"
        );
        assert_eq!(
            agent_reference(Path::new("/etc/hosts"), Some(home), Some((3, 3))),
            "/etc/hosts:L3"
        );
        assert_eq!(display_path(home, Some(home)), "/Users/me");
        assert_eq!(
            display_path(Path::new("/Users/mex/a"), Some(home)),
            "/Users/mex/a"
        );
        assert_eq!(display_path(spec, None), "/Users/me/dev/idema/docs/spec.md");
    }

    #[test]
    fn selection_lines_count_like_copy_ref() {
        let text = "l1\nl2\nl3\nl4\n";
        let l2 = text.find("l2").unwrap();
        let l3 = text.find("l3").unwrap();
        let l4 = text.find("l4").unwrap();
        assert_eq!(selection_lines(text, l2..l2), (2, 2));
        assert_eq!(selection_lines(text, l2..l3 + 2), (2, 3));
        assert_eq!(selection_lines(text, l2..l4), (2, 3));
        assert_eq!(selection_lines(text, l2..l3), (2, 2));
        assert_eq!(selection_lines(text, l2 + 1..l2 + 2), (2, 2));
        assert_eq!(selection_lines(text, text.len()..text.len()), (5, 5));
        assert_eq!(
            selection_lines(text, text.len() + 9..text.len() + 9),
            (5, 5)
        );
    }
}
