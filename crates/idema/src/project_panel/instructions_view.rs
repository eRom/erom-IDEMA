//! L'onglet « Instructions » du volet Projet (spec §3.2) : la notice (CLAUDE.md qui fait ignorer `AGENTS.md`, bouton de
//! fusion), l'état vide sans `AGENTS.md`. Le calcul de la notice est pur (`instructions_notice`) ; le rendu et les gestes
//! sont ceux de `ProjectPanel` (`panel.rs`).

use std::path::Path;

use super::instructions::{
    ClaudeMdKind, InstructionFiles, is_user_claude_home, local_needs_import, merge_blocked,
};
use super::is_home;

const SHADOWS: &str = "Tant que ce projet a un CLAUDE.md, Claude ignore AGENTS.md.";
const EXTENDS: &str = "Ce CLAUDE.md ajoute des règles que seul Claude voit.";
const LOCAL_SHADOWS: &str = "Ce CLAUDE.local.md fait ignorer AGENTS.md à Claude.";
const MERGE_BUTTON: &str = "Fusionner dans AGENTS.md";
const IMPORT_BUTTON: &str = "Importer AGENTS.md dedans";

/// Le geste que le bouton d'une notice déclenche.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeAction {
    /// « Fusionner dans AGENTS.md » : `apply_merge`.
    Merge,
    /// « Importer AGENTS.md dedans » : la ligne `@AGENTS.md` en tête de `CLAUDE.local.md`.
    ImportAgents,
}

/// La notice orange au-dessus de l'éditeur.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Notice {
    pub text: &'static str,
    pub button: &'static str,
    /// Faux : le bouton est grisé et `reason` dit pourquoi.
    pub enabled: bool,
    pub reason: Option<&'static str>,
    pub action: NoticeAction,
}

/// La notice que `files` appelle, ou `None`. `project` est la racine du projet (elle résout les imports), `home` le
/// dossier personnel : pour le projet `<home>` (le dossier personnel n'est pas un projet) comme pour `<home>/.claude`,
/// dont le `CLAUDE.md` est la mémoire globale de l'utilisateur, il n'y a ni bandeau ni fusion.
pub fn instructions_notice(
    files: &InstructionFiles,
    project: &Path,
    home: Option<&Path>,
) -> Option<Notice> {
    if is_home(project, home) || home.is_some_and(|home| is_user_claude_home(project, home)) {
        return None;
    }
    // Un `Bridge` (lien, imports seuls) n'apporte rien : Claude lit `AGENTS.md`.
    let active: Vec<_> = files
        .claude
        .iter()
        .filter(|file| file.kind != ClaudeMdKind::Bridge)
        .collect();
    if !active.is_empty() {
        // Le pire cas dicte le texte : un seul `CLAUDE.md` qui fait ignorer `AGENTS.md` suffit.
        let text = if active.iter().any(|file| file.kind == ClaudeMdKind::Shadows) {
            SHADOWS
        } else {
            EXTENDS
        };
        // Un seul fichier bloqué grise le bouton : la fusion est faite pour tous ou pour aucun.
        let blocked = active.iter().find_map(|file| merge_blocked(file, project));
        return Some(Notice {
            text,
            button: MERGE_BUTTON,
            enabled: blocked.is_none(),
            reason: blocked.map(|blocked| blocked.message()),
            action: NoticeAction::Merge,
        });
    }
    // Un `CLAUDE.md` qui fait le pont (lien, ou import d'`AGENTS.md`) suffit : Claude lit `AGENTS.md` par lui, la notice
    // de `CLAUDE.local.md` serait fausse.
    if files
        .claude
        .iter()
        .any(|file| file.kind == ClaudeMdKind::Bridge)
    {
        return None;
    }
    // `CLAUDE.local.md` n'est jamais fusionné (fichier personnel) : on propose d'y importer `AGENTS.md`. Il ne passe
    // qu'après `CLAUDE.md` : une fois la fusion faite, la notice suivante apparaît.
    files
        .local
        .as_deref()
        .filter(|text| local_needs_import(text))
        .map(|_| Notice {
            text: LOCAL_SHADOWS,
            button: IMPORT_BUTTON,
            enabled: true,
            reason: None,
            action: NoticeAction::ImportAgents,
        })
}

#[cfg(test)]
mod tests {
    use super::super::instructions::{ClaudeMd, ClaudeMdKind};
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    const PROJECT: &str = "/work/app";

    fn claude(kind: ClaudeMdKind, text: &str) -> ClaudeMd {
        ClaudeMd {
            path: PathBuf::from(PROJECT).join("CLAUDE.md"),
            text: text.to_owned(),
            kind,
        }
    }

    fn files(claude: Vec<ClaudeMd>, local: Option<&str>) -> InstructionFiles {
        InstructionFiles {
            agents: Some("# rules\n".to_owned()),
            claude,
            local: local.map(str::to_owned),
        }
    }

    fn notice(files: &InstructionFiles) -> Option<Notice> {
        instructions_notice(files, Path::new(PROJECT), None)
    }

    #[test]
    fn a_shadowing_claude_md_says_claude_ignores_agents_md() {
        let found = notice(&files(vec![claude(ClaudeMdKind::Shadows, "rules\n")], None)).unwrap();
        assert_eq!(
            found.text,
            "Tant que ce projet a un CLAUDE.md, Claude ignore AGENTS.md."
        );
        assert_eq!(found.button, "Fusionner dans AGENTS.md");
        assert!(found.enabled);
        assert_eq!(found.reason, None);
        assert_eq!(found.action, NoticeAction::Merge);
    }

