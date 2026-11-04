//! Le modèle des compétences d'un projet : ce que le CLI `skills` déclare (`list --json`), ce que les trois dossiers de
//! skills portent sur le disque, quels harnais voient chaque skill, les copies qui divergent, et l'API de recherche de
//! skills.sh. Aucune interface ni aucun processus ici : le volet lance le CLI et `curl`, ce module décode et décide.
//!
//! Une skill est identifiée par son dossier (`dir`), pas par le `name` de son front matter : c'est le dossier que le CLI
//! installe, supprime et que les harnais lisent.

use std::collections::BTreeMap;
use std::collections::hash_map::DefaultHasher;
use std::ffi::OsStr;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::Value;

use super::mcp_files::Harness;
use crate::settings::claude_cli::{CliArgument, reject_leading_dash};
use crate::settings::skills_section::frontmatter_description;

/// Les dossiers de skills d'un projet, relatifs à sa racine : Claude, Grok, puis le dossier commun `.agents`.
pub const SKILL_DIRS: [&str; 3] = [".claude/skills", ".grok/skills", ".agents/skills"];

/// Nombre de résultats demandés à skills.sh (`SEARCH_RESULT_LIMIT` du CLI 1.7.4).
const SEARCH_RESULT_LIMIT: &str = "20";
/// Profondeur maximale d'un dossier de skill : garde contre une boucle de liens symboliques.
const MAX_DEPTH: usize = 24;
const DS_STORE: &str = ".DS_Store";

/// D'où vient une skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillSource {
    /// Installée par le CLI : `source` non nulle (`kind` vaut `github`, `local`, `gitlab`, `git`, `well-known`…).
    /// Une source `local` s'affiche comme un chemin.
    Locked { source: String, kind: String },
    /// Écrite à la main : le CLI ne lui connaît ni source ni type.
    Handwritten,
}

/// Une skill du projet, telle que le volet l'affiche.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledSkill {
    /// Le nom du dossier : l'identité de la skill.
    pub dir: String,
    /// Le nom d'affichage (front matter), le dossier à défaut.
    pub name: String,
    pub source: SkillSource,
    /// La `description` du front matter de `SKILL.md`, vide si absente.
    pub description: String,
    /// Les dossiers de `SKILL_DIRS` qui portent la skill, dans l'ordre de `SKILL_DIRS`.
    pub present: Vec<&'static str>,
    /// Les mêmes dossiers groupés par contenu identique : un seul groupe, pas de divergence.
    pub versions: Vec<Vec<&'static str>>,
}

/// Une entrée de `skills list --json` de portée projet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedSkill {
    pub dir: String,
    pub name: String,
    pub source: Option<String>,
    pub kind: Option<String>,
}

/// Vrai pour un nom qui est un seul composant de chemin normal : ni `..`, ni `.`, ni `/`, ni vide.
fn is_single_component(dir: &str) -> bool {
    let mut components = Path::new(dir).components();
    matches!(
        (components.next(), components.next()),
        (Some(Component::Normal(name)), None) if name == OsStr::new(dir)
    )
}

fn invalid_dir(dir: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("nom de dossier de skill invalide : {dir:?}"),
    )
}

/// Le JSON d'un `skills … --json` dans une sortie qui peut porter du bruit avant (et après) : le premier tableau
/// d'objets qui se décode, depuis un `[`. Le `.zshrc` de l'utilisateur peut écrire n'importe quoi, y compris un `[`.
pub fn json_array_in(stdout: &str) -> Option<&str> {
    for (start, _) in stdout.match_indices('[') {
        let rest = &stdout[start..];
        let mut stream = serde_json::Deserializer::from_str(rest).into_iter::<Value>();
        if let Some(Ok(Value::Array(entries))) = stream.next()
            && entries.iter().all(Value::is_object)
        {
            return Some(&rest[..stream.byte_offset()]);
        }
    }
    None
}

