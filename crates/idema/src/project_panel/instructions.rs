//! Les instructions du projet (onglet « Instructions » du volet Projet, spec §3.2) : Claude Code ignore `AGENTS.md`
//! dès qu'un `CLAUDE.md` ou un `CLAUDE.local.md` existe (sauf si l'un d'eux l'importe), alors que les quatre autres
//! harnais ne lisent que `AGENTS.md`. Ce module classe les `CLAUDE.md` du projet, calcule le texte fusionné d'`AGENTS.md`
//! et applique la fusion : écrire `AGENTS.md`, puis mettre chaque `CLAUDE.md` à la Corbeille.
//!
//! La fusion écrit de vrais fichiers d'instructions : elle relit toujours le disque, n'écrase jamais un fichier qui a
//! bougé depuis cette lecture et ne met rien à la Corbeille tant que l'écriture n'a pas eu lieu.

use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use super::{is_home, resolves_into_global_config};
use crate::files::document::{LineEnding, Opened, Saved, read_document, save_unless_changed};
use crate::files::tabs::{Fingerprint, fingerprint};

const AGENTS_FILE: &str = "AGENTS.md";
const CLAUDE_FILE: &str = "CLAUDE.md";
const CLAUDE_LOCAL_FILE: &str = "CLAUDE.local.md";
const CLAUDE_DIR: &str = ".claude";

/// La forme d'un `CLAUDE.md` vis-à-vis d'`AGENTS.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaudeMdKind {
    /// Un lien vers `AGENTS.md`, ou un contenu réduit à ses imports : Claude lit `AGENTS.md`, rien à signaler.
    Bridge,
    /// Importe `AGENTS.md` puis ajoute des règles que seul Claude voit.
    ExtendsAgents,
    /// Tout autre contenu (copie identique comprise) : Claude ignore `AGENTS.md`.
    Shadows,
}

/// Une ligne de texte, avec son appartenance à un bloc de code (les lignes de clôture en font partie).
struct Line<'a> {
    text: &'a str,
    in_code: bool,
}

/// Découpe `text` en lignes et marque celles d'un bloc de code clôturé (``` ou ~~~). Un bloc jamais refermé court
/// jusqu'à la fin.
fn scan_lines(text: &str) -> Vec<Line<'_>> {
    let mut lines = Vec::new();
    // Le caractère et la longueur de la clôture ouverte.
    let mut fence: Option<(char, usize)> = None;
    for raw in text.lines() {
        let trimmed = raw.trim_start();
        let marker = trimmed
            .chars()
            .next()
            .filter(|c| matches!(c, '`' | '~'))
            .map(|c| (c, trimmed.chars().take_while(|x| *x == c).count()))
            .filter(|(_, run)| *run >= 3);
        match (fence, marker) {
            (None, Some(open)) => {
                fence = Some(open);
                lines.push(Line {
                    text: raw,
                    in_code: true,
                });
            }
            (Some((c, len)), Some((mc, run))) if mc == c && run >= len => {
                // Une clôture de fermeture ne porte rien d'autre que ses marqueurs.
                let only_markers = trimmed.trim_end().chars().all(|x| x == c);
                lines.push(Line {
                    text: raw,
                    in_code: true,
                });
                if only_markers {
                    fence = None;
                }
            }
            (Some(_), _) => lines.push(Line {
                text: raw,
                in_code: true,
            }),
            (None, None) => lines.push(Line {
                text: raw,
                in_code: false,
            }),
        }
    }
    lines
}

/// `line` sans ses `code` en ligne (un `` `x` `` ou un ``` ``x`` ``` ; un guillemet inverse jamais refermé reste
/// littéral). Les passages retirés laissent une espace : ils séparent les mots.
fn strip_code_spans(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '`' {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        let open = chars[i..].iter().take_while(|c| **c == '`').count();
        let mut j = i + open;
        let mut close = None;
        while j < chars.len() {
            if chars[j] == '`' {
                let run = chars[j..].iter().take_while(|c| **c == '`').count();
                if run == open {
                    close = Some(j + run);
                    break;
                }
                j += run;
            } else {
                j += 1;
            }
        }
        match close {
            Some(end) => {
                out.push(' ');
                i = end;
            }
            None => {
                out.extend(std::iter::repeat_n('`', open));
                i += open;
            }
        }
    }
    out
}

/// Les mots de `line` : séparés par des espaces, sauf précédées d'une barre oblique inverse (chemin avec espaces).
fn split_words(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' && chars.peek().is_some_and(|next| next.is_whitespace()) {
            current.push('\\');
            current.push(chars.next().unwrap_or(' '));
        } else if c.is_whitespace() {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
        } else {
            current.push(c);
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

/// Les imports `@chemin` d'une ligne hors bloc de code : les mots qui commencent par `@` suivi d'au moins un
/// caractère (`alice@example.com` n'en est pas un), hors `code` en ligne. Le chemin rendu a perdu son `@` et ses
/// barres obliques inverses d'échappement.
fn import_paths(line: &str) -> Vec<String> {
    split_words(&strip_code_spans(line))
        .into_iter()
        .filter_map(|word| {
            let path = word.strip_prefix('@')?;
            (!path.is_empty()).then(|| path.replace("\\ ", " "))
        })
        .collect()
}

/// Résout les `.` et `..` d'un chemin sans toucher au disque (le fichier visé peut ne pas exister).
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Vrai quand l'import `import` (un chemin sans `@`), porté par un fichier du dossier `dir`, vise `<projet>/AGENTS.md`.
fn resolves_to_agents(import: &str, dir: &Path, project: &Path) -> bool {
    normalize(&dir.join(import)) == normalize(&project.join(AGENTS_FILE))
}

/// Vrai quand la ligne est un seul import (`@chemin`, espaces autour) dont le chemin, résolu depuis `dir` (le dossier du
/// fichier qui la porte), est `<projet>/AGENTS.md` : `@AGENTS.md` à la racine, `@../AGENTS.md` dans `.claude/`.
pub fn imports_agents(line: &str, dir: &Path, project: &Path) -> bool {
    let words = split_words(line);
    let [word] = words.as_slice() else {
        return false;
    };
    word.strip_prefix('@').is_some_and(|path| {
        !path.is_empty() && resolves_to_agents(&path.replace("\\ ", " "), dir, project)
    })
}

/// Vrai quand un mot de la ligne, hors `code` en ligne, importe `<projet>/AGENTS.md`.
fn mentions_agents_import(line: &str, dir: &Path, project: &Path) -> bool {
    import_paths(line)
        .iter()
        .any(|path| resolves_to_agents(path, dir, project))
}

/// Classe un `CLAUDE.md` (`dir` : son dossier ; `links_to_agents` : c'est un lien vers `<projet>/AGENTS.md`).
pub fn classify(text: &str, dir: &Path, project: &Path, links_to_agents: bool) -> ClaudeMdKind {
    if links_to_agents {
        return ClaudeMdKind::Bridge;
    }
    let lines = scan_lines(text);
    let content: Vec<&Line> = lines
        .iter()
        .filter(|line| !line.text.trim().is_empty())
        .collect();
    if !content.is_empty()
        && content
            .iter()
            .all(|line| !line.in_code && imports_agents(line.text, dir, project))
    {
        return ClaudeMdKind::Bridge;
    }
    if lines
        .iter()
        .any(|line| !line.in_code && mentions_agents_import(line.text, dir, project))
    {
        ClaudeMdKind::ExtendsAgents
    } else {
        ClaudeMdKind::Shadows
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeMd {
    pub path: PathBuf,
    pub text: String,
    pub kind: ClaudeMdKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstructionFiles {
    pub agents: Option<String>,
    /// `CLAUDE.md` puis `.claude/CLAUDE.md`, ceux qui existent.
    pub claude: Vec<ClaudeMd>,
    pub local: Option<String>,
}

/// Les `CLAUDE.md` du projet, dans l'ordre de fusion : la racine, puis `.claude/`.
fn claude_paths(project: &Path) -> [PathBuf; 2] {
    [
        project.join(CLAUDE_FILE),
        project.join(CLAUDE_DIR).join(CLAUDE_FILE),
    ]
}

/// Un fichier d'instructions qui n'est pas un texte éditable. L'erreur reste en anglais (c'est une ligne de journal) ;
/// `io_error_text` en fait la phrase française que le volet montre.
#[derive(Debug)]
pub(super) struct NotEditableText(pub(super) PathBuf);

impl std::fmt::Display for NotEditableText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} is not editable text", self.0.display())
    }
}

impl std::error::Error for NotEditableText {}

/// Ce que le volet dit d'une erreur d'entrée-sortie : toujours en français, jamais le texte du système (anglais) ni
/// celui d'une erreur interne. Le texte brut reste dans le journal.
pub fn io_error_text(error: &io::Error) -> String {
    if let Some(NotEditableText(path)) = error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<NotEditableText>())
    {
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        return format!("{name} n'est pas un texte modifiable");
    }
    match error.kind() {
        io::ErrorKind::NotFound => "fichier introuvable",
        io::ErrorKind::PermissionDenied => "accès refusé",
        io::ErrorKind::StorageFull => "disque plein",
        io::ErrorKind::ReadOnlyFilesystem => "disque en lecture seule",
        io::ErrorKind::IsADirectory => "c'est un dossier",
        io::ErrorKind::NotADirectory => "ce n'est pas un dossier",
        io::ErrorKind::InvalidData => "contenu illisible",
        _ => "erreur du système",
    }
    .to_string()
}

