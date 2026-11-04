//! Le prompt envoyé à Gemini (spec B-11 §4.2 et §4.3) : lecture de l'index de la cible par le lanceur Git unique,
//! compression du diff comme Zed, assemblage des sections. Pur : aucun GPUI, le pied du volet
//! Changes l'appelle.

use std::collections::BinaryHeap;
use std::fs;
use std::path::Path;

use crate::config::CommitMessageConfig;
use crate::git::{self, Gesture};

/// Le texte du spec §4.3 (celui de Zed, avec les changements de B11-3), sans message système.
pub const PROMPT: &str = r"You are an expert at writing Git commits. Your job is to write a short clear commit message that summarizes the changes.

If you can accurately express the change in just the subject line, don't include anything in the message body. Only use the body when it is providing *useful* information.

Don't repeat information from the subject line in the message body.

Only return the commit message in your response. Do not include any additional meta-commentary about the task. Do not include the raw diff output in the commit message.

The commit history is mostly read by AI agents resuming work, who often see only the subject line: make it self-sufficient.

Follow good Git style:

- Format the subject line as a Conventional Commit: type(scope): description
- Use one of these types: feat, fix, refactor, chore, docs, build, test, style, perf, ci
- The scope is optional: the main directory, crate or area touched, in lowercase
- Start the description with a lowercase letter
- Write the commit message in English
- Separate the subject from the body with a blank line
- Try to limit the subject line to 72 characters, type and scope included
- Do not end the subject line with any punctuation
- Use the imperative mood in the subject line
- Wrap the body at 72 characters
- Keep the body short and concise (omit it entirely if not useful)
";

/// Le plafond du diff envoyé (Zed : 20 000 octets).
pub const MAX_DIFF_BYTES: usize = 20_000;

/// Le nombre de lignes de fichiers gardées dans le résumé `--stat` ; la suite tient en `... and N more files`.
pub const MAX_STAT_FILES: usize = 200;

/// Les fichiers sensibles dont le diff n'envoie jamais le contenu (spec §4.2, écart E-B11-h) : le nom de fichier, sans
/// tenir compte de la casse, vaut exactement l'un de ces noms...
const SENSITIVE_NAMES: [&str; 7] = [
    ".env",
    ".envrc",
    ".netrc",
    "id_rsa",
    "id_dsa",
    "id_ecdsa",
    "id_ed25519",
];

/// ... commence par l'un de ces préfixes...
const SENSITIVE_PREFIXES: [&str; 1] = [".env."];

/// ... ou finit par l'un de ces suffixes.
const SENSITIVE_SUFFIXES: [&str; 5] = [".env", ".pem", ".key", ".p12", ".pfx"];

/// Les fichiers de règles du projet, dans l'ordre de Zed ; le premier qui a du contenu gagne.
const RULES_FILES: [&str; 9] = [
    ".rules",
    ".cursorrules",
    ".windsurfrules",
    ".clinerules",
    ".github/copilot-instructions.md",
    "AGENT.md",
    "AGENTS.md",
    "CLAUDE.md",
    "GEMINI.md",
];

/// Une ligne de diff plus longue est coupée à cette taille (Zed).
const MAX_LINE_BYTES: usize = 256;

const OMITTED_DIFF: &str =
    "The full diff is too large and was omitted; rely on the summary above.\n";

/// Les morceaux du prompt, sans mise en forme : `build_prompt` les assemble et borne le résumé et le diff.
pub struct PromptParts<'a> {
    pub rules: Option<&'a str>,
    pub instructions: Option<&'a str>,
    pub subject: Option<&'a str>,
    pub stat: &'a str,
    pub diff: &'a str,
}

/// Ce que le journal de debug retient d'une génération (spec §4.6) : jamais le texte du prompt.
#[derive(Debug, PartialEq, Eq)]
pub struct PromptStats {
    /// Les octets du diff réellement envoyés ; 0 quand il est omis.
    pub diff_bytes: usize,
    /// Vrai quand le diff dépassait le plafond et est passé par la compression, qu'il tienne ensuite ou non.
    pub compressed: bool,
    /// Vrai quand la phrase d'omission remplace le diff.
    pub omitted: bool,
}

/// L'échec d'une lecture de l'index : `raw` est la sortie d'erreur de Git, telle quelle (ou le message du lanceur).
#[derive(Debug)]
pub struct GatherError {
    pub raw: String,
}

/// La première ligne du champ de message, `None` si elle est vide après `trim`.
pub fn subject_line(field: &str) -> Option<&str> {
    let first = field.lines().next()?;
    (!first.trim().is_empty()).then_some(first)
}