    #[test]
    fn a_claude_md_that_extends_agents_md_says_claude_alone_sees_its_rules() {
        let found = notice(&files(
            vec![claude(ClaudeMdKind::ExtendsAgents, "@AGENTS.md\nextra\n")],
            None,
        ))
        .unwrap();
        assert_eq!(
            found.text,
            "Ce CLAUDE.md ajoute des règles que seul Claude voit."
        );
        assert!(found.enabled);
    }

    #[test]
    fn a_bridge_claude_md_needs_no_notice() {
        assert_eq!(
            notice(&files(
                vec![claude(ClaudeMdKind::Bridge, "@AGENTS.md\n")],
                None
            )),
            None
        );
        assert_eq!(notice(&files(vec![], None)), None);
    }

    #[test]
    fn a_claude_md_importing_other_files_greys_the_merge_and_says_why() {
        let found = notice(&files(
            vec![claude(ClaudeMdKind::Shadows, "@docs/style.md\nrules\n")],
            None,
        ))
        .unwrap();
        // Le bandeau reste, le bouton est inactif.
        assert_eq!(
            found.text,
            "Tant que ce projet a un CLAUDE.md, Claude ignore AGENTS.md."
        );
        assert!(!found.enabled);
        assert_eq!(
            found.reason,
            Some("Ce CLAUDE.md importe d'autres fichiers : fusion à faire à la main.")
        );
    }

    #[test]
    fn an_inline_agents_import_greys_the_merge_with_its_own_reason() {
        let found = notice(&files(
            vec![claude(
                ClaudeMdKind::Shadows,
                "Voir @AGENTS.md pour le détail.\nrules\n",
            )],
            None,
        ))
        .unwrap();
        assert!(!found.enabled);
        assert_eq!(
            found.reason,
            Some("Ce CLAUDE.md cite @AGENTS.md dans une phrase : fusion à faire à la main.")
        );
    }

    #[test]
    fn a_blocked_file_among_two_greys_the_merge() {
        let mut second = claude(ClaudeMdKind::Shadows, "@docs/style.md\n");
        second.path = PathBuf::from(PROJECT).join(".claude").join("CLAUDE.md");
        let found = notice(&files(
            vec![claude(ClaudeMdKind::Shadows, "rules\n"), second],
            None,
        ))
        .unwrap();
        assert!(!found.enabled);
    }

    #[test]
    fn a_local_claude_file_without_the_import_offers_to_import_agents_md() {
        let found = notice(&files(vec![], Some("my private notes\n"))).unwrap();
        assert_eq!(
            found.text,
            "Ce CLAUDE.local.md fait ignorer AGENTS.md à Claude."
        );
        assert_eq!(found.button, "Importer AGENTS.md dedans");
        assert!(found.enabled);
        assert_eq!(found.action, NoticeAction::ImportAgents);
    }

    #[test]
    fn a_bridge_claude_md_makes_the_local_banner_false() {
        // Claude lit `AGENTS.md` par le pont : `CLAUDE.local.md` ne le fait pas ignorer.
        assert_eq!(
            notice(&files(
                vec![claude(ClaudeMdKind::Bridge, "@AGENTS.md\n")],
                Some("my private notes\n")
            )),
            None
        );
    }

    #[test]
    fn a_local_claude_file_that_already_imports_agents_md_is_not_flagged() {
        assert_eq!(notice(&files(vec![], Some("@AGENTS.md\nnotes\n"))), None);
    }

    #[test]
    fn the_claude_md_notice_comes_before_the_local_one() {
        let found = notice(&files(
            vec![claude(ClaudeMdKind::Shadows, "rules\n")],
            Some("notes\n"),
        ))
        .unwrap();
        assert_eq!(found.action, NoticeAction::Merge);
    }

    #[test]
    fn the_user_claude_home_has_no_notice_at_all() {
        let home = std::env::temp_dir().join(format!("idema-notice-home-{}", std::process::id()));
        let project = home.join(".claude");
        fs::create_dir_all(&project).unwrap();
        let shadows = ClaudeMd {
            path: project.join("CLAUDE.md"),
            text: "my global memory\n".to_owned(),
            kind: ClaudeMdKind::Shadows,
        };
        let all = files(vec![shadows], Some("notes\n"));
        assert_eq!(instructions_notice(&all, &project, Some(&home)), None);
        // Le même contenu dans un autre projet est signalé.
        assert!(instructions_notice(&all, Path::new(PROJECT), Some(&home)).is_some());
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn the_home_folder_has_no_notice_at_all() {
        let home = std::env::temp_dir().join(format!("idema-notice-home2-{}", std::process::id()));
        fs::create_dir_all(home.join(".claude")).unwrap();
        let shadows = ClaudeMd {
            path: home.join("CLAUDE.md"),
            text: "global rules\n".to_owned(),
            kind: ClaudeMdKind::Shadows,
        };
        let all = files(vec![shadows], Some("notes\n"));
        assert_eq!(instructions_notice(&all, &home, Some(&home)), None);
        // Un sous-dossier du dossier personnel reste un projet.
        let sub = home.join("app");
        fs::create_dir_all(&sub).unwrap();
        assert!(instructions_notice(&all, &sub, Some(&home)).is_some());
        let _ = fs::remove_dir_all(&home);
    }
}
