//! Le score flou d'un chemin (P2 jalon 3, filtre du volet Fichiers) : une requête est une sous-séquence du chemin, et
//! l'alignement de meilleur score est retenu. Aucune dépendance à GPUI : tout se teste sans fenêtre.

/// Un chemin qui correspond : son score, et les caractères trouvés.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FuzzyMatch {
    pub score: i32,
    /// Les décalages en octets des caractères trouvés dans le candidat, croissants.
    pub positions: Vec<usize>,
}

/// Score d'un caractère trouvé.
const MATCH_SCORE: i32 = 16;
/// Bonus d'un caractère qui commence un mot.
const WORD_START_BONUS: i32 = 12;
/// Bonus d'un caractère qui suit immédiatement le caractère trouvé précédent.
const CONSECUTIVE_BONUS: i32 = 10;
/// Bonus d'un caractère du dernier segment du chemin (le nom du fichier).
const FILE_NAME_BONUS: i32 = 8;
/// Score d'une case inatteignable de la table.
const UNREACHABLE: i32 = i32::MIN / 2;

/// Le caractère replié pour la comparaison : son premier caractère en minuscule.
fn fold(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// Faut-il considérer que `current` commence un mot, après `previous` ?
fn starts_word(previous: Option<char>, current: char) -> bool {
    match previous {
        None => true,
        Some(previous) => {
            matches!(previous, '/' | '_' | '-' | '.')
                || previous.is_whitespace()
                || (previous.is_lowercase() && current.is_uppercase())
        }
    }
}

/// Cherche l'alignement de meilleur score de `query` dans `candidate`.
///
/// Les blancs de la requête sont ignorés, la casse aussi. Rend `None` pour une requête vide ou qui n'est pas une
/// sous-séquence du candidat. Programmation dynamique en O(requête x candidat) : `table[i][j]` est le meilleur score
/// avec le `i`-ième caractère de la requête posé sur le `j`-ième du candidat. La pénalité de saut (-1 par caractère
/// entre deux trouvés) est additive d'un caractère trouvé au suivant : un maximum glissant de `table[i-1][k] + k` sur
/// les `k` antérieurs la porte en temps constant par case.
pub fn fuzzy_match(query: &str, candidate: &str) -> Option<FuzzyMatch> {
    let needle: Vec<char> = query
        .chars()
        .filter(|c| !c.is_whitespace())
        .map(fold)
        .collect();
    if needle.is_empty() {
        return None;
    }
    let chars: Vec<(usize, char)> = candidate.char_indices().collect();
    let n = chars.len();
    if needle.len() > n {
        return None;
    }
    let folded: Vec<char> = chars.iter().map(|&(_, c)| fold(c)).collect();
    // Le dernier segment : les caractères après le dernier « / ».
    let file_name_start = chars
        .iter()
        .rposition(|&(_, c)| c == '/')
        .map_or(0, |slash| slash + 1);

    // Le score propre à un caractère du candidat, indépendant de ses voisins trouvés.
    let own_score = |j: usize| {
        let previous = j.checked_sub(1).map(|p| chars[p].1);
        let mut score = MATCH_SCORE;
        if starts_word(previous, chars[j].1) {
            score += WORD_START_BONUS;
        }
        if j >= file_name_start {
            score += FILE_NAME_BONUS;
        }
        score
    };

    let m = needle.len();
    let mut table = vec![vec![UNREACHABLE; n]; m];
    // `parent[i][j]` : où le caractère `i - 1` de la requête est posé, quand `(i, j)` est atteint.
    let mut parent = vec![vec![usize::MAX; n]; m];

    for (j, &c) in folded.iter().enumerate() {
        if c == needle[0] {
            table[0][j] = own_score(j);
        }
    }
    for i in 1..m {
        // Meilleur `table[i-1][k] + k` sur les `k < j`, et son `k`.
        let mut best_prior = (UNREACHABLE, usize::MAX);
        for j in i..n {
            let k = j - 1;
            if table[i - 1][k] > UNREACHABLE && table[i - 1][k] + k as i32 > best_prior.0 {
                best_prior = (table[i - 1][k] + k as i32, k);
            }
            if folded[j] != needle[i] || best_prior.1 == usize::MAX {
                continue;
            }
            // Saut : -(j - k - 1) caractères sautés, soit `best_prior - j + 1`.
            let mut best = best_prior.0 - j as i32 + 1;
            let mut from = best_prior.1;
            // Voisin immédiat : le bonus de consécutivité s'ajoute, sans saut.
            if table[i - 1][k] > UNREACHABLE && table[i - 1][k] + CONSECUTIVE_BONUS >= best {
                best = table[i - 1][k] + CONSECUTIVE_BONUS;
                from = k;
            }
            table[i][j] = best + own_score(j);
            parent[i][j] = from;
        }
    }

    // Le meilleur point d'arrivée ; à égalité, le plus à gauche.
    let (mut end, mut score) = (usize::MAX, UNREACHABLE);
    for (j, &value) in table[m - 1].iter().enumerate() {
        if value > score {
            (end, score) = (j, value);
        }
    }
    if end == usize::MAX {
        return None;
    }

    let mut positions = vec![0; m];
    let mut j = end;
    for i in (0..m).rev() {
        positions[i] = chars[j].0;
        if i > 0 {
            j = parent[i][j];
        }
    }
    Some(FuzzyMatch { score, positions })
}

/// Les candidats qui correspondent à `query` (leur indice dans `candidates`), du meilleur au moins bon : score
/// décroissant, puis le chemin le plus court en caractères, puis l'ordre lexicographique.
pub fn rank(query: &str, candidates: &[&str]) -> Vec<(usize, FuzzyMatch)> {
    let mut matches: Vec<(usize, FuzzyMatch)> = candidates
        .iter()
        .enumerate()
        .filter_map(|(index, candidate)| fuzzy_match(query, candidate).map(|m| (index, m)))
        .collect();
    matches.sort_by(|(a, left), (b, right)| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| {
                candidates[*a]
                    .chars()
                    .count()
                    .cmp(&candidates[*b].chars().count())
            })
            .then_with(|| candidates[*a].cmp(candidates[*b]))
    });
    matches
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Les indices de `candidates` rendus par `rank`, dans l'ordre.
    fn ranked_indices(query: &str, candidates: &[&str]) -> Vec<usize> {
        rank(query, candidates)
            .into_iter()
            .map(|(index, _)| index)
            .collect()
    }

    #[test]
    fn a_query_matches_only_as_an_ordered_subsequence() {
        assert!(fuzzy_match("pnl", "files/panel.rs").is_some());
        assert_eq!(fuzzy_match("lnp", "files/panel.rs"), None);
        assert_eq!(fuzzy_match("", "a.md"), None);
        assert_eq!(fuzzy_match("  ", "a.md"), None);
    }

    #[test]
    fn case_and_spaces_in_the_query_are_ignored() {
        assert!(fuzzy_match("PANEL", "files/panel.rs").is_some());
        assert!(fuzzy_match("panel rs", "files/panel.rs").is_some());
    }

    #[test]
    fn positions_point_at_the_matched_characters() {
        let panel = fuzzy_match("pnl", "panel.rs").expect("pnl matches panel.rs");
        assert_eq!(panel.positions, vec![0, 2, 4]);
        // « é » pèse deux octets : « f » est à 2, « é » à 3.
        let cafe = fuzzy_match("fé", "café.md").expect("fé matches café.md");
        assert_eq!(cafe.positions, vec![2, 3]);
    }

    #[test]
    fn a_match_in_the_file_name_beats_one_in_a_folder() {
        let order = ranked_indices("panel", &["panel/old/notes.md", "src/files/panel.rs"]);
        assert_eq!(order.first(), Some(&1));
    }

    #[test]
    fn consecutive_letters_beat_scattered_ones() {
        let order = ranked_indices("spec", &["scripts/plan-e-cut.md", "docs/spec.md"]);
        assert_eq!(order.first(), Some(&1));
    }

    #[test]
    fn a_match_at_the_start_of_a_word_scores_higher() {
        let order = ranked_indices("pr", &["src/sprout.rs", "src/project.rs"]);
        assert_eq!(order.first(), Some(&1));
    }

    #[test]
    fn equal_scores_prefer_the_shorter_path_then_the_alphabet() {
        let order = ranked_indices("a", &["a.txt", "a.md", "b/a.md"]);
        assert_eq!(order, vec![1, 0, 2]);
    }

    #[test]
    fn the_best_alignment_wins_over_the_leftmost_one() {
        // « f » de « foo/ » puis « b » coûterait 6 sauts et perdrait le début de mot : l'alignement du nom gagne.
        let found = fuzzy_match("fb", "foo/foobar").expect("fb matches foo/foobar");
        assert_eq!(found.positions, vec![4, 7]);
    }

    #[test]
    fn a_query_longer_than_the_candidate_never_matches() {
        assert_eq!(fuzzy_match("panels", "panel"), None);
    }
}