/// Le résumé `--stat` borné à `max_files` lignes de fichiers, puis `... and N more files`, puis la ligne de total de Git
/// (` N files changed, …`, qui ne contient jamais ` | `, au contraire d'une ligne de fichier).
pub fn truncate_stat(stat: &str, max_files: usize) -> String {
    let mut lines: Vec<&str> = stat.lines().collect();
    let total = match lines.last() {
        Some(last) if is_stat_total(last) => lines.pop(),
        _ => None,
    };
    if lines.len() <= max_files {
        return stat.to_string();
    }
    let mut out = String::new();
    for line in &lines[..max_files] {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(&format!("... and {} more files\n", lines.len() - max_files));
    if let Some(total) = total {
        out.push_str(total);
        out.push('\n');
    }
    out
}

fn is_stat_total(line: &str) -> bool {
    !line.contains(" | ") && line.contains(" changed")
}

/// Le diff compressé comme chez Zed (`compress_commit_diff`) : tel quel s'il tient dans `max`, sinon lignes coupées à 256
/// octets, puis hunks retirés un par un. Le résultat peut dépasser `max` quand chaque fichier n'a plus qu'un hunk :
/// `build_prompt` omet alors le diff.
pub fn compress_diff(diff: &str, max: usize) -> String {
    if diff.len() <= max {
        return diff.to_string();
    }
    let cut: String = diff
        .lines()
        .map(|line| {
            if line.len() > MAX_LINE_BYTES {
                let end = line.floor_char_boundary(MAX_LINE_BYTES);
                format!("{}...[truncated]\n", &line[..end])
            } else {
                format!("{line}\n")
            }
        })
        .collect();
    if cut.len() <= max {
        return cut;
    }
    drop_hunks(&cut, max)
}

/// Le patch d'un fichier : son en-tête (tout ce qui précède le premier `@@`) et ses hunks.
struct FilePatch {
    header: String,
    hunks: Vec<String>,
    kept: usize,
}

impl FilePatch {
    fn skipped(&self) -> usize {
        self.hunks.len() - self.kept
    }

    fn marker(skipped: usize) -> String {
        if skipped == 0 {
            String::new()
        } else {
            format!("[...skipped {skipped} hunks...]\n")
        }
    }

    fn render(&self) -> String {
        let mut out = self.header.clone();
        for hunk in &self.hunks[..self.kept] {
            out.push_str(hunk);
        }
        out.push_str(&Self::marker(self.skipped()));
        out
    }
}

/// Coupe le texte (dont chaque ligne finit par `\n`) en patchs de fichier. Une ligne de contenu commence toujours par un
/// espace, `+`, `-` ou `\` : `diff --git ` et `@@` en tête de ligne sont donc sans ambiguïté. Zed coupe sur `---`, ce qui
/// colle l'en-tête `diff --git` d'un fichier à la fin du précédent et prend pour un fichier la ligne `--- x` d'une
/// suppression ; le format de `git diff` demande cette coupe-ci. Un fichier sans hunk (binaire, mode, renommage) garde
/// son en-tête au lieu de disparaître.
fn split_files(text: &str) -> Vec<FilePatch> {
    let mut files: Vec<FilePatch> = Vec::new();
    let mut in_hunks = false;
    for line in text.split_inclusive('\n') {
        if line.starts_with("diff --git ") || files.is_empty() {
            files.push(FilePatch {
                header: String::new(),
                hunks: Vec::new(),
                kept: 0,
            });
            in_hunks = false;
        }
        let file = files.last_mut().expect("a file was pushed above");
        if line.starts_with("@@") {
            in_hunks = true;
            file.hunks.push(String::new());
        }
        if in_hunks {
            file.hunks
                .last_mut()
                .expect("a hunk was pushed")
                .push_str(line);
        } else {
            file.header.push_str(line);
        }
    }
    for file in &mut files {
        file.kept = file.hunks.len();
    }
    files
}

/// Retire un hunk à la fois au fichier qui en garde le plus (le dernier à égalité, comme `max_by_key` chez Zed), sans
/// jamais descendre sous un hunk par fichier, jusqu'à tenir dans `max`. Le tas évite de relire tous les fichiers à chaque
/// retrait.
fn drop_hunks(text: &str, max: usize) -> String {
    let mut files = split_files(text);
    let mut size = text.len();
    let mut candidates: BinaryHeap<(usize, usize)> = files
        .iter()
        .enumerate()
        .filter(|(_, file)| file.kept > 1)
        .map(|(index, file)| (file.kept, index))
        .collect();
    while size > max {
        let Some((kept, index)) = candidates.pop() else {
            break;
        };
        let file = &mut files[index];
        let skipped = file.skipped();
        // Le hunk retiré sort de la taille, le marqueur « skipped » grossit d'un cran.
        size = size - file.hunks[kept - 1].len() - FilePatch::marker(skipped).len()
            + FilePatch::marker(skipped + 1).len();
        file.kept = kept - 1;
        if file.kept > 1 {
            candidates.push((file.kept, index));
        }
    }
    files.iter().map(FilePatch::render).collect()
}

/// Assemble le prompt : `PROMPT`, règles, instructions, titre, résumé, diff. Une section absente ne laisse aucune trace.
/// Le résumé et le diff sont bornés ici, quel que soit l'appelant.
pub fn build_prompt(parts: &PromptParts) -> (String, PromptStats) {
    let mut prompt = String::from(PROMPT);
    if let Some(rules) = parts.rules.filter(|rules| !rules.trim().is_empty()) {
        prompt.push_str(&format!(
            "\n\nThe user has provided the following rules specific to this project that you should follow when writing the commit message:\n<project_rules>\n{rules}\n</project_rules>\n"
        ));
    }
    if let Some(instructions) = parts
        .instructions
        .filter(|instructions| !instructions.trim().is_empty())
    {
        prompt.push_str(&format!(
            "\n\nThe user has provided the following instructions for writing commit messages that you should follow:\n<commit_message_instructions>\n{instructions}\n</commit_message_instructions>\n"
        ));
    }
    if let Some(subject) = parts.subject {
        prompt.push_str(&format!("\nHere is the user's subject line:\n{subject}"));
    }
    prompt.push_str(&format!(
        "\nHere is a summary of the changed files:\n{}",
        truncate_stat(parts.stat, MAX_STAT_FILES)
    ));
    let diff = compress_diff(parts.diff, MAX_DIFF_BYTES);
    let compressed = parts.diff.len() > MAX_DIFF_BYTES;
    let omitted = diff.len() > MAX_DIFF_BYTES;
    prompt.push_str("\nHere are the changes in this commit:\n");
    let diff_bytes = if omitted {
        prompt.push_str(OMITTED_DIFF);
        0
    } else {
        prompt.push_str(&diff);
        diff.len()
    };
    (
        prompt,
        PromptStats {
            diff_bytes,
            compressed,
            omitted,
        },
    )
}

/// Les règles du projet : le premier fichier de la liste de Zed qui existe à la racine et qui a du contenu, trimé. Un
/// fichier vide est sauté, pas un obstacle aux suivants.
pub fn project_rules(root: &Path) -> Option<String> {
    RULES_FILES.iter().find_map(|name| {
        let text = fs::read_to_string(root.join(name)).ok()?;
        let text = text.trim();
        (!text.is_empty()).then(|| text.to_string())
    })
}

/// Ce nom de fichier (dernier composant, casse ignorée) est-il sensible ?
fn is_sensitive_name(name: &str) -> bool {
    let name = name.to_lowercase();
    SENSITIVE_NAMES.contains(&name.as_str())
        || SENSITIVE_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix))
        || SENSITIVE_SUFFIXES
            .iter()
            .any(|suffix| name.ends_with(suffix))
}

