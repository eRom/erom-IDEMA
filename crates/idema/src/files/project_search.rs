//! Chercher et remplacer dans les fichiers du projet, sans fenêtre (P2 jalon 3) : la liste des fichiers par Git, la
//! recherche ligne par ligne avec le crate `regex`, le remplacement des seules lignes cochées, et l'écriture par la
//! sauvegarde atomique gardée du document. Aucune dépendance à GPUI : tout se teste sur le disque.

use std::borrow::Cow;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::io;
use std::ops::Range;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use regex::{Regex, RegexBuilder};

use crate::errors::explain_write;
use crate::files::document::{Opened, Saved, read_document, save_unless_changed};
use crate::files::tabs::fingerprint;
use crate::git::{Gesture, run as run_git};

/// Le nombre de lignes de résultats au-delà duquel la recherche s'arrête et le dit.
pub const MAX_RESULT_LINES: usize = 1000;

/// Les fichiers ordinaires du projet : ceux que Git suit et ceux qu'il ne suit pas encore sans les ignorer, en chemins
/// absolus, triés. Un fichier suivi disparu du disque, un lien symbolique et un sous-module ne sont pas des fichiers
/// ordinaires : ils sont écartés, comme `.DS_Store`, que le Finder sème partout. `Err` quand `git` échoue (hors dépôt, binaire absent). Ne touche à rien.
pub fn project_files(root: &Path) -> io::Result<Vec<PathBuf>> {
    let output = run_git(
        root,
        Gesture::Read,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
        None,
    )?;
    if !output.success() {
        return Err(io::Error::other("git ls-files failed"));
    }
    let mut files: Vec<PathBuf> = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .map(|entry| root.join(OsStr::from_bytes(entry)))
        .filter(|path| path.file_name() != Some(OsStr::new(".DS_Store")))
        .filter(|path| path.symlink_metadata().is_ok_and(|meta| meta.is_file()))
        .collect();
    files.sort();
    files.dedup();
    Ok(files)
}

/// Les bascules de la recherche ; le défaut est celui de la spec : insensible à la casse, texte simple.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SearchOptions {
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub regex: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryError {
    InvalidRegex,
}

