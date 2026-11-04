//! La ligne du `.gitignore` (spec P3 §4.8) : ce qui s'écrit, sans toucher aux lignes déjà là. L'écriture passe par le
//! système de fichiers, pas par Git.

use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::git::{Gesture, run};
use crate::hud;

/// Le texte de l'alerte quand le `.gitignore` ne s'écrit pas (spec P3 §5) ; le message brut de l'erreur est en détail.
pub const WRITE_FAILED: &str = "Impossible d'écrire le .gitignore.";

/// Le détail de l'alerte quand le nom du fichier porte un retour à la ligne.
pub const LINE_BREAK_IN_NAME: &str = "nom de fichier avec retour à la ligne";

/// Ce que `add_to_gitignore` a fait de la ligne.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IgnoreOutcome {
    /// La ligne a été ajoutée au fichier.
    Added(String),
    /// La ligne y était déjà : rien n'a été écrit.
    Exists(String),
    /// Git suit déjà ce chemin (ou un fichier dessous) : rien n'a été écrit. L'entrée du menu était allumée à tort, par
    /// une course (un agent a stagé entre deux lectures).
    Refused(String),
}

impl IgnoreOutcome {
    /// Une ligne au journal : `changes ignore added line=…`, `changes ignore exists line=…` ou
    /// `changes ignore refused reason=tracked line=…`.
    pub fn log(&self) {
        hud::log_event(&self.log_message());
    }

    fn log_message(&self) -> String {
        match self {
            IgnoreOutcome::Added(line) => format!("changes ignore added line={line}"),
            IgnoreOutcome::Exists(line) => format!("changes ignore exists line={line}"),
            IgnoreOutcome::Refused(line) => {
                format!("changes ignore refused reason=tracked line={line}")
            }
        }
    }
}

/// La ligne qui ignore `rel` (relatif à la racine du dépôt) : `/a/b` pour un fichier, `/a/b/` pour un dossier. Les
/// caractères que Git lit comme un motif (`* ? [ \`) et un espace final sont protégés par une barre oblique inverse,
/// pour que `[id].tsx` n'ignore que lui-même.
pub fn ignore_line(rel: &Path, is_dir: bool) -> String {
    let mut line = String::from("/");
    let path = rel.to_string_lossy();
    for (index, component) in path.split('/').filter(|part| !part.is_empty()).enumerate() {
        if index > 0 {
            line.push('/');
        }
        for c in component.chars() {
            if matches!(c, '*' | '?' | '[' | '\\') {
                line.push('\\');
            }
            line.push(c);
        }
    }
    // Git retire les espaces finaux d'une ligne, sauf le dernier s'il est protégé.
    if line.ends_with(' ') {
        line.insert(line.len() - 1, '\\');
    }
    if is_dir {
        line.push('/');
    }
    line
}

/// Le contenu du `.gitignore` une fois `line` ajoutée à `existing`, `None` si la ligne y est déjà. La fin de ligne du
/// fichier (`\n` ou `\r\n`) est gardée, et un saut de ligne final manquant est ajouté avant la nouvelle ligne. Le
/// résultat commence toujours par `existing`, tel quel.
pub fn append_line(existing: Option<&str>, line: &str) -> Option<String> {
    let existing = existing.unwrap_or("");
    if existing
        .lines()
        .any(|present| present.trim_end_matches('\r') == line)
    {
        return None;
    }
    let eol = if existing.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let mut content = existing.to_string();
    if !content.is_empty() && !content.ends_with('\n') {
        content.push_str(eol);
    }
    content.push_str(line);
    content.push_str(eol);
    Some(content)
}