/// Le texte d'une ligne d'en-tête nomme-t-il un fichier sensible ? Les chemins de Git peuvent contenir des espaces et
/// des guillemets (noms avec accents) : le texte entier compte pour un chemin, et chacun de ses mots aussi. Le
/// contrôle est volontairement large, un faux positif n'omet qu'un patch de plus.
fn names_a_sensitive_file(text: &str) -> bool {
    let last_component = |path: &str| {
        let path = path.trim_matches(|c: char| c == '"' || c.is_whitespace());
        path.rsplit('/').next().unwrap_or(path).to_string()
    };
    is_sensitive_name(&last_component(text))
        || text
            .split_whitespace()
            .any(|word| is_sensitive_name(&last_component(word)))
}

/// Les lignes d'en-tête d'un patch qui portent un chemin : la ligne `diff --git`, les renommages et copies, les `---` et
/// `+++` (absents d'un renommage pur, d'un binaire ou d'un changement de mode, d'où les autres).
fn path_lines(header: &str) -> impl Iterator<Item = &str> {
    const PREFIXES: [&str; 7] = [
        "diff --git ",
        "rename from ",
        "rename to ",
        "copy from ",
        "copy to ",
        "--- ",
        "+++ ",
    ];
    header
        .lines()
        .filter_map(|line| PREFIXES.iter().find_map(|prefix| line.strip_prefix(prefix)))
}