/// Décode `skills list --json`. Les entrées de portée projet seulement ; le dossier est le dernier composant de `path`,
/// et une entrée dont le dossier n'est pas un seul composant normal est écartée.
pub fn parse_list(json: &str) -> Result<Vec<ListedSkill>, String> {
    let value: Value = serde_json::from_str(json)
        .map_err(|_| "La liste des compétences est illisible.".to_string())?;
    let Some(entries) = value.as_array() else {
        return Err("La liste des compétences est illisible.".to_string());
    };
    let text =
        |entry: &Value, key: &str| entry.get(key).and_then(Value::as_str).map(str::to_string);
    let mut listed = Vec::new();
    for entry in entries {
        if entry.get("scope").and_then(Value::as_str) != Some("project") {
            continue;
        }
        let Some(path) = entry.get("path").and_then(Value::as_str) else {
            continue;
        };
        let Some(dir) = Path::new(path).file_name().and_then(OsStr::to_str) else {
            continue;
        };
        if !is_single_component(dir) {
            continue;
        }
        listed.push(ListedSkill {
            dir: dir.to_string(),
            name: text(entry, "name").unwrap_or_else(|| dir.to_string()),
            source: text(entry, "source"),
            kind: text(entry, "sourceType"),
        });
    }
    Ok(listed)
}

/// La valeur de `name:` dans le front matter, sans guillemets.
fn frontmatter_name(text: &str) -> Option<String> {
    let mut lines = text.lines();
    if lines.next().map(str::trim_end) != Some("---") {
        return None;
    }
    lines
        .take_while(|line| line.trim_end() != "---")
        .find_map(|line| line.strip_prefix("name:"))
        .map(|value| value.trim().trim_matches(['"', '\'']).to_string())
        .filter(|name| !name.is_empty())
}

fn copy_path(project: &Path, root: &str, dir: &str) -> PathBuf {
    project.join(root).join(dir)
}

/// Les fichiers d'un dossier de skill, chemins relatifs et octets, `.DS_Store` exclus. Les liens sont suivis.
fn collect_files(
    root: &Path,
    relative: &Path,
    depth: usize,
    out: &mut Vec<(String, Vec<u8>)>,
) -> io::Result<()> {
    if depth > MAX_DEPTH {
        return Err(io::Error::other("dossier de skill trop profond"));
    }
    for entry in fs::read_dir(root.join(relative))? {
        let entry = entry?;
        if entry.file_name() == DS_STORE {
            continue;
        }
        let relative = relative.join(entry.file_name());
        let metadata = fs::metadata(entry.path())?;
        if metadata.is_dir() {
            collect_files(root, &relative, depth + 1, out)?;
        } else if metadata.is_file() {
            out.push((
                relative.to_string_lossy().into_owned(),
                fs::read(entry.path())?,
            ));
        }
    }
    Ok(())
}