/// Le texte d'un fichier d'instructions, `None` quand il n'existe pas. Un fichier qui n'est pas du texte éditable
/// (binaire, dossier, trop gros) est une erreur : on ne fusionne pas ce qu'on ne sait pas lire.
fn read_text(path: &Path) -> io::Result<Option<(String, LineEnding)>> {
    match read_document(path) {
        Ok(Opened::Text(loaded)) => Ok(Some((loaded.text, loaded.line_ending))),
        Ok(Opened::External) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            NotEditableText(path.to_path_buf()),
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Vrai quand `path` et `<projet>/AGENTS.md` sont le même fichier (même `dev` et même `ino`, liens suivis) : un lien
/// de `CLAUDE.md` vers `AGENTS.md`, l'inverse, ou un lien physique. Un tel `CLAUDE.md` n'a pas de texte à lui : il ne
/// se fusionne pas et ne part pas à la Corbeille (ce serait détruire `AGENTS.md`, ou son contenu).
fn is_agents_file(path: &Path, project: &Path) -> bool {
    match (fs::metadata(path), fs::metadata(project.join(AGENTS_FILE))) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

/// `read`, avec la fin de ligne d'`AGENTS.md` pour que l'écriture la conserve.
fn read_with_ending(project: &Path) -> io::Result<(InstructionFiles, LineEnding)> {
    let (agents, ending) = match read_text(&project.join(AGENTS_FILE))? {
        Some((text, ending)) => (Some(text), ending),
        None => (None, LineEnding::Lf),
    };
    let mut claude = Vec::new();
    for path in claude_paths(project) {
        let Some((text, _)) = read_text(&path)? else {
            continue;
        };
        let dir = path.parent().unwrap_or(project);
        let kind = classify(&text, dir, project, is_agents_file(&path, project));
        claude.push(ClaudeMd { path, text, kind });
    }
    let local = read_text(&project.join(CLAUDE_LOCAL_FILE))?.map(|(text, _)| text);
    Ok((
        InstructionFiles {
            agents,
            claude,
            local,
        },
        ending,
    ))
}

/// Lit `AGENTS.md`, les `CLAUDE.md` (racine puis `.claude/`) et `CLAUDE.local.md` du projet. Un fichier absent est
/// `None` ou omis ; un fichier illisible ou qui n'est pas du texte est une erreur.
pub fn read(project: &Path) -> io::Result<InstructionFiles> {
    read_with_ending(project).map(|(files, _)| files)
}

/// Pourquoi la fusion n'a pas lieu. Le volet grise le bouton avec `message()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeBlocked {
    /// Un `CLAUDE.md` importe d'autres fichiers : fusion à faire à la main.
    OtherImports,
    /// Un `CLAUDE.md` cite `@AGENTS.md` au milieu d'une phrase : une fois dans `AGENTS.md`, il s'importerait lui-même.
    InlineAgentsImport,
}

impl MergeBlocked {
    /// Le libellé du volet (spec §3.2), sans tiret cadratin.
    pub fn message(self) -> &'static str {
        match self {
            MergeBlocked::OtherImports => {
                "Ce CLAUDE.md importe d'autres fichiers : fusion à faire à la main."
            }
            MergeBlocked::InlineAgentsImport => {
                "Ce CLAUDE.md cite @AGENTS.md dans une phrase : fusion à faire à la main."
            }
        }
    }
}

/// Le prédicat unique du blocage, celui que le volet utilise pour griser le bouton et que `merge_text` applique :
/// `Some` quand ce `CLAUDE.md` ne peut pas se fusionner tel quel. Un `Bridge` n'apporte rien et ne bloque jamais. Les
/// lignes qui sont à elles seules un import d'`AGENTS.md` s'effacent à la fusion ; tout autre import hors code bloque
/// (`OtherImports` avant `InlineAgentsImport` quand les deux se présentent).
pub fn merge_blocked(claude: &ClaudeMd, project: &Path) -> Option<MergeBlocked> {
    if claude.kind == ClaudeMdKind::Bridge {
        return None;
    }
    let dir = claude.path.parent().unwrap_or(project);
    let mut inline_agents = false;
    for line in scan_lines(&claude.text) {
        if line.in_code || imports_agents(line.text, dir, project) {
            continue;
        }
        for path in import_paths(line.text) {
            if resolves_to_agents(&path, dir, project) {
                inline_agents = true;
            } else {
                return Some(MergeBlocked::OtherImports);
            }
        }
    }
    inline_agents.then_some(MergeBlocked::InlineAgentsImport)
}

/// Le texte d'un `CLAUDE.md` à fusionner : sans les lignes qui importent `AGENTS.md`, sans lignes vides en tête et en
/// queue. À n'appeler que si `merge_blocked` rend `None`.
fn mergeable_block(claude: &ClaudeMd, project: &Path) -> String {
    let dir = claude.path.parent().unwrap_or(project);
    let kept: Vec<&str> = scan_lines(&claude.text)
        .into_iter()
        .filter(|line| line.in_code || !imports_agents(line.text, dir, project))
        .map(|line| line.text)
        .collect();
    let start = kept
        .iter()
        .position(|line| !line.trim().is_empty())
        .unwrap_or(kept.len());
    let end = kept
        .iter()
        .rposition(|line| !line.trim().is_empty())
        .map_or(start, |last| last + 1);
    kept[start..end].join("\n")
}

/// Vrai quand toutes les lignes de `block` figurent, à la suite et telles quelles (hors espaces de fin de ligne),
/// dans `haystack`.
fn already_contains(haystack: &str, block: &str) -> bool {
    let hay: Vec<&str> = haystack.lines().map(str::trim_end).collect();
    let needle: Vec<&str> = block.lines().map(str::trim_end).collect();
    if needle.len() > hay.len() {
        return false;
    }
    hay.windows(needle.len()).any(|window| window == needle)
}

/// Le texte complet du nouvel `AGENTS.md` : celui d'`AGENTS.md` suivi de ce que chaque `CLAUDE.md` (dans l'ordre reçu)
/// apporte de nouveau, après une ligne vide. `None` quand il n'y a rien à ajouter. Un `Bridge` (lien, import seul)
/// n'apporte rien. `project` est la racine du projet : elle sert à résoudre les imports (`@../AGENTS.md` dans
/// `.claude/`), jamais devinée depuis le chemin du fichier.
pub fn merge_text(
    agents: Option<&str>,
    claude: &[ClaudeMd],
    project: &Path,
) -> Result<Option<String>, MergeBlocked> {
    let mut merged = agents.unwrap_or("").to_string();
    let mut changed = false;
    for file in claude.iter().filter(|f| f.kind != ClaudeMdKind::Bridge) {
        if let Some(blocked) = merge_blocked(file, project) {
            return Err(blocked);
        }
        let block = mergeable_block(file, project);
        if block.is_empty() || already_contains(&merged, &block) {
            continue;
        }
        // Seules les fins de ligne de queue partent : les espaces de la dernière ligne d'AGENTS.md restent.
        let head = merged.trim_end_matches(['\n', '\r']);
        merged = if head.trim().is_empty() {
            format!("{block}\n")
        } else {
            format!("{head}\n\n{block}\n")
        };
        changed = true;
    }
    Ok(changed.then_some(merged))
}

#[derive(Debug)]
pub enum MergeError {
    Blocked(MergeBlocked),
    /// Un fichier a changé sur le disque entre la lecture et l'écriture : rien n'est écrit ni mis à la Corbeille.
    Changed,
    /// Le projet est `~/.claude` : son `CLAUDE.md` est la mémoire globale de l'utilisateur, jamais fusionnée.
    UserMemory,
    /// Le projet est le dossier personnel : son `~/.claude/CLAUDE.md` est la mémoire globale de l'utilisateur, jamais
    /// fusionnée dans un `AGENTS.md` du dossier personnel.
    Home,
    /// Un fichier du projet mène, par un lien de dossier (`.claude` vers `~/.claude`), à une configuration globale : le
    /// fusionner ou le mettre à la Corbeille toucherait celle de toute la machine. Le nom est relatif au projet.
    GlobalConfig(String),
    Io(io::Error),
}

impl std::fmt::Display for MergeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MergeError::Blocked(blocked) => write!(f, "{}", blocked.message()),
            MergeError::Changed => {
                write!(f, "an instruction file changed on disk during the merge")
            }
            MergeError::UserMemory => write!(f, "the user memory in ~/.claude is never merged"),
            MergeError::Home => write!(f, "the home folder is not a project"),
            MergeError::GlobalConfig(file) => {
                write!(f, "{file} resolves into a global configuration")
            }
            MergeError::Io(error) => write!(f, "{error}"),
        }
    }
}