/// Le motif de la recherche : la requête telle quelle en mode expression, échappée sinon ; entouré de `\b` pour le mot
/// entier. En mode expression, la requête seule est compilée d'abord : entourée, un motif déséquilibré comme `a)|(b`
/// deviendrait valide et chercherait autre chose que ce qui a été tapé.
pub fn compile(query: &str, options: SearchOptions) -> Result<Regex, QueryError> {
    let build = |pattern: &str| {
        RegexBuilder::new(pattern)
            .case_insensitive(!options.case_sensitive)
            .build()
            .map_err(|_| QueryError::InvalidRegex)
    };
    let pattern = if options.regex {
        let raw = build(query)?;
        if !options.whole_word {
            return Ok(raw);
        }
        query.to_string()
    } else {
        regex::escape(query)
    };
    if options.whole_word {
        build(&format!(r"\b(?:{pattern})\b"))
    } else {
        build(&pattern)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct LineMatch {
    pub line: usize,
    pub text: String,
    pub ranges: Vec<Range<usize>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FileMatches {
    pub path: PathBuf,
    pub lines: Vec<LineMatch>,
    /// Le nombre de lignes du texte au moment de la recherche : le remplacement n'écrit que si le texte en a encore
    /// autant (voir `replace_lines`).
    pub line_count: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SearchOutcome {
    pub files: Vec<FileMatches>,
    pub truncated: bool,
}

impl SearchOutcome {
    pub fn line_count(&self) -> usize {
        self.files.iter().map(|file| file.lines.len()).sum()
    }
}

/// Les lignes d'un texte en `\n` : le segment vide qui suit un `\n` final n'en est pas une.
fn text_lines(text: &str) -> impl Iterator<Item = &str> {
    text.strip_suffix('\n').unwrap_or(text).split('\n')
}

/// Le nombre de lignes d'un texte en `\n`, comme `text_lines` les compte.
pub fn count_lines(text: &str) -> usize {
    text_lines(text).count()
}

/// Les occurrences non vides de `regex` dans une ligne, en octets. Une occurrence vide (`x*` partout) n'est pas une
/// occurrence : ni listée ni remplacée.
fn non_empty_ranges(regex: &Regex, line: &str) -> Vec<Range<usize>> {
    regex
        .find_iter(line)
        .filter(|found| !found.is_empty())
        .map(|found| found.range())
        .collect()
}

/// Cherche `regex` ligne par ligne dans `files`, dans l'ordre. Le texte d'un chemin de `overrides` (un onglet ouvert)
/// remplace celui du disque ; un fichier illisible, binaire, non UTF-8 ou de plus de 5 Mo est sauté. S'arrête à `limit`
/// lignes (`truncated` quand il en restait au moins une de plus). `stop` est lu avant chaque fichier : levé, on rend ce
/// qu'on a et l'appelant le jette.
pub fn search_files(
    files: &[PathBuf],
    overrides: &HashMap<PathBuf, String>,
    regex: &Regex,
    limit: usize,
    stop: &AtomicBool,
) -> SearchOutcome {
    let mut outcome = SearchOutcome {
        files: Vec::new(),
        truncated: false,
    };
    let mut found = 0;
    for path in files {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let text: Cow<str> = match overrides.get(path) {
            Some(text) => Cow::Borrowed(text),
            None => match read_document(path) {
                Ok(Opened::Text(loaded)) => Cow::Owned(loaded.text),
                _ => continue,
            },
        };
        let mut lines = Vec::new();
        for (index, line) in text_lines(&text).enumerate() {
            let ranges = non_empty_ranges(regex, line);
            if ranges.is_empty() {
                continue;
            }
            if found == limit {
                outcome.truncated = true;
                break;
            }
            found += 1;
            lines.push(LineMatch {
                line: index + 1,
                text: line.to_string(),
                ranges,
            });
        }
        if !lines.is_empty() {
            outcome.files.push(FileMatches {
                path: path.clone(),
                lines,
                line_count: count_lines(&text),
            });
        }
        if outcome.truncated {
            break;
        }
    }
    outcome
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineTarget {
    pub line: usize,
    pub original: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replaced {
    pub text: String,
    pub occurrences: usize,
    pub stale_lines: usize,
}

/// Remplace, dans chaque ligne cible dont le texte est encore celui de la recherche, les occurrences non vides de
/// `regex`. `expand` développe `$1` et `${nom}` (mode expression régulière) ; sinon `replacement` s'écrit tel quel. Une
/// ligne changée ou absente reste, et compte dans `stale_lines`. Le reste du texte est rendu octet pour octet : on ne
/// passe jamais par `replace_all`, qui remplacerait aussi les occurrences vides.
///
/// `expected_lines` est le nombre de lignes que le texte avait à la recherche. S'il en a un autre, des lignes ont été
/// insérées ou retirées : le texte d'une ligne ne prouve plus qu'elle est la même (deux lignes jumelles se décalent
/// sans qu'aucune ne change). Toutes les cibles sont alors périmées et rien n'est écrit.
pub fn replace_lines(
    text: &str,
    targets: &[LineTarget],
    expected_lines: usize,
    regex: &Regex,
    replacement: &str,
    expand: bool,
) -> Replaced {
    let mut lines: Vec<Cow<str>> = text.split('\n').map(Cow::Borrowed).collect();
    // Le segment vide après un `\n` final n'est pas une ligne : une cible qui le vise est périmée.
    let real_lines = lines.len() - usize::from(text.ends_with('\n'));
    if count_lines(text) != expected_lines {
        return Replaced {
            text: text.to_string(),
            occurrences: 0,
            stale_lines: targets.len(),
        };
    }
    let mut occurrences = 0;
    let mut stale_lines = 0;
    for target in targets {
        let index = target.line.wrapping_sub(1);
        if index >= real_lines || lines[index] != target.original.as_str() {
            stale_lines += 1;
            continue;
        }
        let line = lines[index].clone();
        let mut replaced = String::with_capacity(line.len());
        let mut last = 0;
        let mut count = 0;
        for captures in regex.captures_iter(&line) {
            let whole = captures.get(0).expect("group 0 always exists");
            if whole.is_empty() {
                continue;
            }
            replaced.push_str(&line[last..whole.start()]);
            if expand {
                captures.expand(replacement, &mut replaced);
            } else {
                replaced.push_str(replacement);
            }
            last = whole.end();
            count += 1;
        }
        if count > 0 {
            replaced.push_str(&line[last..]);
            lines[index] = Cow::Owned(replaced);
            occurrences += count;
        }
    }
    Replaced {
        text: lines.join("\n"),
        occurrences,
        stale_lines,
    }
}

#[derive(Debug)]
pub enum ReplaceError {
    Changed,
    NotText,
    Io(io::Error),
}

impl ReplaceError {
    /// Le texte d'interface de l'échec.
    pub fn message(&self) -> String {
        match self {
            ReplaceError::Changed => "Le fichier a changé pendant le remplacement.".to_string(),
            ReplaceError::NotText => "Le fichier n'est plus du texte modifiable.".to_string(),
            ReplaceError::Io(error) => explain_write(error).text,
        }
    }

    /// L'étiquette du journal (`reason=…`).
    pub fn reason(&self) -> &'static str {
        match self {
            ReplaceError::Changed => "changed",
            ReplaceError::NotText => "not-text",
            ReplaceError::Io(_) => "io",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileReplace {
    pub occurrences: usize,
    pub stale_lines: usize,
}

pub fn replace_in_file(
    path: &Path,
    targets: &[LineTarget],
    expected_lines: usize,
    regex: &Regex,
    replacement: &str,
    expand: bool,
) -> Result<FileReplace, ReplaceError> {
    // L'empreinte avant la lecture : un agent qui écrit entre la lecture et le `rename` est vu par la garde.
    let print = fingerprint(path).map_err(ReplaceError::Io)?;
    let loaded = match read_document(path).map_err(ReplaceError::Io)? {
        Opened::Text(loaded) => loaded,
        Opened::External => return Err(ReplaceError::NotText),
    };
    let replaced = replace_lines(
        &loaded.text,
        targets,
        expected_lines,
        regex,
        replacement,
        expand,
    );
    let done = FileReplace {
        occurrences: replaced.occurrences,
        stale_lines: replaced.stale_lines,
    };
    if replaced.occurrences == 0 {
        return Ok(done);
    }
    match save_unless_changed(path, &replaced.text, loaded.line_ending, print) {
        Ok(Saved::Written) => Ok(done),
        Ok(Saved::DiskChanged) => Err(ReplaceError::Changed),
        Err(error) => Err(ReplaceError::Io(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files::tabs::fingerprint;
    use crate::project::git_init;
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::sync::atomic::AtomicUsize;

    /// Un dossier jetable propre au test, retiré à la fin.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let dir = std::env::temp_dir().join(format!(
                "idema-search-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn write(&self, name: &str, content: &[u8]) -> PathBuf {
            let path = self.0.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, content).unwrap();
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn git_add(dir: &Path, name: &str) {
        let input = format!(":(literal){name}\0");
        let args = ["add", "-A", "--pathspec-from-file=-", "--pathspec-file-nul"];
        let output = run_git(dir, Gesture::Stage, &args, Some(input.as_bytes())).unwrap();
        assert!(output.success());
    }

    fn regex_of(query: &str, options: SearchOptions) -> Regex {
        compile(query, options).unwrap()
    }

    fn run(
        files: &[PathBuf],
        overrides: &HashMap<PathBuf, String>,
        regex: &Regex,
        limit: usize,
    ) -> SearchOutcome {
        search_files(files, overrides, regex, limit, &AtomicBool::new(false))
    }

    /// Les numéros de ligne trouvés dans un texte donné (cherché par son contenu, comme le ferait un onglet ouvert).
    fn matched_lines(text: &str, query: &str, options: SearchOptions) -> Vec<usize> {
        let path = PathBuf::from("/virtual/a.txt");
        let overrides = HashMap::from([(path.clone(), text.to_string())]);
        let outcome = run(&[path], &overrides, &regex_of(query, options), 1000);
        outcome
            .files
            .iter()
            .flat_map(|file| file.lines.iter().map(|line| line.line))
            .collect()
    }

    fn target(line: usize, original: &str) -> LineTarget {
        LineTarget {
            line,
            original: original.to_string(),
        }
    }

    const REGEX_MODE: SearchOptions = SearchOptions {
        case_sensitive: false,
        whole_word: false,
        regex: true,
    };

    #[test]
    fn project_files_lists_tracked_and_untracked_files_but_not_ignored_ones() {
        let scratch = Scratch::new();
        let dir = scratch.path();
        git_init(dir).unwrap();
        scratch.write(".gitignore", b"*.log\ntarget/\n");
        scratch.write("a.md", b"a");
        scratch.write("d.md", b"d");
        git_add(dir, "a.md");
        git_add(dir, "d.md");
        fs::remove_file(dir.join("d.md")).unwrap();
        scratch.write("b.yml", b"b");
        scratch.write("sub/c.txt", b"c");
        scratch.write("x.log", b"x");
        scratch.write("target/t.o", b"t");
        // Le Finder sème des `.DS_Store` qu'aucun `.gitignore` du projet n'ignore forcément.
        scratch.write(".DS_Store", b"");
        scratch.write("sub/.DS_Store", b"");
        symlink(dir.join("a.md"), dir.join("l.md")).unwrap();

        let files = project_files(dir).unwrap();

        assert_eq!(
            files,
            vec![
                dir.join(".gitignore"),
                dir.join("a.md"),
                dir.join("b.yml"),
                dir.join("sub/c.txt"),
            ]
        );
    }

    #[test]
    fn project_files_fails_outside_a_repository() {
        let scratch = Scratch::new();
        scratch.write("a.md", b"a");

        assert!(project_files(scratch.path()).is_err());
    }

    #[test]
    fn the_default_search_is_case_insensitive_and_literal() {
        let text = "Foo\nfoo\nf.o\nfao\n";

        assert_eq!(matched_lines(text, "foo", SearchOptions::default()), [1, 2]);
        assert_eq!(matched_lines(text, "f.o", SearchOptions::default()), [3]);
        assert_eq!(matched_lines(text, "f.o", REGEX_MODE), [1, 2, 3, 4]);
    }

    #[test]
    fn case_and_whole_word_narrow_the_matches() {
        let text = "foo\nFoo\nfood\n";
        let sensitive = SearchOptions {
            case_sensitive: true,
            ..SearchOptions::default()
        };
        let word = SearchOptions {
            whole_word: true,
            ..SearchOptions::default()
        };

        assert_eq!(matched_lines(text, "foo", sensitive), [1, 3]);
        assert_eq!(matched_lines(text, "foo", word), [1, 2]);
    }

    #[test]
    fn case_insensitive_search_folds_accented_letters() {
        assert_eq!(
            matched_lines("Été\nété\n", "été", SearchOptions::default()),
            [1, 2]
        );
    }

    #[test]
    fn an_invalid_regex_is_reported() {
        assert!(matches!(
            compile("(", REGEX_MODE),
            Err(QueryError::InvalidRegex)
        ));
        assert!(compile("(", SearchOptions::default()).is_ok());
        // Entouré pour le mot entier, un motif déséquilibré ne doit pas devenir valide.
        let word_regex = SearchOptions {
            whole_word: true,
            ..REGEX_MODE
        };
        assert!(matches!(
            compile("a)|(b", word_regex),
            Err(QueryError::InvalidRegex)
        ));
    }

    #[test]
    fn match_ranges_are_byte_ranges_of_the_line() {
        let path = PathBuf::from("/virtual/a.txt");
        let overrides = HashMap::from([(path.clone(), "café foo\nfoo x foo\n".to_string())]);

        let outcome = run(
            &[path],
            &overrides,
            &regex_of("foo", SearchOptions::default()),
            1000,
        );

        let lines = &outcome.files[0].lines;
        assert_eq!(lines[0].text, "café foo");
        assert_eq!(lines[0].ranges.as_slice(), &[Range { start: 6, end: 9 }]);
        assert_eq!(lines[1].text, "foo x foo");
        assert_eq!(lines[1].ranges, [0..3, 6..9]);
    }

    #[test]
    fn binary_and_non_utf8_files_are_skipped() {
        let scratch = Scratch::new();
        let mut non_utf8 = vec![0xff, 0xfe];
        non_utf8.extend_from_slice(b"foo");
        let files = [
            scratch.write("bin.dat", b"foo\0bar"),
            scratch.write("latin.txt", &non_utf8),
        ];

        let outcome = run(
            &files,
            &HashMap::new(),
            &regex_of("foo", SearchOptions::default()),
            1000,
        );

        assert!(outcome.files.is_empty());
        assert!(!outcome.truncated);
    }

    #[test]
    fn an_open_tab_is_searched_in_its_text_not_on_disk() {
        let scratch = Scratch::new();
        let tab = scratch.write("tab.txt", b"a\n");
        let other = scratch.write("other.txt", b"foo\n");
        let overrides = HashMap::from([(tab.clone(), "foo\n".to_string())]);

        let outcome = run(
            &[tab.clone(), other.clone()],
            &overrides,
            &regex_of("foo", SearchOptions::default()),
            1000,
        );

        let paths: Vec<&PathBuf> = outcome.files.iter().map(|file| &file.path).collect();
        assert_eq!(paths, [&tab, &other]);
        assert_eq!(outcome.line_count(), 2);
    }

    #[test]
    fn a_search_stops_at_the_line_limit_and_says_so() {
        let scratch = Scratch::new();
        let files: Vec<PathBuf> = ["a.txt", "b.txt", "c.txt"]
            .iter()
            .map(|name| scratch.write(name, b"foo\nfoo\n"))
            .collect();
        let regex = regex_of("foo", SearchOptions::default());

        let capped = run(&files, &HashMap::new(), &regex, 4);
        assert_eq!(capped.line_count(), 4);
        assert!(capped.truncated);

        let exact = run(&files, &HashMap::new(), &regex, 6);
        assert_eq!(exact.line_count(), 6);
        assert!(!exact.truncated);
    }

    #[test]
    fn a_raised_stop_flag_returns_what_was_found_without_reading_more() {
        let scratch = Scratch::new();
        let files = [scratch.write("a.txt", b"foo\n")];
        let stop = AtomicBool::new(true);

        let outcome = search_files(
            &files,
            &HashMap::new(),
            &regex_of("foo", SearchOptions::default()),
            1000,
            &stop,
        );

        assert!(outcome.files.is_empty());
    }

    #[test]
    fn an_empty_match_is_neither_listed_nor_replaced() {
        let regex = regex_of("x*", REGEX_MODE);

        assert!(matched_lines("abc\n", "x*", REGEX_MODE).is_empty());

        let replaced = replace_lines("abc\n", &[target(1, "abc")], 1, &regex, "Z", false);
        assert_eq!(replaced.text, "abc\n");
        assert_eq!(replaced.occurrences, 0);
        assert_eq!(replaced.stale_lines, 0);
    }

    #[test]
    fn a_line_with_empty_and_real_matches_replaces_only_the_real_ones() {
        let regex = regex_of("x*", REGEX_MODE);

        let replaced = replace_lines("axxb\n", &[target(1, "axxb")], 1, &regex, "-", false);

        assert_eq!(replaced.text, "a-b\n");
        assert_eq!(replaced.occurrences, 1);
    }

    #[test]
    fn replace_lines_changes_only_the_checked_lines() {
        let text = "foo un\ndeux foo foo\ntrois foo\n";
        let regex = regex_of("foo", SearchOptions::default());

        let two = replace_lines(
            text,
            &[target(1, "foo un"), target(3, "trois foo")],
            3,
            &regex,
            "bar",
            false,
        );
        assert_eq!(two.text, "bar un\ndeux foo foo\ntrois bar\n");
        assert_eq!(two.occurrences, 2);
        assert_eq!(two.stale_lines, 0);

        let middle = replace_lines(text, &[target(2, "deux foo foo")], 3, &regex, "bar", false);
        assert_eq!(middle.text, "foo un\ndeux bar bar\ntrois foo\n");
        assert_eq!(middle.occurrences, 2);
    }

    #[test]
    fn a_line_changed_since_the_search_is_not_replaced() {
        let regex = regex_of("foo", SearchOptions::default());

        let changed = replace_lines(
            "foo deux\n",
            &[target(1, "foo un")],
            1,
            &regex,
            "bar",
            false,
        );
        assert_eq!(changed.text, "foo deux\n");
        assert_eq!(changed.stale_lines, 1);
        assert_eq!(changed.occurrences, 0);

        // Une ligne au-delà de la dernière compte aussi comme changée (le segment après le dernier `\n` n'en est pas une).
        let beyond = replace_lines("foo\n", &[target(2, "foo")], 1, &regex, "bar", false);
        assert_eq!(beyond.text, "foo\n");
        assert_eq!(beyond.stale_lines, 1);
    }

    #[test]
    fn a_plain_replacement_keeps_dollar_signs_and_a_regex_one_expands_groups() {
        let plain = replace_lines(
            "le prix\n",
            &[target(1, "le prix")],
            1,
            &regex_of("prix", SearchOptions::default()),
            "$1",
            false,
        );
        assert_eq!(plain.text, "le $1\n");

        let grouped = replace_lines(
            "alice@\n",
            &[target(1, "alice@")],
            1,
            &regex_of(r"(\w+)@", REGEX_MODE),
            "$1 at",
            true,
        );
        assert_eq!(grouped.text, "alice at\n");
        assert_eq!(grouped.occurrences, 1);
    }

    #[test]
    fn replacing_in_a_crlf_file_keeps_crlf() {
        let scratch = Scratch::new();
        let path = scratch.write("crlf.txt", b"foo\r\nbar\r\n");

        let done = replace_in_file(
            &path,
            &[target(1, "foo")],
            2,
            &regex_of("foo", SearchOptions::default()),
            "baz",
            false,
        )
        .unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"baz\r\nbar\r\n");
        assert_eq!(
            done,
            FileReplace {
                occurrences: 1,
                stale_lines: 0
            }
        );
    }

    #[test]
    fn replace_in_file_writes_nothing_when_nothing_matches() {
        let scratch = Scratch::new();
        let path = scratch.write("a.txt", b"abc\n");
        let before = fingerprint(&path).unwrap();

        let done = replace_in_file(
            &path,
            &[target(1, "abc")],
            1,
            &regex_of("zzz", SearchOptions::default()),
            "y",
            false,
        )
        .unwrap();

        assert_eq!(
            done,
            FileReplace {
                occurrences: 0,
                stale_lines: 0
            }
        );
        assert_eq!(fingerprint(&path).unwrap(), before);
        assert_eq!(fs::read(&path).unwrap(), b"abc\n");
    }

    #[test]
    fn replace_in_file_skips_stale_lines_and_reports_them() {
        let scratch = Scratch::new();
        let path = scratch.write("a.txt", b"foo a\nfoo b\n");

        let done = replace_in_file(
            &path,
            &[target(1, "foo a"), target(2, "foo OLD")],
            2,
            &regex_of("foo", SearchOptions::default()),
            "bar",
            false,
        )
        .unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"bar a\nfoo b\n");
        assert_eq!(
            done,
            FileReplace {
                occurrences: 1,
                stale_lines: 1
            }
        );
    }

    #[test]
    fn a_search_records_the_line_count_of_each_file_it_found() {
        let scratch = Scratch::new();
        let disk = scratch.write("disk.txt", b"foo\nb\nc\n");
        let tab = scratch.write("tab.txt", b"x\n");
        let overrides = HashMap::from([(tab.clone(), "foo\nb".to_string())]);

        let outcome = run(
            &[disk, tab],
            &overrides,
            &regex_of("foo", SearchOptions::default()),
            1000,
        );

        let counts: Vec<usize> = outcome.files.iter().map(|file| file.line_count).collect();
        assert_eq!(counts, [3, 2]);
    }

    #[test]
    fn a_line_count_that_changed_since_the_search_makes_every_target_stale() {
        // Recherche sur `items:\n- foo\n- foo\n` : ligne 3 décochée, ligne 2 cochée. Un agent retire la ligne 1 : la
        // ligne 2 porte maintenant le texte de l'ancienne ligne 3, la jumelle décochée. Le texte seul ne le dit pas.
        let regex = regex_of("foo", SearchOptions::default());
        let after_deletion = "- foo\n- foo\n";

        let replaced = replace_lines(
            after_deletion,
            &[target(2, "- foo")],
            3,
            &regex,
            "bar",
            false,
        );

        assert_eq!(replaced.text, after_deletion);
        assert_eq!(replaced.occurrences, 0);
        assert_eq!(replaced.stale_lines, 1);

        // Le même texte, avec le nombre de lignes de la recherche : la cible passe.
        let unchanged = replace_lines(
            after_deletion,
            &[target(2, "- foo")],
            2,
            &regex,
            "bar",
            false,
        );
        assert_eq!(unchanged.text, "- foo\n- bar\n");
        assert_eq!(unchanged.stale_lines, 0);
    }

    #[test]
    fn replace_in_file_writes_nothing_when_the_line_count_changed() {
        let scratch = Scratch::new();
        let path = scratch.write("twins.txt", b"- foo\n- foo\n");
        let before = fingerprint(&path).unwrap();

        let done = replace_in_file(
            &path,
            &[target(2, "- foo")],
            3,
            &regex_of("foo", SearchOptions::default()),
            "bar",
            false,
        )
        .unwrap();

        assert_eq!(
            done,
            FileReplace {
                occurrences: 0,
                stale_lines: 1
            }
        );
        assert_eq!(fs::read(&path).unwrap(), b"- foo\n- foo\n");
        assert_eq!(fingerprint(&path).unwrap(), before);
    }

    #[test]
    fn replace_errors_say_what_happened() {
        let scratch = Scratch::new();
        let binary = scratch.write("bin.dat", b"foo\0bar");
        let regex = regex_of("foo", SearchOptions::default());

        let not_text = replace_in_file(&binary, &[target(1, "foo")], 1, &regex, "x", false);
        assert!(matches!(not_text, Err(ReplaceError::NotText)));

        let missing = replace_in_file(
            &scratch.path().join("gone.txt"),
            &[target(1, "foo")],
            1,
            &regex,
            "x",
            false,
        );
        let Err(error) = missing else {
            panic!("a missing file must not be replaced");
        };
        assert_eq!(error.reason(), "io");

        assert_eq!(ReplaceError::Changed.reason(), "changed");
        assert_eq!(
            ReplaceError::Changed.message(),
            "Le fichier a changé pendant le remplacement."
        );
        assert_eq!(ReplaceError::NotText.reason(), "not-text");
        assert_eq!(
            ReplaceError::NotText.message(),
            "Le fichier n'est plus du texte modifiable."
        );
    }
}