/// L'empreinte d'un dossier : ses chemins relatifs triés et leurs octets. Un dossier illisible reçoit une empreinte
/// qui n'égale aucune autre, pour qu'il apparaisse comme une version à part plutôt que d'être confondu avec une copie.
fn fingerprint(path: &Path, unique: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    let mut files = Vec::new();
    if collect_files(path, Path::new(""), 0, &mut files).is_err() {
        "unreadable".hash(&mut hasher);
        unique.hash(&mut hasher);
        return hasher.finish();
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    for (relative, bytes) in &files {
        relative.len().hash(&mut hasher);
        relative.hash(&mut hasher);
        bytes.len().hash(&mut hasher);
        bytes.hash(&mut hasher);
    }
    hasher.finish()
}

/// Les skills du projet : celles que le CLI déclare, plus les dossiers trouvés sur le disque qui manquent à `listed`
/// (écrits à la main). Trié par dossier. Une skill sans copie dans les trois dossiers est écartée.
pub fn scan(project: &Path, listed: &[ListedSkill]) -> Vec<InstalledSkill> {
    let mut found: BTreeMap<String, (Option<String>, SkillSource)> = BTreeMap::new();
    for skill in listed {
        if !is_single_component(&skill.dir) || found.contains_key(&skill.dir) {
            continue;
        }
        let source = match &skill.source {
            Some(source) => SkillSource::Locked {
                source: source.clone(),
                kind: skill.kind.clone().unwrap_or_default(),
            },
            None => SkillSource::Handwritten,
        };
        let name = Some(skill.name.clone()).filter(|name| !name.is_empty());
        found.insert(skill.dir.clone(), (name, source));
    }
    for root in SKILL_DIRS {
        let Ok(entries) = fs::read_dir(project.join(root)) else {
            continue;
        };
        for entry in entries.flatten() {
            let Some(dir) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if dir.starts_with('.') || found.contains_key(&dir) {
                continue;
            }
            if entry.path().join("SKILL.md").is_file() {
                found.insert(dir, (None, SkillSource::Handwritten));
            }
        }
    }

    found
        .into_iter()
        .filter_map(|(dir, (listed_name, source))| {
            let present: Vec<&'static str> = SKILL_DIRS
                .into_iter()
                .filter(|root| copy_path(project, root, &dir).is_dir())
                .collect();
            // Une skill déclarée par le CLI mais absente des trois dossiers (`.cursor/skills`, par exemple) n'a rien à
            // montrer ici : « Absente pour tous » serait faux.
            if present.is_empty() {
                return None;
            }
            let text = present
                .iter()
                .find_map(|root| {
                    fs::read_to_string(copy_path(project, root, &dir).join("SKILL.md")).ok()
                })
                .unwrap_or_default();
            let name = listed_name
                .or_else(|| frontmatter_name(&text))
                .unwrap_or_else(|| dir.clone());
            Some(InstalledSkill {
                description: frontmatter_description(&text),
                versions: version_groups(project, &dir, &present),
                dir,
                name,
                source,
                present,
            })
        })
        .collect()
}

/// Les dossiers `present` qui portent la skill `dir`, groupés par contenu identique, lus sur le disque à l'appel.
fn version_groups(project: &Path, dir: &str, present: &[&'static str]) -> Vec<Vec<&'static str>> {
    let mut groups: Vec<(u64, Vec<&'static str>)> = Vec::new();
    for root in present {
        let print = fingerprint(&copy_path(project, root, dir), root);
        match groups.iter_mut().find(|(known, _)| *known == print) {
            Some((_, roots)) => roots.push(root),
            None => groups.push((print, vec![root])),
        }
    }
    groups.into_iter().map(|(_, roots)| roots).collect()
}

/// Les dossiers de `SKILL_DIRS` que chaque harnais lit.
fn harness_dirs(harness: Harness) -> &'static [&'static str] {
    match harness {
        Harness::Claude => &[".claude/skills"],
        Harness::Codex => &[".agents/skills"],
        Harness::OpenCode => &[".claude/skills", ".agents/skills"],
        Harness::Grok => &[".grok/skills", ".agents/skills", ".claude/skills"],
        Harness::Antigravity => &[".agents/skills"],
    }
}

/// Pour chacun des cinq harnais (dans l'ordre de `Harness::ALL`), vrai s'il lit un des dossiers de `present`.
pub fn visible_to(present: &[&str]) -> Vec<(Harness, bool)> {
    Harness::ALL
        .into_iter()
        .map(|harness| {
            let sees = harness_dirs(harness)
                .iter()
                .any(|root| present.contains(root));
            (harness, sees)
        })
        .collect()
}

/// « Absente pour Codex et Antigravity ».
pub fn coverage_text(missing: &[Harness]) -> String {
    let names: Vec<&str> = missing.iter().map(|harness| harness.name()).collect();
    match names.split_last() {
        None => String::new(),
        Some((only, [])) => format!("Absente pour {only}"),
        Some((last, rest)) => format!("Absente pour {} et {last}", rest.join(", ")),
    }
}

/// Copie récursivement un dossier, `.DS_Store` exclus, liens suivis.
fn copy_dir(from: &Path, to: &Path, depth: usize) -> io::Result<()> {
    if depth > MAX_DEPTH {
        return Err(io::Error::other("dossier de skill trop profond"));
    }
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        if entry.file_name() == DS_STORE {
            continue;
        }
        let target = to.join(entry.file_name());
        let metadata = fs::metadata(entry.path())?;
        if metadata.is_dir() {
            copy_dir(&entry.path(), &target, depth + 1)?;
        } else if metadata.is_file() {
            fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

fn exists_or_dangles(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

/// Prépare la copie de `source` à côté de `dest` (dossier caché `.<dir>.idema-tmp`, ignoré par `scan`) et rend son
/// chemin. Le dossier de préparation est le seul que ce module supprime directement : c'est son propre fichier
/// temporaire. Un échec de copie le retire et laisse `dest` intact.
fn stage_copy(source: &Path, dest: &Path) -> io::Result<PathBuf> {
    let parent = dest
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "destination sans parent"))?;
    fs::create_dir_all(parent)?;
    let staging = parent.join(format!(
        ".{}.idema-tmp",
        dest.file_name().and_then(OsStr::to_str).unwrap_or("skill")
    ));
    if exists_or_dangles(&staging) {
        fs::remove_dir_all(&staging)?;
    }
    if let Err(error) = copy_dir(source, &staging, 0) {
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }
    Ok(staging)
}

/// Copie le dossier de la skill dans ceux des trois qui ne l'ont pas, à l'identique. Ne touche pas une copie existante
/// (c'est `keep_version` qui tranche une divergence). La source est la première copie dans l'ordre de `SKILL_DIRS`.
pub fn propagate(project: &Path, dir: &str) -> io::Result<()> {
    if !is_single_component(dir) {
        return Err(invalid_dir(dir));
    }
    let source = SKILL_DIRS
        .into_iter()
        .map(|root| copy_path(project, root, dir))
        .find(|path| path.is_dir())
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("aucune copie de {dir}")))?;
    for root in SKILL_DIRS {
        let dest = copy_path(project, root, dir);
        if !exists_or_dangles(&dest) {
            let staging = stage_copy(&source, &dest)?;
            fs::rename(&staging, &dest)?;
        }
    }
    Ok(())
}

/// Garde la version du dossier `from` (un élément de `SKILL_DIRS`) : les autres copies existantes et différentes sont
/// remplacées par la sienne. Les dossiers où la skill est absente restent absents (c'est `propagate`). Une copie remplacée peut porter
/// des retouches introuvables ailleurs : elle part à la Corbeille par `trash`, jamais supprimée. L'ordre : la copie est
/// préparée à côté, l'ancienne part à la Corbeille, la copie prend sa place. Un échec avant la Corbeille laisse tout
/// intact.
pub fn keep_version(
    project: &Path,
    dir: &str,
    from: &str,
    trash: &mut dyn FnMut(&Path) -> io::Result<PathBuf>,
) -> io::Result<()> {
    if !is_single_component(dir) {
        return Err(invalid_dir(dir));
    }
    if !SKILL_DIRS.contains(&from) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("dossier de skills inconnu : {from:?}"),
        ));
    }
    let source = copy_path(project, from, dir);
    if !source.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("aucune copie de {dir} dans {from}"),
        ));
    }
    let real_source = fs::canonicalize(&source)?;
    for root in SKILL_DIRS.into_iter().filter(|root| *root != from) {
        let dest = copy_path(project, root, dir);
        if !exists_or_dangles(&dest) {
            continue;
        }
        // Un lien qui mène à la source : déjà identique, et la supprimer détruirait la source.
        if fs::canonicalize(&dest).is_ok_and(|real| real == real_source) {
            continue;
        }
        // Déjà identique : rien à remplacer, rien à mettre à la Corbeille.
        if fingerprint(&dest, from) == fingerprint(&source, from) {
            continue;
        }
        let staging = stage_copy(&source, &dest)?;
        if let Err(error) = trash(&dest) {
            let _ = fs::remove_dir_all(&staging);
            return Err(error);
        }
        fs::rename(&staging, &dest)?;
    }
    Ok(())
}