impl From<io::Error> for MergeError {
    fn from(error: io::Error) -> Self {
        MergeError::Io(error)
    }
}

/// Vrai quand `project` est `<home>/.claude` (chemins canoniques) : son `CLAUDE.md` est la mémoire globale de
/// l'utilisateur, chargée dans toutes ses sessions. Le volet n'y propose pas de fusion.
pub fn is_user_claude_home(project: &Path, home: &Path) -> bool {
    match (
        fs::canonicalize(project),
        fs::canonicalize(home.join(CLAUDE_DIR)),
    ) {
        (Ok(project), Ok(user)) => project == user,
        _ => false,
    }
}

/// Fusionne les `CLAUDE.md` dans `AGENTS.md` : relit le disque, écrit `AGENTS.md` si besoin, puis met chaque
/// `CLAUDE.md` à la Corbeille (par `trash`, qui rend le chemin d'arrivée) et rend leur nombre. Un `CLAUDE.md` qui est
/// le même fichier qu'`AGENTS.md` (lien dans un sens ou l'autre, lien physique) n'est jamais mis à la Corbeille.
///
/// Garde-fous : `Home` pour le dossier personnel, `UserMemory` pour `~/.claude` ; l'écriture refuse de remplacer un
/// `AGENTS.md` qui a changé (ou qui est apparu) depuis la lecture, et un `CLAUDE.md` modifié depuis la lecture n'est ni
/// fusionné ni mis à la Corbeille. Dans ces cas, `Changed`, rien d'écrit et rien à la Corbeille. Si l'écriture échoue,
/// rien non plus.
pub fn apply_merge(
    project: &Path,
    trash: &mut dyn FnMut(&Path) -> io::Result<PathBuf>,
) -> Result<usize, MergeError> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    apply_merge_with(project, home.as_deref(), trash, &mut || {})
}

/// Vrai quand aucun des `CLAUDE.md` lus n'a bougé depuis son empreinte. Un `CLAUDE.md` qui est le même fichier
/// qu'`AGENTS.md` suit `AGENTS.md` : son empreinte bouge légitimement à l'écriture.
fn claude_unchanged(
    files: &[ClaudeMd],
    recorded: &[(PathBuf, Option<Fingerprint>)],
    project: &Path,
) -> io::Result<bool> {
    for file in files {
        if is_agents_file(&file.path, project) {
            continue;
        }
        let before = recorded
            .iter()
            .find(|(path, _)| *path == file.path)
            .and_then(|(_, print)| *print);
        if fingerprint(&file.path)? != before {
            return Ok(false);
        }
    }
    Ok(true)
}