/// Ajoute la ligne de `rel` au `.gitignore` de la racine `root`, qui est créé s'il manque. Le fichier est ouvert en
/// ajout : seule la fin s'écrit, jamais les lignes déjà là. Un nom qui porte un retour à la ligne est refusé avant toute
/// écriture : il s'écrirait sur deux lignes, et la seconde serait une règle que l'utilisateur n'a pas demandée.
pub fn add_to_gitignore(root: &Path, rel: &Path, is_dir: bool) -> io::Result<IgnoreOutcome> {
    if rel.to_string_lossy().contains(['\n', '\r']) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            LINE_BREAK_IN_NAME,
        ));
    }
    let path = root.join(".gitignore");
    let existing = match fs::read_to_string(&path) {
        Ok(text) => Some(text),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    let line = ignore_line(rel, is_dir);
    let Some(content) = append_line(existing.as_deref(), &line) else {
        return Ok(IgnoreOutcome::Exists(line));
    };
    let tail = &content[existing.as_deref().map_or(0, str::len)..];
    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
    file.write_all(tail.as_bytes())?;
    Ok(IgnoreOutcome::Added(line))
}

/// Ajoute au `.gitignore` de la racine du dépôt le chemin absolu `path`. La racine est celle que Git donne
/// (`rev-parse --show-toplevel`, en lecture) depuis `dir`, un dossier du dépôt : le projet peut n'en être qu'un
/// sous-dossier, et la ligne s'écrit alors relative à la racine du dépôt, pas à celle du projet.
pub fn add_path_to_gitignore(dir: &Path, path: &Path, is_dir: bool) -> io::Result<IgnoreOutcome> {
    let top = run(dir, Gesture::Read, &["rev-parse", "--show-toplevel"], None)?;
    if !top.success() {
        return Err(io::Error::other(top.stderr.trim().to_string()));
    }
    let bytes = top.stdout.strip_suffix(b"\n").unwrap_or(&top.stdout);
    let root = PathBuf::from(OsStr::from_bytes(bytes));
    let rel = path.strip_prefix(&root).map_err(|_| {
        io::Error::other(format!(
            "{} is outside the repository {}",
            path.display(),
            root.display()
        ))
    })?;
    add_unless_tracked(&root, rel, is_dir)
}

/// Git suit-il `rel` (relatif à la racine `root`) : un fichier de l'index, ou un dossier qui en contient un ? Une lecture
/// (`ls-files --cached`, chemin littéral) faite à l'instant du geste : la liste ou l'arbre que le menu a consultés ont pu
/// vieillir (un agent a stagé entre deux lectures).
pub fn is_tracked(root: &Path, rel: &Path) -> io::Result<bool> {
    let spec = format!("{}{}", crate::changes::ops::LITERAL, rel.to_string_lossy());
    let output = run(
        root,
        Gesture::Read,
        &["ls-files", "-z", "--cached", "--", &spec],
        None,
    )?;
    if !output.success() {
        return Err(io::Error::other(output.stderr.trim().to_string()));
    }
    Ok(!output.stdout.is_empty())
}