/// Le dossier où le CLI 1.7.4 installe une skill de nom `name` : port exact de `sanitizeName` (`cli.mjs`), minuscules,
/// toute suite de caractères hors `[a-z0-9._]` devenue un tiret, points et tirets de tête et de queue retirés, 255
/// caractères au plus, « unnamed-skill » quand il ne reste rien.
pub fn target_dir_for(name: &str) -> String {
    let mut dashed = String::new();
    let mut in_run = false;
    for ch in name.to_lowercase().chars() {
        if matches!(ch, 'a'..='z' | '0'..='9' | '.' | '_') {
            dashed.push(ch);
            in_run = false;
        } else if !in_run {
            dashed.push('-');
            in_run = true;
        }
    }
    // Tout est ASCII ici : couper à 255 octets coupe à 255 caractères, comme `substring(0, 255)`.
    let trimmed = dashed.trim_matches(['.', '-']);
    let cut = &trimmed[..trimmed.len().min(255)];
    if cut.is_empty() {
        "unnamed-skill".to_string()
    } else {
        cut.to_string()
    }
}

/// Les dossiers que l'ajout d'un résultat de skills.sh écrit dans le projet : celui du nom, et celui de l'identifiant
/// (le CLI choisit la skill par l'un ou l'autre).
pub fn hit_target_dirs(hit: &SearchHit) -> Vec<String> {
    let mut dirs = vec![target_dir_for(&hit.name)];
    if !dirs.contains(&hit.skill_id) {
        dirs.push(hit.skill_id.clone());
    }
    dirs
}

