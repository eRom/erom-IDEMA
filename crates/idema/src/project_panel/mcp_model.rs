//! Les serveurs MCP d'un projet : lecture des cinq fichiers des harnais, une ligne par nom de serveur avec son état
//! (complet, présent dans une partie des fichiers, défini différemment), et application des gestes de l'utilisateur
//! (enregistrer, propager, retirer).
//!
//! Ce sont les vraies configs du projet : on n'écrit qu'un fichier dont la section MCP change, on relit le disque
//! juste avant d'écrire, et un fichier illisible n'est jamais touché.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::instructions::io_error_text;
use super::mcp_files::{Harness, ReadServer, ServerChange, normalize, read_servers, write_servers};
use super::{global_config_text, is_home, resolves_into_global_config};

/// Une définition du serveur, et les harnais qui la portent.
#[derive(Debug, Clone, PartialEq)]
pub struct Version {
    pub core: Value,
    pub harnesses: Vec<Harness>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RowState {
    /// Présent et identique dans tous les fichiers lisibles.
    Complete,
    /// Identique partout où il est, mais absent d'au moins un fichier lisible.
    Incomplete { present: Vec<Harness> },
    /// Défini différemment selon les fichiers (l'emporte sur l'absence).
    Divergent { versions: Vec<Version> },
}

#[derive(Debug, Clone, PartialEq)]
pub struct ServerRow {
    pub name: String,
    /// Version commune, ou celle du premier harnais de `Harness::ALL` qui porte le serveur.
    pub core: Value,
    pub state: RowState,
    /// « Désactivé » et réglages propres à un harnais (les `extras`), par harnais.
    pub notes: Vec<(Harness, String)>,
    /// Faux dès qu'un fichier porte une définition que la forme commune ne sait pas écrire.
    pub supported: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProjectServers {
    pub rows: Vec<ServerRow>,
    /// Fichiers qu'on n'a pas pu lire, avec la raison. Leurs serveurs ne comptent pas comme absents.
    pub unreadable: Vec<(Harness, String)>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum McpOp {
    /// « Ajouter », « Enregistrer » et « Garder celle-ci ».
    Save {
        name: String,
        core: Value,
    },
    Propagate {
        name: String,
    },
    Remove {
        name: String,
    },
}

/// Ce que contient un fichier au moment de la lecture.
enum FileState {
    Missing,
    Readable {
        text: String,
        servers: BTreeMap<String, ReadServer>,
    },
    Unreadable(String),
}

fn file_path(project: &Path, harness: Harness) -> PathBuf {
    project.join(harness.mcp_file())
}

fn read_file(project: &Path, harness: Harness) -> FileState {
    // OpenCode lit aussi `opencode.jsonc` (commentaires) : on ne sait pas le réécrire, et ne pas le lire ferait passer
    // ses serveurs pour absents. Seul un `opencode.json` à côté permet de continuer.
    if harness == Harness::OpenCode
        && !file_path(project, harness).exists()
        && project.join("opencode.jsonc").exists()
    {
        return FileState::Unreadable("opencode.jsonc : format non géré".to_string());
    }
    let path = file_path(project, harness);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            // Un lien symbolique dont la cible manque n'est pas un fichier absent : on le remplacerait par un fichier.
            return if path.symlink_metadata().is_ok() {
                FileState::Unreadable(format!("{} : lien symbolique cassé", harness.mcp_file()))
            } else {
                FileState::Missing
            };
        }
        Err(error) => {
            crate::hud::log_event(&format!(
                "mcp read failed file={} error={error}",
                harness.mcp_file()
            ));
            return FileState::Unreadable(format!(
                "{} : lecture impossible ({})",
                harness.mcp_file(),
                io_error_text(&error)
            ));
        }
    };
    // Les messages de `read_servers` ne recopient jamais une ligne du fichier.
    match read_servers(harness, &text) {
        Ok(servers) => FileState::Readable { text, servers },
        Err(error) => FileState::Unreadable(format!("{} : {error}", harness.mcp_file())),
    }
}

fn read_all(project: &Path) -> Vec<(Harness, FileState)> {
    Harness::ALL
        .iter()
        .map(|&harness| (harness, read_file(project, harness)))
        .collect()
}

/// Les entrées d'un nom, dans l'ordre de `Harness::ALL`, parmi les fichiers lisibles.
fn entries_of<'a>(files: &'a [(Harness, FileState)], name: &str) -> Vec<(Harness, &'a ReadServer)> {
    files
        .iter()
        .filter_map(|(harness, state)| match state {
            FileState::Readable { servers, .. } => {
                servers.get(name).map(|server| (*harness, server))
            }
            _ => None,
        })
        .collect()
}

fn readable_harnesses(files: &[(Harness, FileState)]) -> Vec<Harness> {
    files
        .iter()
        .filter(|(_, state)| !matches!(state, FileState::Unreadable(_)))
        .map(|(harness, _)| *harness)
        .collect()
}