/// `apply_merge`, avec le dossier personnel et un crochet appelé entre la lecture et l'écriture : c'est là que les
/// tests font bouger le disque.
fn apply_merge_with(
    project: &Path,
    home: Option<&Path>,
    trash: &mut dyn FnMut(&Path) -> io::Result<PathBuf>,
    after_read: &mut dyn FnMut(),
) -> Result<usize, MergeError> {
    // Jamais de création du dossier du projet : s'il manque, c'est une erreur.
    if !project.is_dir() {
        return Err(MergeError::Io(io::Error::new(
            io::ErrorKind::NotFound,
            format!("{} is not a directory", project.display()),
        )));
    }
    if is_home(project, home) {
        return Err(MergeError::Home);
    }
    if home.is_some_and(|home| is_user_claude_home(project, home)) {
        return Err(MergeError::UserMemory);
    }
    let agents_path = project.join(AGENTS_FILE);
    // Un dossier du projet lié à une config globale : `.claude/CLAUDE.md` serait la mémoire de toute la machine. Un
    // `CLAUDE.md` qui n'est lui-même qu'un lien est mis à la Corbeille comme lien : seul son dossier compte.
    let global = |path: &Path, whole: bool| {
        let probe = if whole { Some(path) } else { path.parent() };
        probe.is_some_and(|probe| resolves_into_global_config(probe, home))
    };
    let guarded = std::iter::once((agents_path.clone(), true))
        .chain(claude_paths(project).into_iter().map(|path| (path, false)));
    for (path, whole) in guarded {
        if global(&path, whole) {
            let name = path.strip_prefix(project).unwrap_or(&path);
            return Err(MergeError::GlobalConfig(name.display().to_string()));
        }
    }
    // Les empreintes d'abord, la lecture ensuite : un fichier qui bouge entre les deux rend l'empreinte plus ancienne
    // que le texte, donc un refus d'écrire, jamais un écrasement.
    let agents_print = fingerprint(&agents_path)?;
    let mut claude_prints: Vec<(PathBuf, Option<Fingerprint>)> = Vec::new();
    for path in claude_paths(project) {
        let print = fingerprint(&path)?;
        claude_prints.push((path, print));
    }
    let (files, ending) = read_with_ending(project)?;
    after_read();

    let merged =
        merge_text(files.agents.as_deref(), &files.claude, project).map_err(MergeError::Blocked)?;
    // Un CLAUDE.md enregistré depuis la lecture porte du texte que la fusion n'a pas vu : on ne touche à rien, ni à
    // AGENTS.md (une reprise dupliquerait le bloc) ni à la Corbeille.
    if !claude_unchanged(&files.claude, &claude_prints, project)? {
        return Err(MergeError::Changed);
    }
    match merged {
        Some(text) => match save_unless_changed(&agents_path, &text, ending, agents_print)? {
            Saved::Written => {}
            Saved::DiskChanged => return Err(MergeError::Changed),
        },
        // Rien à écrire : ce qui figure dans AGENTS.md doit encore y figurer au moment de jeter les CLAUDE.md.
        None if fingerprint(&agents_path)? != agents_print => return Err(MergeError::Changed),
        None => {}
    }
    // Une seconde fois avant la Corbeille : la fenêtre entre la première vérification et l'écriture est la seule
    // qui reste.
    if !claude_unchanged(&files.claude, &claude_prints, project)? {
        return Err(MergeError::Changed);
    }

    let mut trashed = 0;
    for file in files
        .claude
        .iter()
        .filter(|file| !is_agents_file(&file.path, project))
    {
        trash(&file.path)?;
        trashed += 1;
    }
    Ok(trashed)
}

/// Vrai quand `CLAUDE.local.md` (à la racine) n'importe pas déjà `AGENTS.md` : le volet propose alors de l'y importer.
pub fn local_needs_import(text: &str) -> bool {
    let root = Path::new("/");
    !scan_lines(text)
        .iter()
        .any(|line| !line.in_code && mentions_agents_import(line.text, root, root))
}