/// Profondeur maximale de la recherche de `SKILL.md` du CLI (`findSkillDirs`).
const DISCOVERY_DEPTH: usize = 5;
/// Les dossiers que le CLI ne parcourt pas (`SKIP_DIRS`).
const DISCOVERY_SKIPPED: [&str; 5] = ["node_modules", ".git", "dist", "build", "__pycache__"];

fn walk_skill_dirs(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > DISCOVERY_DEPTH {
        return;
    }
    if dir.join("SKILL.md").is_file() {
        out.push(dir.to_path_buf());
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
        let skipped = entry
            .file_name()
            .to_str()
            .is_some_and(|name| DISCOVERY_SKIPPED.contains(&name));
        if is_dir && !skipped {
            walk_skill_dirs(&entry.path(), depth + 1, out);
        }
    }
}

/// Les dossiers que « Ajouter depuis ce dossier » écrit dans le projet : pour chaque `SKILL.md` du dossier choisi, le
/// `sanitizeName` de son `name`. Le CLI ignore une skill sans `name` ou sans `description`. Un dossier qui porte
/// lui-même un `SKILL.md` n'est qu'une skill. La recherche ratisse un peu plus large que le CLI (qui préfère `skills/`
/// et les dossiers d'agents avant de descendre partout) : une collision de trop demande une confirmation de trop, une
/// collision manquée perdrait une skill.
pub fn folder_target_dirs(folder: &Path) -> Vec<String> {
    let mut found = Vec::new();
    if folder.join("SKILL.md").is_file() {
        found.push(folder.to_path_buf());
    } else {
        walk_skill_dirs(folder, 0, &mut found);
    }
    let mut dirs: Vec<String> = Vec::new();
    for dir in found {
        let Ok(text) = fs::read_to_string(dir.join("SKILL.md")) else {
            continue;
        };
        let Some(name) = frontmatter_name(&text) else {
            continue;
        };
        if frontmatter_description(&text).is_empty() {
            continue;
        }
        let target = target_dir_for(&name);
        if !dirs.contains(&target) {
            dirs.push(target);
        }
    }
    dirs
}

/// Ce que l'ajout écraserait sous un nom de dossier : une skill (connue de la liste du projet, ou apparue depuis), ou un
/// dossier sans `SKILL.md` que le CLI vide tout autant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CollisionOrigin {
    /// Une skill : sa source quand la liste du projet la connaît ; « écrite à la main » quand elle est apparue depuis la
    /// dernière lecture (le lock ne l'a pas encore déclarée).
    Skill(SkillSource),
    /// Un dossier qui ne porte aucun `SKILL.md`.
    Folder,
}

/// Une entrée des trois dossiers de skills que l'ajout vide avant d'y copier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskCollision {
    /// Le nom du dossier tel qu'il est sur le disque (le premier trouvé).
    pub dir: String,
    /// Tous les noms sous lesquels l'entrée existe dans les trois dossiers : ils ne diffèrent que par la casse.
    pub names: Vec<String>,
    pub origin: CollisionOrigin,
}

/// Les entrées du disque qu'un ajout écraserait : toute entrée des trois dossiers de skills dont le nom est celui d'une
/// cible, sans tenir compte de la casse (le disque de macOS la confond). Le CLI vide (`rm -rf`) le dossier de chaque
/// cible avant d'y copier, qu'il porte un `SKILL.md` ou non. La liste du projet (`known`, lue une fois, sans
/// surveillance du disque) ne sert qu'à dire d'où vient une skill : ce qui existe se lit sur le disque, à chaque appel.
pub fn disk_collisions(
    project: &Path,
    targets: &[String],
    known: &[InstalledSkill],
) -> Vec<DiskCollision> {
    let wanted: Vec<String> = targets.iter().map(|target| target.to_lowercase()).collect();
    // Par nom en minuscules : les noms vus et si l'un d'eux porte un `SKILL.md`.
    let mut found: BTreeMap<String, (Vec<String>, bool)> = BTreeMap::new();
    for root in SKILL_DIRS {
        let Ok(entries) = fs::read_dir(project.join(root)) else {
            continue;
        };
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let key = name.to_lowercase();
            if !wanted.contains(&key) {
                continue;
            }
            let has_skill_file = entry.path().join("SKILL.md").is_file();
            let (names, with_file) = found.entry(key).or_default();
            if !names.contains(&name) {
                names.push(name);
            }
            *with_file |= has_skill_file;
        }
    }
    found
        .into_iter()
        .map(|(key, (names, has_skill_file))| {
            let origin = match known.iter().find(|skill| skill.dir.to_lowercase() == key) {
                Some(skill) => CollisionOrigin::Skill(skill.source.clone()),
                None if has_skill_file => CollisionOrigin::Skill(SkillSource::Handwritten),
                None => CollisionOrigin::Folder,
            };
            DiskCollision {
                dir: names[0].clone(),
                names,
                origin,
            }
        })
        .collect()
}