/// Regroupe les entrées par définition normalisée, dans l'ordre de première apparition.
fn group_versions(entries: &[(Harness, &ReadServer)]) -> Vec<Version> {
    let mut versions: Vec<(Value, Version)> = Vec::new();
    for (harness, server) in entries {
        let key = normalize(&server.core);
        match versions.iter_mut().find(|(known, _)| *known == key) {
            Some((_, version)) => version.harnesses.push(*harness),
            None => versions.push((
                key,
                Version {
                    core: server.core.clone(),
                    harnesses: vec![*harness],
                },
            )),
        }
    }
    versions.into_iter().map(|(_, version)| version).collect()
}

fn build_row(name: &str, entries: &[(Harness, &ReadServer)], readable: &[Harness]) -> ServerRow {
    let versions = group_versions(entries);
    let present: Vec<Harness> = entries.iter().map(|(harness, _)| *harness).collect();
    let state = if versions.len() > 1 {
        RowState::Divergent { versions }
    } else if readable.iter().all(|harness| present.contains(harness)) {
        RowState::Complete
    } else {
        RowState::Incomplete { present }
    };
    let mut notes = Vec::new();
    for (harness, server) in entries {
        if server.disabled {
            notes.push((*harness, "Désactivé".to_string()));
        }
        // Faits bruts : le libellé (« Désactivé dans OpenCode · ... ») est composé par l'interface.
        for extra in &server.extras {
            notes.push((*harness, extra.clone()));
        }
    }
    ServerRow {
        name: name.to_string(),
        core: entries[0].1.core.clone(),
        state,
        notes,
        supported: entries.iter().all(|(_, server)| server.supported),
    }
}

pub fn load(project: &Path) -> ProjectServers {
    let files = read_all(project);
    let readable = readable_harnesses(&files);
    let mut names: Vec<&String> = files
        .iter()
        .filter_map(|(_, state)| match state {
            FileState::Readable { servers, .. } => Some(servers.keys()),
            _ => None,
        })
        .flatten()
        .collect();
    names.sort();
    names.dedup();
    let rows = names
        .into_iter()
        .map(|name| build_row(name, &entries_of(&files, name), &readable))
        .collect();
    let unreadable = files
        .iter()
        .filter_map(|(harness, state)| match state {
            FileState::Unreadable(reason) => Some((*harness, reason.clone())),
            _ => None,
        })
        .collect();
    ProjectServers { rows, unreadable }
}

/// Applique un geste aux cinq fichiers. Rend les erreurs par fichier, vide si tout est écrit. Une erreur qui
/// concerne le geste entier (projet disparu, propagation refusée) est rendue une fois, au nom du premier harnais
/// qui porte le serveur (Claude à défaut). Un fichier illisible est rendu en erreur et jamais écrit.
pub fn apply(project: &Path, op: &McpOp) -> Vec<(Harness, String)> {
    apply_in(project, op, crate::files::panel::home_dir().as_deref())
}

/// Le message d'un geste refusé parce que le « projet » est le dossier personnel.
pub const HOME_IS_NOT_A_PROJECT: &str =
    "Le dossier personnel n'est pas un projet : rien n'a été écrit.";