/// Le chemin d'arrivée du patch, pour la ligne d'omission : la cible d'un renommage ou d'une copie, sinon la ligne
/// `diff --git a/X b/X` coupée en deux moitiés égales (les chemins peuvent contenir des espaces), sinon son dernier mot.
fn destination_path(header: &str) -> String {
    for line in header.lines() {
        if let Some(path) = line
            .strip_prefix("rename to ")
            .or_else(|| line.strip_prefix("copy to "))
        {
            return path.to_string();
        }
    }
    let Some(line) = header
        .lines()
        .find_map(|line| line.strip_prefix("diff --git "))
    else {
        return String::new();
    };
    for (at, _) in line.match_indices(' ') {
        let (old, new) = (&line[..at], &line[at + 1..]);
        if let (Some(old), Some(new)) = (old.strip_prefix("a/"), new.strip_prefix("b/"))
            && old == new
        {
            return new.to_string();
        }
    }
    let last = line.rsplit(' ').next().unwrap_or(line);
    last.strip_prefix("b/").unwrap_or(last).to_string()
}

/// Remplace le patch de chaque fichier dont l'un des côtés est sensible par `[sensitive file omitted: <chemin>]`. Les
/// deux côtés comptent : renommer `.env` en `config.txt` ou l'inverse ne doit rien laisser passer. Le filtre est ici et
/// pas dans des pathspecs de Git : un pathspec respecte la casse (`ID.PEM` passait) et ne voit qu'un côté d'un renommage.
pub fn omit_sensitive_files(diff: &str) -> String {
    split_files(diff)
        .iter()
        .map(|file| {
            if path_lines(&file.header).any(names_a_sensitive_file) {
                format!(
                    "[sensitive file omitted: {}]\n",
                    destination_path(&file.header)
                )
            } else {
                file.render()
            }
        })
        .collect()
}

/// Lit l'index de la cible et assemble le prompt. Les deux lectures passent par le lanceur Git unique (geste lecture),
/// sans pilote externe ni textconv (un pilote git-crypt ou sops déchiffrerait les secrets et lancerait un programme) : le
/// résumé en entier, le diff sans le contenu des fichiers sensibles.
pub fn gather(
    root: &Path,
    settings: &CommitMessageConfig,
    field: &str,
) -> Result<(String, PromptStats), GatherError> {
    let stat = read_index(
        root,
        &[
            "diff",
            "--cached",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--stat=120",
        ],
    )?;
    let diff = read_index(
        root,
        &[
            "diff",
            "--cached",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            // `-M` impose la détection des renommages quelle que soit la config du dépôt (`diff.renames=false`) : sans elle,
            // `git mv .env config.txt` redevient suppression + ajout et le filtre ne voit plus que `config.txt`.
            "-M",
        ],
    )?;
    let diff = omit_sensitive_files(&diff);
    let rules = settings
        .include_project_rules
        .then(|| project_rules(root))
        .flatten();
    let parts = PromptParts {
        rules: rules.as_deref(),
        instructions: settings.instructions.as_deref(),
        subject: subject_line(field),
        stat: &stat,
        diff: &diff,
    };
    Ok(build_prompt(&parts))
}