/// Met chaque copie de la skill à la Corbeille, par la fonction injectée (une fois par copie). S'arrête à la première
/// erreur.
pub fn trash_copies(
    project: &Path,
    dir: &str,
    trash: &mut dyn FnMut(&Path) -> io::Result<PathBuf>,
) -> io::Result<()> {
    if !is_single_component(dir) {
        return Err(invalid_dir(dir));
    }
    for root in SKILL_DIRS {
        let path = copy_path(project, root, dir);
        if exists_or_dangles(&path) {
            trash(&path)?;
        }
    }
    Ok(())
}

/// Avant un retrait par le CLI : met à la Corbeille une copie de chaque version de la skill (une par groupe de copies
/// identiques, au moins une), car le CLI supprime tout pour de bon et une retouche locale n'existe peut-être que dans
/// les copies. Même quand toutes les copies sont identiques (après « Garder celle-ci », par exemple), une copie part :
/// la version retirée reste récupérable. Le dossier de la skill reste en place : le retrait du CLI se déroule ensuite
/// comme d'habitude. Le lock ne porte qu'un hachage que ce module ne sait pas refaire : on ne devine donc pas quelle
/// version est celle d'origine, on les garde toutes. S'arrête à la première erreur, avant que le CLI ait rien supprimé.
pub fn trash_divergent_copies(
    project: &Path,
    skill: &InstalledSkill,
    trash: &mut dyn FnMut(&Path) -> io::Result<PathBuf>,
) -> io::Result<()> {
    if !is_single_component(&skill.dir) {
        return Err(invalid_dir(&skill.dir));
    }
    // Les versions sont relues sur le disque ici, pas prises dans la liste du volet : une copie retouchée depuis la
    // dernière lecture de la liste serait sinon perdue par le retrait du CLI.
    let present: Vec<&'static str> = SKILL_DIRS
        .into_iter()
        .filter(|root| copy_path(project, root, &skill.dir).is_dir())
        .collect();
    for roots in &version_groups(project, &skill.dir, &present) {
        let Some(root) = roots.first() else {
            continue;
        };
        let origin = copy_path(project, root, &skill.dir);
        // « review-animations (grok) » : le dossier d'agent dit d'où vient la copie, dans la Corbeille.
        let label = root.trim_start_matches('.').split('/').next().unwrap_or("");
        let wrapper = std::env::temp_dir().join(format!(
            "idema-skill-copy-{}-{}",
            std::process::id(),
            COPY_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let staged = wrapper.join(format!("{} ({label})", skill.dir));
        let result = copy_dir(&origin, &staged, 0).and_then(|()| trash(&staged).map(|_| ()));
        // Le dossier de préparation est le propre fichier temporaire de ce module : vide quand la copie est partie.
        let _ = fs::remove_dir_all(&wrapper);
        result?;
    }
    Ok(())
}

/// Numérote les dossiers de préparation de `trash_divergent_copies`.
static COPY_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// Un résultat de skills.sh.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    pub name: String,
    pub source: String,
    pub skill_id: String,
    pub installs: u64,
}

/// `<base>/api/search?q=<texte>&limit=20`, le texte encodé en formulaire (l'espace devient `+`).
pub fn search_url(base: &str, query: &str) -> String {
    let encoded = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("q", query)
        .append_pair("limit", SEARCH_RESULT_LIMIT)
        .finish();
    format!("{}/api/search?{encoded}", base.trim_end_matches('/'))
}

/// Décode la réponse de skills.sh et la trie par installations décroissantes, comme le CLI : l'API ne trie pas.
pub fn parse_search(json: &str) -> Result<Vec<SearchHit>, String> {
    const UNREADABLE: &str = "La réponse de skills.sh est illisible.";
    let value: Value = serde_json::from_str(json).map_err(|_| UNREADABLE.to_string())?;
    let entries = value
        .get("skills")
        .and_then(Value::as_array)
        .ok_or_else(|| UNREADABLE.to_string())?;
    let mut hits: Vec<SearchHit> = entries
        .iter()
        .filter_map(|entry| {
            let text = |key: &str| entry.get(key).and_then(Value::as_str);
            let skill_id = text("skillId")?;
            Some(SearchHit {
                name: text("name").unwrap_or(skill_id).to_string(),
                source: text("source")?.to_string(),
                skill_id: skill_id.to_string(),
                installs: entry.get("installs").and_then(Value::as_u64).unwrap_or(0),
            })
        })
        .collect();
    hits.sort_by_key(|hit| std::cmp::Reverse(hit.installs));
    Ok(hits)
}

/// « 14 », « 1K », « 916,9K », « 1,2M » : une décimale, virgule française, zéro final omis. 999 960 s'écrit « 1M »
/// là où le CLI afficherait « 1000K » : choix d'IdemA, plus lisible.
pub fn format_installs(count: u64) -> String {
    fn scaled(tenths: u64, suffix: &str) -> String {
        match tenths % 10 {
            0 => format!("{}{suffix}", tenths / 10),
            frac => format!("{},{frac}{suffix}", tenths / 10),
        }
    }
    if count < 1_000 {
        return count.to_string();
    }
    let thousands_tenths = (count + 50) / 100;
    if thousands_tenths < 10_000 {
        return scaled(thousands_tenths, "K");
    }
    scaled((count + 50_000) / 100_000, "M")
}

/// Vrai quand la saisie commence comme un chemin (`/`, `~/`, `./`, `../`) : elle désigne alors une source locale à
/// installer, pas une recherche.
pub fn path_query(text: &str) -> bool {
    let text = text.trim();
    ["/", "~/", "./", "../"]
        .iter()
        .any(|prefix| text.starts_with(prefix))
}

/// Résout une saisie de chemin : `~/` dans `home`, un chemin absolu tel quel, le reste dans le projet.
pub fn resolve_path(text: &str, project: &Path, home: &Path) -> PathBuf {
    let text = text.trim();
    if let Some(rest) = text.strip_prefix("~/") {
        home.join(rest)
    } else if text.starts_with('/') {
        PathBuf::from(text)
    } else {
        project.join(text.strip_prefix("./").unwrap_or(text))
    }
}

/// Numérote les recherches : quand les réponses reviennent dans le désordre, seule celle de la dernière requête
/// s'affiche.
#[derive(Debug, Default)]
pub struct SearchQueue {
    generation: u64,
}

impl SearchQueue {
    /// Ouvre une nouvelle recherche et rend son numéro.
    pub fn next(&mut self) -> u64 {
        self.generation += 1;
        self.generation
    }

    /// Vrai seulement pour la recherche la plus récente.
    pub fn accept(&self, generation: u64) -> bool {
        generation == self.generation
    }
}

fn owned(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| arg.to_string()).collect()
}