/// `apply`, avec le dossier personnel donné (les tests en passent un jetable).
fn apply_in(project: &Path, op: &McpOp, home: Option<&Path>) -> Vec<(Harness, String)> {
    if !project.is_dir() {
        return vec![(Harness::Claude, "dossier du projet introuvable".to_string())];
    }
    if is_home(project, home) {
        return vec![(Harness::Claude, HOME_IS_NOT_A_PROJECT.to_string())];
    }
    // Relecture du disque : un agent a pu modifier un fichier depuis l'affichage du volet.
    let files = read_all(project);
    let mut errors: Vec<(Harness, String)> = Vec::new();
    let mut changes: Vec<(Harness, ServerChange)> = Vec::new();

    let name = match op {
        McpOp::Save { name, .. } | McpOp::Propagate { name } | McpOp::Remove { name } => name,
    };
    let entries = entries_of(&files, name);
    let first_carrier = entries
        .first()
        .map(|(harness, _)| *harness)
        .unwrap_or(Harness::Claude);
    let unsupported = entries.iter().any(|(_, server)| !server.supported);

    match op {
        McpOp::Save { name, core } => {
            let wanted = normalize(core);
            for (harness, state) in &files {
                // Un fichier absent se crée : il compte comme un fichier lisible sans serveur.
                let existing = match state {
                    FileState::Readable { servers, .. } => servers.get(name),
                    FileState::Missing => None,
                    FileState::Unreadable(_) => continue,
                };
                // Un serveur que la forme commune ne porte pas reste dans les fichiers qui l'ont déjà.
                if (unsupported && existing.is_none())
                    || existing.is_some_and(|server| normalize(&server.core) == wanted)
                {
                    continue;
                }
                changes.push((
                    *harness,
                    ServerChange::Put {
                        name: name.clone(),
                        core: core.clone(),
                    },
                ));
            }
        }
        McpOp::Propagate { name } => {
            if entries.is_empty() {
                return vec![(Harness::Claude, format!("serveur `{name}` introuvable"))];
            }
            if unsupported {
                return vec![(
                    first_carrier,
                    format!("serveur `{name}` : type non pris en charge, propagation impossible"),
                )];
            }
            if group_versions(&entries).len() > 1 {
                return vec![(
                    first_carrier,
                    format!(
                        "serveur `{name}` : définitions différentes, choisis une version avant de propager"
                    ),
                )];
            }
            let source = entries[0].1.core.clone();
            for (harness, state) in &files {
                if matches!(state, FileState::Readable { servers, .. } if !servers.contains_key(name))
                    || matches!(state, FileState::Missing)
                {
                    changes.push((
                        *harness,
                        ServerChange::Put {
                            name: name.clone(),
                            core: source.clone(),
                        },
                    ));
                }
            }
        }
        McpOp::Remove { name } => {
            for (harness, _) in &entries {
                changes.push((*harness, ServerChange::Remove { name: name.clone() }));
            }
        }
    }

    for (harness, state) in &files {
        if let FileState::Unreadable(reason) = state {
            errors.push((*harness, reason.clone()));
        }
    }
    for (harness, change) in changes {
        let original = match &files
            .iter()
            .find(|(known, _)| *known == harness)
            .map(|(_, s)| s)
        {
            Some(FileState::Readable { text, .. }) => Some(text.as_str()),
            _ => None,
        };
        if let Err(reason) = write_one(
            project,
            harness,
            original,
            std::slice::from_ref(&change),
            home,
        ) {
            errors.push((harness, reason));
        }
    }
    errors.sort_by_key(|(harness, _)| Harness::ALL.iter().position(|known| known == harness));
    errors
}

/// Calcule le nouveau texte du fichier et l'écrit s'il diffère. Journalise chaque écriture tentée.
fn write_one(
    project: &Path,
    harness: Harness,
    original: Option<&str>,
    changes: &[ServerChange],
    home: Option<&Path>,
) -> Result<(), String> {
    let rel = harness.mcp_file();
    // Un lien symbolique du projet vers la config globale d'un harnais (`.codex` vers `~/.codex`) : jamais écrite d'ici.
    if resolves_into_global_config(&file_path(project, harness), home) {
        crate::hud::log_event(&format!("mcp write file={rel} result=error"));
        return Err(global_config_text(rel));
    }
    let new_text = write_servers(harness, original, changes).map_err(|error| {
        crate::hud::log_event(&format!("mcp write file={rel} result=error"));
        format!("{rel} : {error}")
    })?;
    if new_text.is_empty() || original == Some(new_text.as_str()) {
        return Ok(());
    }
    let result = write_atomic(
        &file_path(project, harness),
        &new_text,
        harness.mcp_file().contains('/'),
    );
    crate::hud::log_event(&format!(
        "mcp write file={rel} result={}",
        if result.is_ok() { "ok" } else { "error" }
    ));
    result.map_err(|error| {
        // Le texte du système (anglais) reste dans le journal ; le volet montre la raison en français.
        crate::hud::log_event(&format!("mcp write failed file={rel} error={error}"));
        format!("{rel} : écriture impossible ({})", io_error_text(&error))
    })
}

/// Fichier temporaire dans le même dossier puis renommage. Un lien symbolique (dotfiles) est suivi pour ne pas être
/// remplacé par un fichier ordinaire, et les droits d'un fichier existant sont conservés (il peut porter des secrets).
fn write_atomic(path: &Path, text: &str, create_parent: bool) -> io::Result<()> {
    let target = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if path.symlink_metadata().is_ok_and(|meta| meta.is_symlink()) && !target.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "lien symbolique cassé",
        ));
    }
    // Seuls `.codex/`, `.grok/` et `.agents/` se créent, un seul niveau : le dossier du projet (parent de
    // `.mcp.json` et `opencode.json`) ne se recrée jamais s'il a disparu.
    if create_parent && let Some(dir) = target.parent() {
        match fs::create_dir(dir) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    let file_name = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let temp = target.with_file_name(format!(".{file_name}.idema-{}.tmp", std::process::id()));
    let result = (|| {
        // Le mode est posé à l'ouverture : le fichier peut porter des secrets, il ne doit jamais être lisible par
        // tous, même un instant. Un fichier existant garde son mode ; un nouveau suit l'umask (0666 moins umask).
        let existing_mode = fs::metadata(&target)
            .ok()
            .map(|meta| meta.permissions().mode() & 0o7777);
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(existing_mode.unwrap_or(0o666))
            .open(&temp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        if let Some(mode) = existing_mode {
            // L'umask a pu retirer des bits à la création.
            file.set_permissions(fs::Permissions::from_mode(mode))?;
        }
        fs::rename(&temp, &target)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

#[cfg(test)]
mod tests;