/// `text` précédé de l'import d'`AGENTS.md` (`@AGENTS.md`, une ligne en tête).
pub fn with_agents_import(text: &str) -> String {
    format!("@{AGENTS_FILE}\n{text}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Un projet jetable sous le dossier temporaire, retiré à la fin ; la « Corbeille » des tests y vit aussi.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let root = std::env::temp_dir().join(format!(
                "idema-instructions-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(root.join("project")).unwrap();
            fs::create_dir_all(root.join("trash")).unwrap();
            Scratch(root)
        }

        fn project(&self) -> PathBuf {
            self.0.join("project")
        }

        fn write(&self, relative: &str, text: &str) {
            let path = self.project().join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }

        fn read(&self, relative: &str) -> String {
            fs::read_to_string(self.project().join(relative)).unwrap()
        }

        fn exists(&self, relative: &str) -> bool {
            fs::symlink_metadata(self.project().join(relative)).is_ok()
        }

        /// La fausse Corbeille : déplace dans `trash/` et note ce qu'elle a reçu.
        fn trash_into<'a>(
            &'a self,
            received: &'a mut Vec<PathBuf>,
        ) -> impl FnMut(&Path) -> io::Result<PathBuf> + 'a {
            move |path: &Path| {
                received.push(path.to_path_buf());
                let landed = self.0.join("trash").join(format!("{}", received.len()));
                fs::rename(path, &landed)?;
                Ok(landed)
            }
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn project() -> PathBuf {
        PathBuf::from("/work/app")
    }

    fn claude_md(path: &str, text: &str) -> ClaudeMd {
        let path = project().join(path);
        let dir = path.parent().unwrap().to_path_buf();
        let kind = classify(text, &dir, &project(), false);
        ClaudeMd {
            path,
            text: text.to_string(),
            kind,
        }
    }

    #[test]
    fn forms_of_claude_md() {
        let root = project();
        assert_eq!(
            classify("anything", &root, &root, true),
            ClaudeMdKind::Bridge
        );
        assert_eq!(
            classify("@AGENTS.md\n", &root, &root, false),
            ClaudeMdKind::Bridge
        );
        assert_eq!(
            classify("@./AGENTS.md\n\n  @AGENTS.md  \n", &root, &root, false),
            ClaudeMdKind::Bridge
        );
        assert_eq!(
            classify("@AGENTS.md\n\n## Rules\n- one\n", &root, &root, false),
            ClaudeMdKind::ExtendsAgents
        );
        assert_eq!(
            classify(
                "# Rules\nSee @AGENTS.md for the rest.\n",
                &root,
                &root,
                false
            ),
            ClaudeMdKind::ExtendsAgents
        );
        // Une copie identique d'AGENTS.md masque AGENTS.md : ce qu'on y écrit n'atteint plus Claude.
        assert_eq!(
            classify("# Rules\n- one\n", &root, &root, false),
            ClaudeMdKind::Shadows
        );
        assert_eq!(classify("", &root, &root, false), ClaudeMdKind::Shadows);
    }

    #[test]
    fn an_import_inside_code_is_not_an_import() {
        let root = project();
        let text = "```\n@AGENTS.md\n```\n";
        assert_eq!(classify(text, &root, &root, false), ClaudeMdKind::Shadows);
        assert_eq!(
            classify("`@AGENTS.md`\n", &root, &root, false),
            ClaudeMdKind::Shadows
        );
    }

    #[test]
    fn imports_resolve_from_the_file() {
        let root = project();
        let dot_claude = root.join(".claude");
        assert!(imports_agents("@../AGENTS.md", &dot_claude, &root));
        assert!(!imports_agents("@AGENTS.md", &dot_claude, &root));
        assert!(imports_agents("@AGENTS.md", &root, &root));
        assert!(!imports_agents("@../AGENTS.md", &root, &root));
        assert!(!imports_agents("@AGENTS.md and more", &root, &root));
        assert!(imports_agents("   @AGENTS.md   ", &root, &root));

        let bridge = claude_md(".claude/CLAUDE.md", "@../AGENTS.md\n");
        assert_eq!(bridge.kind, ClaudeMdKind::Bridge);
        assert_eq!(merge_text(Some("B\n"), &[bridge], &project()), Ok(None));

        let shadow = claude_md(".claude/CLAUDE.md", "@AGENTS.md\n");
        assert_eq!(shadow.kind, ClaudeMdKind::Shadows);
        assert_eq!(
            merge_text(Some("B\n"), &[shadow], &project()),
            Err(MergeBlocked::OtherImports)
        );
    }

    #[test]
    fn merge_adds_only_what_is_missing() {
        let a = claude_md("CLAUDE.md", "A");
        assert_eq!(
            merge_text(Some("B"), &[a], &project()),
            Ok(Some("B\n\nA\n".to_string()))
        );

        // Une copie identique, ou déjà présente au milieu, n'ajoute rien.
        let copy = claude_md("CLAUDE.md", "B\n");
        assert_eq!(merge_text(Some("B"), &[copy], &project()), Ok(None));
        let inside = claude_md("CLAUDE.md", "x\ny");
        assert_eq!(
            merge_text(Some("w\nx  \ny\nz\n"), &[inside], &project()),
            Ok(None)
        );

        let extends = claude_md("CLAUDE.md", "@AGENTS.md\nA");
        assert_eq!(
            merge_text(Some("B"), &[extends], &project()),
            Ok(Some("B\n\nA\n".to_string()))
        );

        let root = claude_md("CLAUDE.md", "A");
        let nested = claude_md(".claude/CLAUDE.md", "C");
        assert_eq!(
            merge_text(Some("B"), &[root.clone(), nested.clone()], &project()),
            Ok(Some("B\n\nA\n\nC\n".to_string()))
        );
        assert_eq!(
            merge_text(None, &[root], &project()),
            Ok(Some("A\n".to_string()))
        );
        assert_eq!(
            merge_text(None, &[nested.clone(), nested], &project()),
            Ok(Some("C\n".to_string()))
        );
    }

    #[test]
    fn a_short_rule_is_not_found_inside_a_longer_line() {
        // « A » ne « figure » pas dans « BAD » : la comparaison se fait par lignes entières.
        let a = claude_md("CLAUDE.md", "A");
        assert_eq!(
            merge_text(Some("BAD\n"), &[a], &project()),
            Ok(Some("BAD\n\nA\n".to_string()))
        );
    }

    #[test]
    fn merge_keeps_fenced_code_untouched() {
        let text = "@AGENTS.md\n```\n@AGENTS.md\n@x/y\n```\nrule";
        let file = claude_md("CLAUDE.md", text);
        assert_eq!(
            merge_text(Some("B"), &[file], &project()),
            Ok(Some("B\n\n```\n@AGENTS.md\n@x/y\n```\nrule\n".to_string()))
        );
    }

    #[test]
    fn other_imports_block_the_merge() {
        let blocked = claude_md("CLAUDE.md", "@docs/rules.md\nA");
        assert_eq!(
            merge_text(Some("B"), &[blocked], &project()),
            Err(MergeBlocked::OtherImports)
        );
        for text in ["alice@example.com", "`@x/y`", "```\n@x/y\n```"] {
            let fine = claude_md("CLAUDE.md", text);
            assert!(merge_text(Some("B"), &[fine], &project()).is_ok(), "{text}");
        }
        // Les formes d'import, jugées par le prédicat du volet : au milieu d'une phrase, chemin échappé, hors code et
        // hors guillemets, e-mail, import d'AGENTS.md seul (qui s'efface à la fusion).
        let verdict = |text: &str| merge_blocked(&claude_md("CLAUDE.md", text), &project());
        assert_eq!(
            verdict("see @docs/rules.md here"),
            Some(MergeBlocked::OtherImports)
        );
        assert_eq!(
            verdict("@Design\\ Docs/api.md"),
            Some(MergeBlocked::OtherImports)
        );
        assert_eq!(verdict("alice@example.com"), None);
        assert_eq!(verdict("use `@x/y` here"), None);
        assert_eq!(verdict("```\n@x/y\n```\n"), None);
        assert_eq!(verdict("@AGENTS.md\n@./AGENTS.md\n"), None);
        assert_eq!(verdict("\"@x/y\" in quotes"), None);
        // Un import d'AGENTS.md au milieu d'une phrase ferait un AGENTS.md qui s'importe lui-même : il bloque, avec
        // sa raison propre, et le prédicat du volet et `merge_text` disent la même chose.
        let inline = claude_md("CLAUDE.md", "Read @AGENTS.md first");
        assert_eq!(
            merge_blocked(&inline, &project()),
            Some(MergeBlocked::InlineAgentsImport)
        );
        assert_eq!(
            merge_text(Some("B"), std::slice::from_ref(&inline), &project()),
            Err(MergeBlocked::InlineAgentsImport)
        );
        assert_eq!(
            MergeBlocked::InlineAgentsImport.message(),
            "Ce CLAUDE.md cite @AGENTS.md dans une phrase : fusion à faire à la main."
        );
        assert_eq!(
            MergeBlocked::OtherImports.message(),
            "Ce CLAUDE.md importe d'autres fichiers : fusion à faire à la main."
        );
        // Les deux ensemble : les autres imports d'abord. Un import seul d'AGENTS.md ne bloque pas.
        let both = claude_md("CLAUDE.md", "Read @AGENTS.md first\n@docs/rules.md\n");
        assert_eq!(
            merge_blocked(&both, &project()),
            Some(MergeBlocked::OtherImports)
        );
        let plain = claude_md("CLAUDE.md", "@AGENTS.md\nA\n");
        assert_eq!(merge_blocked(&plain, &project()), None);
        let bridge = claude_md("CLAUDE.md", "@AGENTS.md\n");
        assert_eq!(merge_blocked(&bridge, &project()), None);
    }

    #[test]
    fn the_project_is_given_not_guessed_from_the_path() {
        // Le projet s'appelle `.claude` (l'utilisateur ouvre ~/.claude) : `<projet>/CLAUDE.md` est à la racine, et son
        // `@AGENTS.md` vise bien `<projet>/AGENTS.md`.
        let root = PathBuf::from("/home/me/.claude");
        let file = ClaudeMd {
            path: root.join("CLAUDE.md"),
            text: "@AGENTS.md\nA\n".to_string(),
            kind: classify("@AGENTS.md\nA\n", &root, &root, false),
        };
        assert_eq!(file.kind, ClaudeMdKind::ExtendsAgents);
        assert_eq!(merge_blocked(&file, &root), None);
        assert_eq!(
            merge_text(Some("B"), &[file], &root),
            Ok(Some("B\n\nA\n".to_string()))
        );
        // Et son `.claude/CLAUDE.md` imbriqué résout `@../AGENTS.md` vers la racine du projet.
        let nested_dir = root.join(".claude");
        let nested = ClaudeMd {
            path: nested_dir.join("CLAUDE.md"),
            text: "@../AGENTS.md\nC\n".to_string(),
            kind: classify("@../AGENTS.md\nC\n", &nested_dir, &root, false),
        };
        assert_eq!(nested.kind, ClaudeMdKind::ExtendsAgents);
        assert_eq!(
            merge_text(Some("B"), &[nested], &root),
            Ok(Some("B\n\nC\n".to_string()))
        );
    }

    #[test]
    fn trailing_spaces_of_agents_md_survive_the_merge() {
        let a = claude_md("CLAUDE.md", "A");
        assert_eq!(
            merge_text(Some("B  \n\n"), &[a], &project()),
            Ok(Some("B  \n\nA\n".to_string()))
        );
    }

    #[test]
    fn read_finds_every_file_and_the_link() {
        let scratch = Scratch::new();
        scratch.write("AGENTS.md", "B\n");
        scratch.write(".claude/CLAUDE.md", "@../AGENTS.md\nrule\n");
        scratch.write("CLAUDE.local.md", "mine\n");
        std::os::unix::fs::symlink("AGENTS.md", scratch.project().join("CLAUDE.md")).unwrap();

        let files = read(&scratch.project()).unwrap();
        assert_eq!(files.agents.as_deref(), Some("B\n"));
        assert_eq!(files.local.as_deref(), Some("mine\n"));
        assert_eq!(files.claude.len(), 2);
        assert_eq!(files.claude[0].path, scratch.project().join("CLAUDE.md"));
        assert_eq!(files.claude[0].kind, ClaudeMdKind::Bridge);
        assert_eq!(files.claude[1].kind, ClaudeMdKind::ExtendsAgents);
    }

    #[test]
    fn read_of_an_empty_project_is_empty() {
        let scratch = Scratch::new();
        let files = read(&scratch.project()).unwrap();
        assert_eq!(files.agents, None);
        assert!(files.claude.is_empty());
        assert_eq!(files.local, None);
    }

    #[test]
    fn a_text_saved_after_the_read_survives_the_merge() {
        let scratch = Scratch::new();
        scratch.write("AGENTS.md", "B\n");
        scratch.write("CLAUDE.md", "A\n");
        // Une lecture ancienne (celle que le volet affiche)...
        let stale = read(&scratch.project()).unwrap();
        assert_eq!(stale.agents.as_deref(), Some("B\n"));
        // ... puis le texte d'AGENTS.md change sur le disque.
        scratch.write("AGENTS.md", "B2\n");

        let mut received = Vec::new();
        let count =
            apply_merge(&scratch.project(), &mut scratch.trash_into(&mut received)).unwrap();
        assert_eq!(count, 1);
        let merged = scratch.read("AGENTS.md");
        assert!(merged.contains("B2"), "{merged}");
        assert!(merged.contains('A'), "{merged}");
        assert_eq!(merged, "B2\n\nA\n");
    }

    #[test]
    fn apply_merge_writes_then_trashes() {
        let scratch = Scratch::new();
        scratch.write("AGENTS.md", "B\n");
        scratch.write("CLAUDE.md", "A\n");
        scratch.write(".claude/CLAUDE.md", "C\n");

        let mut received = Vec::new();
        let count =
            apply_merge(&scratch.project(), &mut scratch.trash_into(&mut received)).unwrap();
        assert_eq!(count, 2);
        assert_eq!(
            received,
            vec![
                scratch.project().join("CLAUDE.md"),
                scratch.project().join(".claude/CLAUDE.md"),
            ]
        );
        assert_eq!(scratch.read("AGENTS.md"), "B\n\nA\n\nC\n");
        assert!(!scratch.exists("CLAUDE.md"));
        assert!(!scratch.exists(".claude/CLAUDE.md"));
    }

    #[test]
    fn apply_merge_creates_a_missing_agents_md_and_keeps_crlf() {
        let scratch = Scratch::new();
        scratch.write("CLAUDE.md", "A\nB\n");
        let mut received = Vec::new();
        assert_eq!(
            apply_merge(&scratch.project(), &mut scratch.trash_into(&mut received)).unwrap(),
            1
        );
        assert_eq!(scratch.read("AGENTS.md"), "A\nB\n");

        let crlf = Scratch::new();
        crlf.write("AGENTS.md", "one\r\ntwo\r\n");
        crlf.write("CLAUDE.md", "three\n");
        let mut received = Vec::new();
        apply_merge(&crlf.project(), &mut crlf.trash_into(&mut received)).unwrap();
        assert_eq!(crlf.read("AGENTS.md"), "one\r\ntwo\r\n\r\nthree\r\n");
    }

    #[test]
    fn nothing_to_add_still_trashes_the_claude_md() {
        let scratch = Scratch::new();
        scratch.write("AGENTS.md", "B\n");
        scratch.write("CLAUDE.md", "B\n");
        let mut received = Vec::new();
        let count =
            apply_merge(&scratch.project(), &mut scratch.trash_into(&mut received)).unwrap();
        assert_eq!(count, 1);
        assert_eq!(scratch.read("AGENTS.md"), "B\n");
        assert!(!scratch.exists("CLAUDE.md"));
    }

    #[test]
    fn a_link_to_agents_is_left_alone() {
        let scratch = Scratch::new();
        scratch.write("AGENTS.md", "B\n");
        std::os::unix::fs::symlink("AGENTS.md", scratch.project().join("CLAUDE.md")).unwrap();
        let mut received = Vec::new();
        let count =
            apply_merge(&scratch.project(), &mut scratch.trash_into(&mut received)).unwrap();
        assert_eq!(count, 0);
        assert!(received.is_empty());
        assert_eq!(scratch.read("AGENTS.md"), "B\n");
        assert!(scratch.exists("CLAUDE.md"));
    }

    /// Les trois façons d'avoir un seul fichier sous les deux noms : `AGENTS.md -> CLAUDE.md`, lien physique, et
    /// l'inverse. Le texte est à `CLAUDE.md`, que rien ne doit jeter.
    fn same_file_layouts() -> Vec<(&'static str, Scratch)> {
        let reverse = Scratch::new();
        reverse.write("CLAUDE.md", "full text\n");
        std::os::unix::fs::symlink("CLAUDE.md", reverse.project().join("AGENTS.md")).unwrap();
        let hard = Scratch::new();
        hard.write("AGENTS.md", "full text\n");
        fs::hard_link(
            hard.project().join("AGENTS.md"),
            hard.project().join("CLAUDE.md"),
        )
        .unwrap();
        vec![("AGENTS.md -> CLAUDE.md", reverse), ("hard link", hard)]
    }

    #[test]
    fn one_file_under_both_names_is_a_bridge_and_is_never_trashed() {
        for (layout, scratch) in same_file_layouts() {
            let files = read(&scratch.project()).unwrap();
            assert_eq!(files.claude.len(), 1, "{layout}");
            assert_eq!(files.claude[0].kind, ClaudeMdKind::Bridge, "{layout}");

            let mut received = Vec::new();
            let count =
                apply_merge(&scratch.project(), &mut scratch.trash_into(&mut received)).unwrap();
            assert_eq!(count, 0, "{layout}");
            assert!(received.is_empty(), "{layout}");
            assert_eq!(scratch.read("AGENTS.md"), "full text\n", "{layout}");
            assert_eq!(scratch.read("CLAUDE.md"), "full text\n", "{layout}");
        }
    }

    #[test]
    fn a_second_claude_md_merges_through_the_link_without_losing_the_text() {
        let merged = "full text\n\nextra rule\n";
        // `AGENTS.md -> CLAUDE.md` : l'écriture passe par le lien, le fichier commun porte le texte fusionné sous ses
        // deux noms et reste en place ; seul `.claude/CLAUDE.md` part.
        let reverse = Scratch::new();
        reverse.write("CLAUDE.md", "full text\n");
        std::os::unix::fs::symlink("CLAUDE.md", reverse.project().join("AGENTS.md")).unwrap();
        reverse.write(".claude/CLAUDE.md", "extra rule\n");
        let mut received = Vec::new();
        let count =
            apply_merge(&reverse.project(), &mut reverse.trash_into(&mut received)).unwrap();
        assert_eq!(count, 1);
        assert_eq!(received, vec![reverse.project().join(".claude/CLAUDE.md")]);
        assert_eq!(reverse.read("AGENTS.md"), merged);
        assert_eq!(reverse.read("CLAUDE.md"), merged);

        // Lien physique : l'écriture atomique remplace `AGENTS.md` par un nouveau fichier, le lien est rompu et
        // `CLAUDE.md` n'est plus qu'une copie périmée de ce qu'`AGENTS.md` contient déjà en entier : elle peut partir
        // (sinon Claude lirait l'ancien texte et ignorerait `AGENTS.md`).
        let hard = Scratch::new();
        hard.write("AGENTS.md", "full text\n");
        fs::hard_link(
            hard.project().join("AGENTS.md"),
            hard.project().join("CLAUDE.md"),
        )
        .unwrap();
        hard.write(".claude/CLAUDE.md", "extra rule\n");
        let mut received = Vec::new();
        let count = apply_merge(&hard.project(), &mut hard.trash_into(&mut received)).unwrap();
        assert_eq!(count, 2);
        assert_eq!(hard.read("AGENTS.md"), merged);
        assert!(!hard.exists("CLAUDE.md"));
        assert!(!hard.exists(".claude/CLAUDE.md"));
    }

    #[test]
    fn agents_md_changed_after_the_read_blocks_the_write_and_the_trash() {
        let scratch = Scratch::new();
        scratch.write("AGENTS.md", "B\n");
        scratch.write("CLAUDE.md", "A\n");
        let mut received = Vec::new();
        let project = scratch.project();
        let result = apply_merge_with(
            &project,
            None,
            &mut scratch.trash_into(&mut received),
            &mut || scratch.write("AGENTS.md", "B, saved meanwhile\n"),
        );
        assert!(matches!(result, Err(MergeError::Changed)), "{result:?}");
        assert!(received.is_empty());
        assert_eq!(scratch.read("AGENTS.md"), "B, saved meanwhile\n");
        assert_eq!(scratch.read("CLAUDE.md"), "A\n");
    }

    #[test]
    fn an_agents_md_created_after_the_read_is_not_overwritten() {
        let scratch = Scratch::new();
        scratch.write("CLAUDE.md", "A\n");
        let mut received = Vec::new();
        let project = scratch.project();
        let result = apply_merge_with(
            &project,
            None,
            &mut scratch.trash_into(&mut received),
            &mut || scratch.write("AGENTS.md", "created by an agent\n"),
        );
        assert!(matches!(result, Err(MergeError::Changed)), "{result:?}");
        assert!(received.is_empty());
        assert_eq!(scratch.read("AGENTS.md"), "created by an agent\n");
        assert!(scratch.exists("CLAUDE.md"));
    }

    #[test]
    fn a_claude_md_saved_after_the_read_is_not_trashed() {
        let scratch = Scratch::new();
        scratch.write("AGENTS.md", "B\n");
        scratch.write("CLAUDE.md", "A\n");
        let mut received = Vec::new();
        let project = scratch.project();
        let result = apply_merge_with(
            &project,
            None,
            &mut scratch.trash_into(&mut received),
            &mut || scratch.write("CLAUDE.md", "A\nplus a rule written meanwhile\n"),
        );
        assert!(matches!(result, Err(MergeError::Changed)), "{result:?}");
        assert!(received.is_empty());
        assert_eq!(
            scratch.read("CLAUDE.md"),
            "A\nplus a rule written meanwhile\n"
        );
        // AGENTS.md n'a pas été touché : la reprise ne duplique rien.
        assert_eq!(scratch.read("AGENTS.md"), "B\n");

        let mut received = Vec::new();
        apply_merge(&project, &mut scratch.trash_into(&mut received)).unwrap();
        assert_eq!(
            scratch.read("AGENTS.md"),
            "B\n\nA\nplus a rule written meanwhile\n"
        );
        assert!(!scratch.exists("CLAUDE.md"));
    }

    #[test]
    fn a_blocked_merge_writes_and_trashes_nothing() {
        let scratch = Scratch::new();
        scratch.write("AGENTS.md", "B\n");
        scratch.write("CLAUDE.md", "@docs/rules.md\nA\n");
        let mut received = Vec::new();
        let result = apply_merge(&scratch.project(), &mut scratch.trash_into(&mut received));
        assert!(
            matches!(result, Err(MergeError::Blocked(MergeBlocked::OtherImports))),
            "{result:?}"
        );
        assert!(received.is_empty());
        assert_eq!(scratch.read("AGENTS.md"), "B\n");
        assert!(scratch.exists("CLAUDE.md"));
    }

    #[test]
    fn a_failed_write_trashes_nothing() {
        let scratch = Scratch::new();
        scratch.write("CLAUDE.md", "A\n");
        // AGENTS.md est un dossier : l'écriture ne peut pas aboutir.
        fs::create_dir(scratch.project().join("AGENTS.md")).unwrap();
        let mut received = Vec::new();
        let result = apply_merge(&scratch.project(), &mut scratch.trash_into(&mut received));
        assert!(matches!(result, Err(MergeError::Io(_))), "{result:?}");
        assert!(received.is_empty());
        assert!(scratch.exists("CLAUDE.md"));
    }

    #[test]
    fn a_write_the_disk_refuses_trashes_nothing() {
        use std::os::unix::fs::PermissionsExt;
        let scratch = Scratch::new();
        scratch.write("AGENTS.md", "B\n");
        scratch.write("CLAUDE.md", "A\n");
        // AGENTS.md se lit mais ne s'écrit pas : la lecture réussit, c'est `save_unless_changed` qui échoue.
        let agents = scratch.project().join("AGENTS.md");
        fs::set_permissions(&agents, fs::Permissions::from_mode(0o444)).unwrap();
        let mut received = Vec::new();
        let result = apply_merge(&scratch.project(), &mut scratch.trash_into(&mut received));
        assert!(
            matches!(&result, Err(MergeError::Io(error)) if error.kind() == io::ErrorKind::PermissionDenied),
            "{result:?}"
        );
        assert!(received.is_empty());
        assert_eq!(scratch.read("AGENTS.md"), "B\n");
        assert_eq!(scratch.read("CLAUDE.md"), "A\n");
        assert!(!scratch.exists(".AGENTS.md.idema-tmp"));
    }

    #[test]
    fn the_user_claude_home_is_never_merged() {
        let scratch = Scratch::new();
        let home = scratch.0.join("home");
        let user_claude = home.join(".claude");
        fs::create_dir_all(&user_claude).unwrap();
        fs::write(user_claude.join("CLAUDE.md"), "my global memory\n").unwrap();
        fs::write(user_claude.join("AGENTS.md"), "B\n").unwrap();

        assert!(is_user_claude_home(&user_claude, &home));
        // Un chemin qui passe par un lien se compare en chemins canoniques.
        std::os::unix::fs::symlink(&user_claude, scratch.0.join("alias")).unwrap();
        assert!(is_user_claude_home(&scratch.0.join("alias"), &home));
        assert!(!is_user_claude_home(&scratch.project(), &home));
        // Le dossier personnel n'est pas `~/.claude`, mais il n'est pas un projet non plus : voir le test suivant.
        assert!(!is_user_claude_home(&home, &home));

        let mut received = Vec::new();
        let result = apply_merge_with(
            &user_claude,
            Some(&home),
            &mut scratch.trash_into(&mut received),
            &mut || {},
        );
        assert!(matches!(result, Err(MergeError::UserMemory)), "{result:?}");
        assert!(received.is_empty());
        assert_eq!(
            fs::read_to_string(user_claude.join("CLAUDE.md")).unwrap(),
            "my global memory\n"
        );
        assert_eq!(
            fs::read_to_string(user_claude.join("AGENTS.md")).unwrap(),
            "B\n"
        );

        // Un autre projet du même HOME se fusionne normalement.
        scratch.write("CLAUDE.md", "A\n");
        let mut received = Vec::new();
        let count = apply_merge_with(
            &scratch.project(),
            Some(&home),
            &mut scratch.trash_into(&mut received),
            &mut || {},
        )
        .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn the_home_folder_is_never_merged() {
        let scratch = Scratch::new();
        let home = scratch.0.join("home");
        let user_claude = home.join(".claude");
        fs::create_dir_all(&user_claude).unwrap();
        fs::write(user_claude.join("CLAUDE.md"), "my global memory\n").unwrap();
        // Même CLAUDE.md à la racine du dossier personnel : il ne part pas non plus.
        fs::write(home.join("CLAUDE.md"), "root memory\n").unwrap();
        fs::write(home.join("AGENTS.md"), "B\n").unwrap();

        // Le dossier personnel lui-même, puis un lien vers lui.
        std::os::unix::fs::symlink(&home, scratch.0.join("alias")).unwrap();
        for project in [home.clone(), scratch.0.join("alias")] {
            let mut received = Vec::new();
            let result = apply_merge_with(
                &project,
                Some(&home),
                &mut scratch.trash_into(&mut received),
                &mut || {},
            );
            assert!(matches!(result, Err(MergeError::Home)), "{result:?}");
            assert!(received.is_empty());
        }
        assert_eq!(fs::read_to_string(home.join("AGENTS.md")).unwrap(), "B\n");
        assert_eq!(
            fs::read_to_string(home.join("CLAUDE.md")).unwrap(),
            "root memory\n"
        );
        assert_eq!(
            fs::read_to_string(user_claude.join("CLAUDE.md")).unwrap(),
            "my global memory\n"
        );
        assert!(!home.join(".AGENTS.md.idema-tmp").exists());
    }

    #[test]
    fn a_claude_folder_linked_to_the_global_one_is_never_merged_nor_trashed() {
        let scratch = Scratch::new();
        let home = scratch.0.join("home");
        let user_claude = home.join(".claude");
        fs::create_dir_all(&user_claude).unwrap();
        fs::write(user_claude.join("CLAUDE.md"), "my global memory\n").unwrap();
        let project = home.join("work");
        fs::create_dir_all(&project).unwrap();
        fs::write(project.join("AGENTS.md"), "B\n").unwrap();
        fs::write(project.join("CLAUDE.md"), "A\n").unwrap();
        // Le `.claude` du projet est le `~/.claude` de la machine.
        std::os::unix::fs::symlink(&user_claude, project.join(".claude")).unwrap();

        let mut received = Vec::new();
        let result = apply_merge_with(
            &project,
            Some(&home),
            &mut scratch.trash_into(&mut received),
            &mut || {},
        );
        assert!(
            matches!(&result, Err(MergeError::GlobalConfig(file)) if file == ".claude/CLAUDE.md"),
            "{result:?}"
        );
        assert!(received.is_empty());
        assert_eq!(
            fs::read_to_string(user_claude.join("CLAUDE.md")).unwrap(),
            "my global memory\n"
        );
        assert_eq!(
            fs::read_to_string(project.join("AGENTS.md")).unwrap(),
            "B\n"
        );
        assert_eq!(
            fs::read_to_string(project.join("CLAUDE.md")).unwrap(),
            "A\n"
        );

        // Un vrai dossier `.claude` du projet se fusionne normalement.
        fs::remove_file(project.join(".claude")).unwrap();
        fs::create_dir_all(project.join(".claude")).unwrap();
        fs::write(project.join(".claude/CLAUDE.md"), "C\n").unwrap();
        let count = apply_merge_with(
            &project,
            Some(&home),
            &mut scratch.trash_into(&mut received),
            &mut || {},
        )
        .unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn a_project_named_dot_claude_merges_with_its_own_root() {
        let scratch = Scratch::new();
        let project = scratch.0.join(".claude");
        fs::create_dir_all(&project).unwrap();
        fs::write(project.join("AGENTS.md"), "B\n").unwrap();
        fs::write(project.join("CLAUDE.md"), "@AGENTS.md\nA\n").unwrap();
        let mut received = Vec::new();
        let count = apply_merge_with(
            &project,
            None,
            &mut scratch.trash_into(&mut received),
            &mut || {},
        )
        .unwrap();
        assert_eq!(count, 1);
        assert_eq!(
            fs::read_to_string(project.join("AGENTS.md")).unwrap(),
            "B\n\nA\n"
        );
    }

    #[test]
    fn a_missing_project_directory_is_an_error_and_is_not_created() {
        let scratch = Scratch::new();
        let missing = scratch.0.join("gone");
        let mut received = Vec::new();
        let result = apply_merge(&missing, &mut scratch.trash_into(&mut received));
        assert!(matches!(result, Err(MergeError::Io(_))), "{result:?}");
        assert!(!missing.exists());
    }

    #[test]
    fn local_md_gets_the_import_once() {
        assert!(local_needs_import("x"));
        assert!(!local_needs_import("@AGENTS.md\nx"));
        assert!(!local_needs_import("x\n@./AGENTS.md\n"));
        assert!(local_needs_import("```\n@AGENTS.md\n```\n"));
        assert_eq!(with_agents_import("x"), "@AGENTS.md\nx");
        assert!(!local_needs_import(&with_agents_import("x")));
    }

    /// Porte de la Tâche 11 : la vraie fusion dans le projet jetable de `IDEMA_HOST_PROJECT` ; la porte lance ensuite
    /// `claude -p` dessus. La « Corbeille » déplace le `CLAUDE.md` dans le projet, sous un nom que Claude ne lit pas :
    /// jamais la vraie Corbeille.
    #[test]
    #[ignore = "host gate: merges CLAUDE.md of IDEMA_HOST_PROJECT"]
    fn host_merges_claude_md() {
        let project = crate::project_panel::host_gate::host_project();
        let rule = "Le mot de passe du jour est GIROFLE.";
        fs::write(project.join("CLAUDE.md"), format!("{rule}\n")).unwrap();
        fs::write(
            project.join("AGENTS.md"),
            "# Projet jetable\n\nRépondre en français.\n",
        )
        .unwrap();

        let mut trashed: Vec<PathBuf> = Vec::new();
        let count = apply_merge(&project, &mut |path: &Path| {
            let target = project.join("CLAUDE.md.trashed");
            fs::rename(path, &target)?;
            trashed.push(path.to_path_buf());
            Ok(target)
        })
        .unwrap();

        assert_eq!(count, 1);
        assert_eq!(trashed, [project.join("CLAUDE.md")]);
        assert!(!project.join("CLAUDE.md").exists());
        assert!(project.join("CLAUDE.md.trashed").is_file());
        assert_eq!(
            fs::read_to_string(project.join("AGENTS.md")).unwrap(),
            format!("# Projet jetable\n\nRépondre en français.\n\n{rule}\n")
        );
    }
}