fn read_index(root: &Path, args: &[&str]) -> Result<String, GatherError> {
    let output = git::run(root, Gesture::Read, args, None).map_err(|error| GatherError {
        raw: error.to_string(),
    })?;
    if !output.success() {
        return Err(GatherError { raw: output.stderr });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::changes::test_repo::{TempRepo, fixture_repo};
    use crate::config::parse_config;

    /// Un hunk de `bytes` octets environ, en lignes de 100 octets (sous la coupe des 256).
    fn hunk(bytes: usize) -> String {
        let mut text = String::from("@@ -1,3 +1,3 @@\n");
        for _ in 0..bytes / 100 {
            text.push('+');
            text.push_str(&"x".repeat(98));
            text.push('\n');
        }
        text
    }

    /// Le patch de `git diff` d'un fichier, avec `hunks` hunks de `bytes` octets.
    fn file_patch(name: &str, hunks: usize, bytes: usize) -> String {
        let mut text = format!(
            "diff --git a/{name} b/{name}\nindex 1111111..2222222 100644\n--- a/{name}\n+++ b/{name}\n"
        );
        for _ in 0..hunks {
            text.push_str(&hunk(bytes));
        }
        text
    }

    /// Le patch d'un fichier dans le résultat de `compress_diff`.
    fn section<'a>(diff: &'a str, name: &str) -> &'a str {
        let start = diff
            .find(&format!("diff --git a/{name} "))
            .unwrap_or_else(|| panic!("{name} vanished from the diff"));
        let rest = &diff[start..];
        let end = rest[1..]
            .find("diff --git ")
            .map_or(rest.len(), |at| at + 1);
        &rest[..end]
    }

    fn settings(text: &str) -> CommitMessageConfig {
        parse_config(text).unwrap().commit_message.unwrap()
    }

    fn stage(repo: &TempRepo, paths: &[&str]) {
        let mut args = vec!["add", "--"];
        args.extend(paths);
        repo.git(&args);
    }

    #[test]
    fn a_small_diff_passes_untouched() {
        let diff = file_patch("a.rs", 1, 800);
        assert!(diff.len() <= MAX_DIFF_BYTES);
        assert_eq!(compress_diff(&diff, MAX_DIFF_BYTES), diff);
    }

    #[test]
    fn long_lines_are_cut_on_a_char_boundary() {
        let long = format!("+{}", "é".repeat(300));
        let mut diff = file_patch("a.rs", 0, 0);
        diff.push_str("@@ -1,3 +1,3 @@\n");
        diff.push_str(&long);
        diff.push('\n');
        diff.push_str(&format!("+{}\n", "x".repeat(98)).repeat(250));
        assert!(diff.len() >= 25_000);

        let out = compress_diff(&diff, MAX_DIFF_BYTES);
        let line = out
            .lines()
            .find(|line| line.starts_with("+é"))
            .expect("the long line is kept");
        let kept = line
            .strip_suffix("...[truncated]")
            .expect("the cut line ends with the marker");
        assert!(kept.len() <= MAX_LINE_BYTES, "kept {} bytes", kept.len());
        // 256 tombe au milieu d'un `é` (2 octets, décalés d'un par le `+`) : la coupe recule d'un octet.
        assert_eq!(kept.chars().skip(1).filter(|c| *c == 'é').count(), 127);
        assert!(kept.chars().skip(1).all(|c| c == 'é'));
    }

    #[test]
    fn hunks_are_dropped_from_the_largest_file_first() {
        let diff = format!(
            "{}{}",
            file_patch("small.rs", 1, 600),
            file_patch("big.rs", 40, 600)
        );
        assert!(diff.len() > MAX_DIFF_BYTES);

        let out = compress_diff(&diff, MAX_DIFF_BYTES);
        assert!(out.len() <= MAX_DIFF_BYTES, "{} bytes left", out.len());
        assert_eq!(section(&out, "small.rs").matches("@@ ").count(), 1);
        let big = section(&out, "big.rs");
        assert!(big.matches("@@ ").count() < 40);
        assert!(big.contains("[...skipped "));
        assert!(!section(&out, "small.rs").contains("[...skipped "));
    }

    #[test]
    fn a_file_without_hunk_survives_the_compression() {
        let binary = "diff --git a/logo.bin b/logo.bin\nindex 1111111..2222222 100644\nBinary files a/logo.bin and b/logo.bin differ\n";
        let diff = format!("{binary}{}", file_patch("big.rs", 40, 600));
        let out = compress_diff(&diff, MAX_DIFF_BYTES);
        assert!(section(&out, "logo.bin").contains("Binary files"));
        assert!(out.len() <= MAX_DIFF_BYTES);
    }

    #[test]
    fn every_file_keeps_one_hunk() {
        let diff: String = (0..30)
            .map(|n| file_patch(&format!("f{n}.rs"), 1, 2_000))
            .collect();
        let out = compress_diff(&diff, MAX_DIFF_BYTES);
        assert!(out.len() > MAX_DIFF_BYTES);
        for n in 0..30 {
            assert_eq!(section(&out, &format!("f{n}.rs")).matches("@@ ").count(), 1);
        }

        let parts = PromptParts {
            rules: None,
            instructions: None,
            subject: None,
            stat: " 30 files changed, 600 insertions(+)\n",
            diff: &diff,
        };
        let (prompt, stats) = build_prompt(&parts);
        assert!(stats.omitted);
        assert!(stats.compressed);
        assert_eq!(stats.diff_bytes, 0);
        assert!(prompt.ends_with(&format!(
            "\nHere are the changes in this commit:\n{OMITTED_DIFF}"
        )));
        assert!(!prompt.contains("f0.rs"));
    }

    #[test]
    fn the_stat_is_cut_after_200_files() {
        let mut stat: String = (0..30_000)
            .map(|n| format!(" src/file{n}.rs | 2 +-\n"))
            .collect();
        let total = " 30000 files changed, 30000 insertions(+), 30000 deletions(-)";
        stat.push_str(total);
        stat.push('\n');

        let out = truncate_stat(&stat, MAX_STAT_FILES);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), MAX_STAT_FILES + 2);
        assert_eq!(lines[0], " src/file0.rs | 2 +-");
        assert_eq!(lines[MAX_STAT_FILES], "... and 29800 more files");
        assert_eq!(lines[MAX_STAT_FILES + 1], total);
    }

    #[test]
    fn a_short_stat_is_left_alone() {
        let stat = " a.txt | 1 +\n 1 file changed, 1 insertion(+)\n";
        assert_eq!(truncate_stat(stat, MAX_STAT_FILES), stat);
    }

    #[test]
    fn sections_follow_zed_order_and_text() {
        let parts = PromptParts {
            rules: Some("Use the scope \"test\"."),
            instructions: Some("Be terse."),
            subject: Some("docs: notes"),
            stat: " a.txt | 1 +\n 1 file changed, 1 insertion(+)\n",
            diff: "diff --git a/a.txt b/a.txt\n",
        };
        let (prompt, stats) = build_prompt(&parts);
        assert!(prompt.starts_with(PROMPT));
        assert_eq!(
            stats,
            PromptStats {
                diff_bytes: parts.diff.len(),
                compressed: false,
                omitted: false
            }
        );
        let pieces = [
            "\n\nThe user has provided the following rules specific to this project that you should follow when writing the commit message:\n<project_rules>\nUse the scope \"test\".\n</project_rules>\n",
            "\n\nThe user has provided the following instructions for writing commit messages that you should follow:\n<commit_message_instructions>\nBe terse.\n</commit_message_instructions>\n",
            "\nHere is the user's subject line:\ndocs: notes",
            "\nHere is a summary of the changed files:\n a.txt | 1 +\n 1 file changed, 1 insertion(+)\n",
            "\nHere are the changes in this commit:\ndiff --git a/a.txt b/a.txt\n",
        ];
        let mut from = 0;
        for piece in pieces {
            let at = prompt[from..]
                .find(piece)
                .unwrap_or_else(|| panic!("missing or out of order: {piece:?}"));
            from += at + piece.len();
        }
    }

    #[test]
    fn absent_sections_leave_no_trace() {
        let parts = PromptParts {
            rules: None,
            instructions: Some("  \n"),
            subject: None,
            stat: "",
            diff: "",
        };
        let (prompt, _) = build_prompt(&parts);
        assert!(!prompt.contains("<project_rules>"));
        assert!(!prompt.contains("<commit_message_instructions>"));
        assert!(!prompt.contains("user's subject line"));
    }

    #[test]
    fn a_blank_field_sends_no_subject() {
        assert_eq!(subject_line("  \n\n"), None);
        assert_eq!(subject_line(""), None);
        assert_eq!(subject_line("docs: notes\nbody"), Some("docs: notes"));
    }

    #[test]
    fn the_first_rules_file_wins() {
        let repo = TempRepo::init();
        assert_eq!(project_rules(repo.path()), None);
        repo.write("CLAUDE.md", b"from claude\n");
        assert_eq!(project_rules(repo.path()).as_deref(), Some("from claude"));
        repo.write("AGENTS.md", b"  from agents  \n");
        assert_eq!(project_rules(repo.path()).as_deref(), Some("from agents"));
        // Un fichier vide est sauté.
        repo.write(".rules", b" \n\n");
        assert_eq!(project_rules(repo.path()).as_deref(), Some("from agents"));
        repo.write(".rules", b"first\n");
        assert_eq!(project_rules(repo.path()).as_deref(), Some("first"));
    }

    #[test]
    fn gather_reads_only_the_index() {
        let repo = fixture_repo();
        stage(&repo, &["README.md", "notes/brouillon.md"]);
        repo.write("CLAUDE.md", b"Use the scope \"test\".\n");

        let config = settings("[commit_message]\nenabled = true\n");
        let (prompt, stats) = gather(repo.path(), &config, "feat: wip\nbody").unwrap();
        assert!(prompt.contains("README.md"));
        assert!(prompt.contains("brouillon.md"));
        assert!(prompt.contains("Use the scope \"test\"."));
        // Seule la première ligne du champ part.
        assert!(prompt.contains("subject line:\nfeat: wip\nHere is a summary"));
        assert!(!prompt.contains("old.txt"));
        assert!(!prompt.contains("logo.bin"));
        assert!(!prompt.contains("captures"));
        assert!(!stats.omitted && !stats.compressed && stats.diff_bytes > 0);

        let off = settings("[commit_message]\nenabled = true\ninclude_project_rules = false\n");
        let (prompt, _) = gather(repo.path(), &off, "").unwrap();
        assert!(!prompt.contains("<project_rules>"));
    }

    /// Un chemin sensible par motif de la liste, majuscules et sous-dossiers compris (un dossier par cas : le disque de
    /// macOS ignore la casse, `.env` et `.ENV` ne tiendraient pas côte à côte).
    const SENSITIVE_PATHS: [&str; 17] = [
        "a/.env",
        "b/.ENV",
        "c/.envrc",
        "d/.netrc",
        "e/id_rsa",
        "f/id_dsa",
        "g/id_ecdsa",
        "h/id_ed25519",
        "i/.env.local",
        "j/prod.env",
        "k/ID.PEM",
        "l/server.Key",
        "m/cert.p12",
        "n/bundle.pfx",
        "o/ID_RSA",
        ".env",
        "deep/er/dir/.Env.production",
    ];

    #[test]
    fn sensitive_files_are_named_but_never_sent() {
        let repo = fixture_repo();
        let mut staged = vec!["README.md", "conf/settings.toml"];
        repo.write("conf/settings.toml", b"ORDINARY-WITNESS\n");
        for (n, path) in SENSITIVE_PATHS.iter().enumerate() {
            repo.write(path, format!("SECRET-WITNESS-{n}\n").as_bytes());
            staged.push(path);
        }
        stage(&repo, &staged);

        let config = settings("[commit_message]\nenabled = true\n");
        let (prompt, _) = gather(repo.path(), &config, "").unwrap();
        for path in SENSITIVE_PATHS {
            assert!(prompt.contains(path), "{path} is not named in the summary");
            assert!(
                prompt.contains(&format!("[sensitive file omitted: {path}]")),
                "{path} has no omission line"
            );
        }
        assert!(
            !prompt.contains("SECRET-WITNESS"),
            "a sensitive content reached the prompt"
        );
        // Les voisins ordinaires restent dans le diff.
        assert!(prompt.contains("+ORDINARY-WITNESS"));
        assert!(prompt.contains("ligne modifiée"));
    }

    #[test]
    fn a_sensitive_file_renamed_away_is_still_omitted() {
        let repo = fixture_repo();
        repo.write(
            ".env",
            b"SECRET-WITNESS=1\nline two\nline three\nline four\n",
        );
        repo.write(
            "cert.key",
            b"SECRET-WITNESS=2\nline two\nline three\nline four\n",
        );
        repo.commit_all("Add secrets");
        repo.git(&["mv", ".env", "config.txt"]);
        repo.git(&["mv", "cert.key", "cert.key.bak"]);

        let config = settings("[commit_message]\nenabled = true\n");
        let (prompt, _) = gather(repo.path(), &config, "").unwrap();
        assert!(!prompt.contains("SECRET-WITNESS"), "{prompt}");
        assert!(prompt.contains("[sensitive file omitted: config.txt]"));
        assert!(prompt.contains("[sensitive file omitted: cert.key.bak]"));
    }

    #[test]
    fn a_rename_is_detected_even_with_diff_renames_off() {
        let repo = fixture_repo();
        repo.write(
            ".env",
            b"SECRET-WITNESS=1\nline two\nline three\nline four\n",
        );
        repo.commit_all("Add secret");
        // Config locale du dépôt jetable : sans `-M`, le renommage redevient suppression + ajout et `config.txt` partirait.
        repo.git(&["config", "diff.renames", "false"]);
        repo.git(&["mv", ".env", "config.txt"]);

        let config = settings("[commit_message]\nenabled = true\n");
        let (prompt, _) = gather(repo.path(), &config, "").unwrap();
        assert!(!prompt.contains("SECRET-WITNESS"), "{prompt}");
        assert!(prompt.contains("[sensitive file omitted: config.txt]"));
    }

    #[test]
    fn a_renamed_and_edited_sensitive_file_is_omitted() {
        let repo = fixture_repo();
        repo.write(
            ".env",
            b"SECRET-WITNESS=1\nline two\nline three\nline four\n",
        );
        repo.commit_all("Add secret");
        repo.git(&["mv", ".env", "config.txt"]);
        repo.write(
            "config.txt",
            b"SECRET-WITNESS=1\nline two\nline three\nline four\nEXTRA=1\n",
        );
        stage(&repo, &["config.txt"]);

        let config = settings("[commit_message]\nenabled = true\n");
        let (prompt, _) = gather(repo.path(), &config, "").unwrap();
        assert!(!prompt.contains("SECRET-WITNESS"), "{prompt}");
        assert!(!prompt.contains("EXTRA=1"));
        assert!(prompt.contains("[sensitive file omitted: config.txt]"));
    }

    #[test]
    fn an_ordinary_file_renamed_to_a_sensitive_name_is_omitted() {
        let repo = fixture_repo();
        repo.write(
            "notes.txt",
            b"SECRET-WITNESS=1\nline two\nline three\nline four\n",
        );
        repo.commit_all("Add notes");
        repo.git(&["mv", "notes.txt", "id_rsa"]);

        let config = settings("[commit_message]\nenabled = true\n");
        let (prompt, _) = gather(repo.path(), &config, "").unwrap();
        assert!(!prompt.contains("SECRET-WITNESS"), "{prompt}");
        assert!(prompt.contains("[sensitive file omitted: id_rsa]"));
    }

    #[test]
    fn a_textconv_driver_never_runs_on_the_index() {
        let repo = fixture_repo();
        // Un pilote textconv (git-crypt, sops) lance un programme et déchiffre : ici, il passe tout en majuscules.
        repo.git(&["config", "diff.upper.textconv", "tr a-z A-Z <"]);
        repo.write(".gitattributes", b"*.sec diff=upper\n");
        repo.write("notes.sec", b"clearwitness\n");
        stage(&repo, &[".gitattributes", "notes.sec"]);

        let config = settings("[commit_message]\nenabled = true\n");
        let (prompt, _) = gather(repo.path(), &config, "").unwrap();
        assert!(prompt.contains("+clearwitness"), "{prompt}");
        assert!(!prompt.contains("CLEARWITNESS"));
    }

    /// Le patch d'un fichier qui ne montre que son en-tête de renommage ou de mode.
    fn bare_patch(header: &str) -> String {
        format!("{header}index 1111111..2222222 100644\n")
    }

    #[test]
    fn the_filter_reads_quoted_and_spaced_paths() {
        let quoted = "diff --git \"a/caf\\303\\251.env\" \"b/caf\\303\\251.env\"\nindex 1111111..2222222 100644\n--- \"a/caf\\303\\251.env\"\n+++ \"b/caf\\303\\251.env\"\n@@ -1 +1 @@\n-SECRET-WITNESS\n+SECRET-WITNESS-2\n";
        let spaced = "diff --git a/my dir/.env b/my dir/.env\nindex 1111111..2222222 100644\n--- a/my dir/.env\t\n+++ b/my dir/.env\t\n@@ -1 +1 @@\n-SECRET-WITNESS\n+SECRET-WITNESS-2\n";
        let copied = "diff --git a/a.txt b/id_ed25519\nsimilarity index 90%\ncopy from a.txt\ncopy to id_ed25519\nindex 1111111..2222222 100644\n--- a/a.txt\n+++ b/id_ed25519\n@@ -1 +1 @@\n-SECRET-WITNESS\n+SECRET-WITNESS-2\n";
        let ordinary = file_patch("src/environment.rs", 1, 200);
        let diff = format!("{quoted}{spaced}{copied}{ordinary}");

        let out = omit_sensitive_files(&diff);
        assert!(!out.contains("SECRET-WITNESS"), "{out}");
        assert!(out.contains("[sensitive file omitted: "));
        assert_eq!(out.matches("[sensitive file omitted: ").count(), 3);
        assert!(out.contains("[sensitive file omitted: id_ed25519]"));
        assert!(section(&out, "src/environment.rs").contains("@@ "));
    }

    #[test]
    fn lookalike_names_are_not_sensitive() {
        let names = [
            "src/environment.rs",
            "src/.environment",
            "keyboard.rs",
            "turkey",
            "monkey.txt",
            "id_rsa.pub",
            "pem.rs",
            "docs/env.md",
            "id_rsa_notes.txt",
            ".github/workflows/env.yml",
        ];
        let diff: String = names.iter().map(|n| file_patch(n, 1, 200)).collect();
        assert_eq!(omit_sensitive_files(&diff), diff);
    }

    #[test]
    fn a_header_only_patch_is_filtered_too() {
        let rename = bare_patch(
            "diff --git a/.env b/config.txt\nsimilarity index 100%\nrename from .env\nrename to config.txt\n",
        );
        let neighbour = file_patch("b.rs", 1, 200);
        let out = omit_sensitive_files(&format!("{rename}{neighbour}"));
        assert!(out.starts_with("[sensitive file omitted: config.txt]\n"));
        assert!(section(&out, "b.rs").contains("@@ "));
    }

    #[test]
    fn a_failing_git_read_reports_its_output() {
        let repo = TempRepo::init();
        let config = settings("[commit_message]\nenabled = true\n");
        let outside = repo.path().join("missing-directory");
        let error = gather(&outside, &config, "").unwrap_err();
        assert!(!error.raw.is_empty());
    }
}