/// `skills add <source> [--skill <nom>] --agent claude-code grok codex --copy -y --json`. Avec `--json`, le CLI rend sur stdout
/// un tableau de résultats et envoie tout son journal sur stderr. Une valeur qui commence par un
/// tiret est refusée : le CLI la lirait comme une option.
pub fn add_args(source: &str, skill: Option<&str>) -> Result<Vec<String>, &'static str> {
    reject_leading_dash(source, CliArgument::Source)?;
    let mut args = owned(&["skills", "add", source]);
    if let Some(skill) = skill {
        reject_leading_dash(skill, CliArgument::Name)?;
        args.extend(owned(&["--skill", skill]));
    }
    args.extend(owned(&[
        "--agent",
        "claude-code",
        "grok",
        "codex",
        "--copy",
        "-y",
        "--json",
    ]));
    Ok(args)
}

/// `skills remove <nom> -y`.
pub fn remove_args(name: &str) -> Result<Vec<String>, &'static str> {
    reject_leading_dash(name, CliArgument::Name)?;
    Ok(owned(&["skills", "remove", name, "-y"]))
}

/// `skills list --json` (portée projet par défaut, lancé dans le dossier du projet).
pub fn list_args() -> Vec<String> {
    owned(&["skills", "list", "--json"])
}

#[cfg(test)]
mod tests;