/// Comme `add_to_gitignore`, mais seulement pour ce que Git ne suit pas : relu à l'instant de l'écriture. Un chemin suivi
/// n'est pas écrit (`Refused`) : cacher un dossier qui porte des fichiers suivis masquerait tous ses futurs fichiers.
pub fn add_unless_tracked(root: &Path, rel: &Path, is_dir: bool) -> io::Result<IgnoreOutcome> {
    if is_tracked(root, rel)? {
        return Ok(IgnoreOutcome::Refused(ignore_line(rel, is_dir)));
    }
    add_to_gitignore(root, rel, is_dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("idema-ignore-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn ignore_line_of_a_folder_ends_with_a_slash() {
        assert_eq!(ignore_line(Path::new("captures"), true), "/captures/");
    }

    #[test]
    fn ignore_line_of_a_file_has_no_trailing_slash() {
        assert_eq!(
            ignore_line(Path::new("notes/brouillon.md"), false),
            "/notes/brouillon.md"
        );
    }

    #[test]
    fn ignore_line_protects_glob_characters_and_a_trailing_space() {
        assert_eq!(
            ignore_line(Path::new("app/[id].tsx"), false),
            "/app/\\[id].tsx"
        );
        assert_eq!(ignore_line(Path::new("a*b?"), false), "/a\\*b\\?");
        assert_eq!(ignore_line(Path::new("a b "), false), "/a b\\ ");
        assert_eq!(
            ignore_line(Path::new("notes/a b.md"), false),
            "/notes/a b.md"
        );
    }

    #[test]
    fn append_line_to_nothing_or_an_empty_file() {
        assert_eq!(append_line(None, "/x").as_deref(), Some("/x\n"));
        assert_eq!(append_line(Some(""), "/x").as_deref(), Some("/x\n"));
    }

    #[test]
    fn append_line_adds_the_missing_final_newline() {
        assert_eq!(append_line(Some("a"), "/x").as_deref(), Some("a\n/x\n"));
        assert_eq!(append_line(Some("a\n"), "/x").as_deref(), Some("a\n/x\n"));
    }

    #[test]
    fn append_line_keeps_the_crlf_of_the_file() {
        assert_eq!(
            append_line(Some("a\r\n"), "/x").as_deref(),
            Some("a\r\n/x\r\n")
        );
        assert_eq!(
            append_line(Some("a\r\nb"), "/x").as_deref(),
            Some("a\r\nb\r\n/x\r\n")
        );
    }

    #[test]
    fn append_line_skips_a_line_already_there() {
        assert_eq!(append_line(Some("a\n/x\nb\n"), "/x"), None);
        assert_eq!(append_line(Some("a\r\n/x\r\n"), "/x"), None);
        assert_eq!(append_line(Some("a\n/x"), "/x"), None);
        assert_eq!(append_line(Some("a\n/x\r"), "/x"), None);
    }

    #[test]
    fn append_line_does_not_take_a_longer_line_for_a_duplicate() {
        assert!(append_line(Some("/xy\n"), "/x").is_some());
    }

    #[test]
    fn add_to_gitignore_creates_then_skips() {
        let root = temp_dir("create");
        let rel = Path::new("captures");

        let first = add_to_gitignore(&root, rel, true).unwrap();
        let second = add_to_gitignore(&root, rel, true).unwrap();

        assert_eq!(first, IgnoreOutcome::Added("/captures/".to_string()));
        assert_eq!(second, IgnoreOutcome::Exists("/captures/".to_string()));
        let text = fs::read_to_string(root.join(".gitignore")).unwrap();
        assert_eq!(text, "/captures/\n");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_subfolder_project_writes_at_the_repository_root() {
        use crate::changes::test_repo::TempRepo;
        let repo = TempRepo::init();
        repo.write("app/notes/b.md", b"x");
        let project = repo.path().join("app");

        let outcome = add_path_to_gitignore(&project, &project.join("notes/b.md"), false).unwrap();

        // La ligne est relative à la racine du dépôt, et le fichier est celui de la racine.
        assert_eq!(outcome, IgnoreOutcome::Added("/app/notes/b.md".to_string()));
        let text = fs::read_to_string(repo.path().join(".gitignore")).unwrap();
        assert_eq!(text, "/app/notes/b.md\n");
        assert!(!project.join(".gitignore").exists());
        // Git lit la règle : le fichier est ignoré, il ne reste de non suivi que le `.gitignore` lui-même.
        assert_eq!(repo.git(&["status", "--porcelain"]).trim(), "?? .gitignore");
    }

    #[test]
    fn a_tracked_path_is_refused_at_gesture_time_and_nothing_is_written() {
        use crate::changes::test_repo::TempRepo;
        let repo = TempRepo::init();
        repo.write("notes/old.md", b"a");
        repo.write("notes/new.md", b"b");
        repo.write("README.md", b"r");
        repo.git(&["add", "notes/old.md", "README.md"]);
        let root = repo.path();

        // Un fichier stagé, un dossier qui en contient un : refusés, rien n'est écrit.
        assert_eq!(
            add_unless_tracked(root, Path::new("README.md"), false).unwrap(),
            IgnoreOutcome::Refused("/README.md".to_string())
        );
        assert_eq!(
            add_unless_tracked(root, Path::new("notes"), true).unwrap(),
            IgnoreOutcome::Refused("/notes/".to_string())
        );
        assert!(!root.join(".gitignore").exists());
        // Un fichier que Git ne suit pas, même dans un dossier suivi : ajouté.
        assert_eq!(
            add_unless_tracked(root, Path::new("notes/new.md"), false).unwrap(),
            IgnoreOutcome::Added("/notes/new.md".to_string())
        );
        assert_eq!(
            fs::read_to_string(root.join(".gitignore")).unwrap(),
            "/notes/new.md\n"
        );
    }

    #[test]
    fn the_tracked_check_reads_a_path_with_glob_characters_literally() {
        use crate::changes::test_repo::TempRepo;
        let repo = TempRepo::init();
        repo.write("app/[id].tsx", b"x");
        repo.write("app/i.tsx", b"x");
        repo.git(&["add", "app/i.tsx"]);

        // `[id].tsx` n'est pas suivi : le motif ne doit pas lire `i.tsx`.
        assert!(!is_tracked(repo.path(), Path::new("app/[id].tsx")).unwrap());
        assert!(is_tracked(repo.path(), Path::new("app/i.tsx")).unwrap());
    }

    #[test]
    fn a_refusal_logs_the_reason_and_the_line() {
        // Le journal est le seul témoin d'un refus silencieux : son libellé est celui des scripts.
        let message = |outcome: IgnoreOutcome| outcome.log_message();
        assert_eq!(
            message(IgnoreOutcome::Refused("/notes/".to_string())),
            "changes ignore refused reason=tracked line=/notes/"
        );
        assert_eq!(
            message(IgnoreOutcome::Added("/a".to_string())),
            "changes ignore added line=/a"
        );
        assert_eq!(
            message(IgnoreOutcome::Exists("/a".to_string())),
            "changes ignore exists line=/a"
        );
    }

    #[test]
    fn a_path_outside_the_repository_is_refused_and_nothing_is_written() {
        use crate::changes::test_repo::TempRepo;
        let repo = TempRepo::init();

        let outcome = add_path_to_gitignore(repo.path(), Path::new("/somewhere/else.md"), false);

        assert!(outcome.is_err());
        assert!(!repo.path().join(".gitignore").exists());
    }

    #[test]
    fn a_name_with_a_line_break_is_refused_and_nothing_is_written() {
        use crate::changes::test_repo::TempRepo;
        let repo = TempRepo::init();
        repo.write(".gitignore", b"/target/\n");
        // Sans le refus, ce nom s'écrirait `/a` puis `/*` : une règle qui ignore tout, ajoutée à l'insu de l'utilisateur.
        for name in ["a\n*", "a\r\nb", "dir/x\n"] {
            let error = add_unless_tracked(repo.path(), Path::new(name), false).unwrap_err();

            assert_eq!(error.to_string(), "nom de fichier avec retour à la ligne");
        }
        let text = fs::read_to_string(repo.path().join(".gitignore")).unwrap();
        assert_eq!(text, "/target/\n");
    }

    #[test]
    fn add_to_gitignore_leaves_the_existing_lines_untouched() {
        let root = temp_dir("append");
        fs::write(root.join(".gitignore"), "/.worktrees/\r\ntarget").unwrap();

        let outcome = add_to_gitignore(&root, Path::new("notes/brouillon.md"), false).unwrap();

        assert_eq!(
            outcome,
            IgnoreOutcome::Added("/notes/brouillon.md".to_string())
        );
        let text = fs::read_to_string(root.join(".gitignore")).unwrap();
        assert_eq!(text, "/.worktrees/\r\ntarget\r\n/notes/brouillon.md\r\n");
        let _ = fs::remove_dir_all(&root);
    }
}
