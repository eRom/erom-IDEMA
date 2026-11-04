//! Le plan de restauration : quand restaurer, comment une session sauvegardée redevient des panes et des commandes
//! de lancement, et les lignes de la notice. Rien ici ne touche tmux ni le disque : le dossier est testé par une
//! fermeture injectée.

use std::path::{Path, PathBuf};
use std::time::Duration;

use idema_term::activity::LAUNCH_GIVE_UP;

use idema_tmux::server::ListedSession;

use super::claude::is_uuid;
use super::snapshot::{SavedPane, SavedSession, Snapshot};

/// L'écart minimal entre deux lancements d'agent restaurés : démarrer tous les agents d'un coup saturerait la machine.
pub const LAUNCH_STAGGER: Duration = Duration::from_millis(300);

/// Marge ajoutée au délai de reprise des captures. Elle absorbe le temps entre `on_restored` et la création de chaque
/// `PaneView` (la porte d'un pane ne démarre qu'à la première relecture des fenêtres de sa session), la
/// quantification du tick de la session (1 s) et le démarrage de l'agent tapé en dernier, qui écrit son fichier de
/// session juste après. Elle ne menace pas la règle de l'extinction : les retenues repartent de zéro à la reprise
/// (`SnapshotKeeper::resume`) et il faut ensuite `HOLD` (10 s) de pauvreté continue avant qu'une session soit
/// appauvrie, donc 5 s de plus ne retardent qu'une capture déjà tolérée.
const RESUME_MARGIN: Duration = Duration::from_secs(5);

/// Le délai du lancement numéro `index` (0 pour le premier) d'une restauration, compté sur toutes ses sessions.
pub fn launch_delay(index: usize) -> Duration {
    LAUNCH_STAGGER * u32::try_from(index).unwrap_or(u32::MAX)
}

/// Quand l'instantané reprend ses captures : après le dernier lancement, la porte d'envoi comprise (elle abandonne au
/// bout de `LAUNCH_GIVE_UP`), tant que des agents démarrent une capture les verrait sans conversation. Sans lancement,
/// tout de suite.
pub fn resume_delay(launches: usize) -> Duration {
    match launches {
        0 => Duration::ZERO,
        count => launch_delay(count - 1) + LAUNCH_GIVE_UP + RESUME_MARGIN,
    }
}

/// Restaurer seulement sur un serveur vide et quand l'instantané a de quoi refaire : un serveur qui a déjà des
/// sessions n'est jamais touché.
pub fn should_restore(listed: &[ListedSession], snapshot: &Snapshot) -> bool {
    listed.is_empty() && !snapshot.sessions.is_empty()
}

/// La commande à taper dans un pane restauré : `<command> --resume <id>` quand une conversation est connue, la
/// commande seule sinon.
pub fn resume_command(command: &str, conversation: Option<&str>) -> String {
    match conversation {
        Some(id) => format!("{command} --resume {id}"),
        None => command.to_owned(),
    }
}

/// Pourquoi une session n'est pas refaite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// Le dossier du projet n'existe plus.
    FolderGone,
    /// La création a échoué ; le détail va au journal, jamais à la notice.
    Failed(String),
}

impl SkipReason {
    /// Le texte de la notice.
    pub fn label(&self) -> String {
        match self {
            Self::FolderGone => "dossier introuvable".to_owned(),
            Self::Failed(_) => "création impossible".to_owned(),
        }
    }

    /// Le texte du journal : le détail de l'échec y figure, jamais dans la notice.
    pub fn journal_reason(&self) -> String {
        match self {
            Self::FolderGone => "folder gone".to_owned(),
            Self::Failed(detail) => detail.clone(),
        }
    }
}

/// Un pane à recréer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanePlan {
    pub cwd: PathBuf,
    pub agent: String,
    pub hidden_at: Option<u64>,
    pub active: bool,
    /// La commande à lancer dans le pane ; `None` pour un shell nu.
    pub launch: Option<String>,
    /// La commande reprend une conversation (`--resume`) : le journal compte les reprises et les départs à neuf.
    pub resumed: bool,
    /// `@idema_resume` à poser sur le pane neuf : `<conversation> <commande sauvée>`, exactement quand le lancement
    /// reprend une conversation. Sans elle, un second redémarrage retomberait sur l'entrée du lanceur et perdrait le
    /// modèle, l'effort et le remote-control de la commande ramenée.
    pub resume_option: Option<String>,
}

/// Une session à recréer, avec ses panes corrigés.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionPlan {
    pub saved: SavedSession,
    pub panes: Vec<PanePlan>,
}

/// Une session sauvegardée peut être restaurée, sauf un bouillon quand la section `[ideation]` est absente de la
/// config : la fonction est alors masquée, ses sessions ne reviennent pas.
pub fn ideation_restorable(saved: &SavedSession, enabled: bool) -> bool {
    !saved.ideation || enabled
}

/// Le plan d'une session sauvegardée. Garantit ce que la création de tmux ne vérifie pas : un pane dont le dossier a
/// disparu retombe sur le projet (tmux ouvrirait `$HOME` sans rien dire), et la session a exactement un pane visible
/// et actif (une session toute cachée est refusée à la création).
pub fn plan_session(
    saved: &SavedSession,
    is_dir: impl Fn(&Path) -> bool,
) -> Result<SessionPlan, SkipReason> {
    // Le projet d'un bouillon est son dossier d'idéation : la même règle vaut pour lui.
    if !is_dir(&saved.project) {
        return Err(SkipReason::FolderGone);
    }
    if saved.panes.is_empty() {
        return Err(SkipReason::Failed("no pane saved".to_owned()));
    }
    let mut panes: Vec<PanePlan> = saved
        .panes
        .iter()
        .map(|pane| pane_plan(pane, &saved.project, &is_dir))
        .collect();
    // Aucun pane visible : le premier sort de la fenêtre cachée.
    if panes.iter().all(|pane| pane.hidden_at.is_some()) {
        panes[0].hidden_at = None;
    }
    // Un seul actif, et parmi les visibles : le premier visible déjà actif, sinon le premier visible.
    let active = panes
        .iter()
        .position(|pane| pane.hidden_at.is_none() && pane.active)
        .or_else(|| panes.iter().position(|pane| pane.hidden_at.is_none()))
        .expect("a visible pane exists");
    for (index, pane) in panes.iter_mut().enumerate() {
        pane.active = index == active;
    }
    Ok(SessionPlan {
        saved: saved.clone(),
        panes,
    })
}

fn pane_plan(pane: &SavedPane, project: &Path, is_dir: &impl Fn(&Path) -> bool) -> PanePlan {
    let cwd = if is_dir(&pane.cwd) {
        pane.cwd.clone()
    } else {
        project.to_path_buf()
    };
    // La conversation est tapée dans un shell et `snapshot.json` est modifiable : seul un UUID passe.
    let conversation = pane.conversation.as_deref().filter(|id| is_uuid(id));
    let resume_option = conversation
        .zip(pane.command.as_deref())
        .map(|(id, command)| format!("{id} {command}"));
    PanePlan {
        cwd,
        agent: pane.agent.clone(),
        hidden_at: pane.hidden_at,
        active: pane.active,
        launch: pane
            .command
            .as_deref()
            .map(|command| resume_command(command, conversation)),
        resumed: resume_option.is_some(),
        resume_option,
    }
}

/// Les lignes de la notice : le bilan, puis une ligne par session non restaurée (nom affiché, raison).
pub fn notice_lines(restored: usize, skipped: &[(String, SkipReason)]) -> Vec<String> {
    let summary = match restored {
        0 => "Aucune session restaurée après le redémarrage".to_owned(),
        1 => "1 session restaurée après le redémarrage".to_owned(),
        count => format!("{count} sessions restaurées après le redémarrage"),
    };
    std::iter::once(summary)
        .chain(
            skipped
                .iter()
                .map(|(name, reason)| format!("Non restaurée : {name} ({})", reason.label())),
        )
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const UUID: &str = "aaaa1111-bbbb-4ccc-8ddd-eeeeffff0000";

    fn listed(name: &str) -> ListedSession {
        ListedSession {
            name: name.into(),
            project: "/p".into(),
            created: 1,
            layout: String::new(),
            ideation: false,
            remote_name: String::new(),
            display_name: name.into(),
        }
    }

    fn pane(cwd: &str) -> SavedPane {
        SavedPane {
            cwd: cwd.into(),
            agent: "Shell".into(),
            hidden_at: None,
            active: false,
            conversation: None,
            command: None,
        }
    }

    fn session(panes: Vec<SavedPane>) -> SavedSession {
        SavedSession {
            name: "idema-a".into(),
            project: "/p".into(),
            display_name: "Shell".into(),
            created: 1,
            layout: "single".into(),
            ideation: false,
            remote_name: String::new(),
            panes,
        }
    }

    fn snapshot_with(count: usize) -> Snapshot {
        Snapshot {
            sessions: (0..count).map(|_| session(vec![pane("/p")])).collect(),
            ..Snapshot::default()
        }
    }

    #[test]
    fn restore_only_on_an_empty_server_with_a_snapshot() {
        assert!(should_restore(&[], &snapshot_with(1)));
        assert!(!should_restore(&[listed("idema-a")], &snapshot_with(1)));
        assert!(!should_restore(&[], &snapshot_with(0)));
    }

    #[test]
    fn launches_are_staggered_across_the_whole_restoration() {
        assert_eq!(launch_delay(0), Duration::ZERO);
        assert_eq!(launch_delay(1), LAUNCH_STAGGER);
        assert_eq!(launch_delay(4), LAUNCH_STAGGER * 4);
    }

    #[test]
    fn captures_resume_only_after_the_last_gate_has_given_up() {
        assert_eq!(resume_delay(0), Duration::ZERO);
        // Un seul lancement : sa porte abandonne au bout de LAUNCH_GIVE_UP, la marge s'y ajoute.
        assert!(resume_delay(1) > LAUNCH_GIVE_UP);
        // Chaque lancement de plus recule la reprise d'au moins l'écart entre deux lancements.
        assert_eq!(resume_delay(6) - resume_delay(1), LAUNCH_STAGGER * 5);
    }

    #[test]
    fn resume_command_appends_the_conversation() {
        assert_eq!(
            resume_command("idnma run claude", Some("U")),
            "idnma run claude --resume U"
        );
        assert_eq!(resume_command("idnma run codex", None), "idnma run codex");
    }

    #[test]
    fn a_missing_project_skips_the_session() {
        let saved = session(vec![pane("/p")]);
        assert_eq!(
            plan_session(&saved, |_| false).unwrap_err(),
            SkipReason::FolderGone
        );
    }

    #[test]
    fn a_missing_ideation_folder_skips_the_session_too() {
        const IDEATION_TEST_DIR: &str = "/Users/Shared/erom-ideation";
        let mut saved = session(vec![pane(IDEATION_TEST_DIR)]);
        saved.ideation = true;
        saved.project = IDEATION_TEST_DIR.into();
        assert_eq!(
            plan_session(&saved, |_| false).unwrap_err(),
            SkipReason::FolderGone
        );
    }

    #[test]
    fn a_disabled_ideation_session_is_not_restorable() {
        let mut ideation = session(vec![pane("/p")]);
        ideation.ideation = true;
        let project = session(vec![pane("/p")]);
        assert!(!ideation_restorable(&ideation, false));
        assert!(ideation_restorable(&ideation, true));
        assert!(ideation_restorable(&project, false));
    }

    #[test]
    fn a_missing_pane_folder_falls_back_to_the_project() {
        let saved = session(vec![pane("/p/gone"), pane("/p/here")]);
        let plan = plan_session(&saved, |path| path != Path::new("/p/gone")).unwrap();
        assert_eq!(plan.panes[0].cwd, PathBuf::from("/p"));
        assert_eq!(plan.panes[1].cwd, PathBuf::from("/p/here"));
    }

    #[test]
    fn launches_follow_command_and_conversation() {
        let mut with_conversation = pane("/p");
        with_conversation.command = Some("idnma run claude".into());
        with_conversation.conversation = Some(UUID.into());
        let mut command_only = pane("/p");
        command_only.command = Some("idnma run codex".into());
        let saved = session(vec![with_conversation, command_only, pane("/p")]);
        let plan = plan_session(&saved, |_| true).unwrap();
        let launches: Vec<Option<&str>> = plan
            .panes
            .iter()
            .map(|pane| pane.launch.as_deref())
            .collect();
        let resumed = format!("idnma run claude --resume {UUID}");
        assert_eq!(
            launches,
            [Some(resumed.as_str()), Some("idnma run codex"), None]
        );
    }

    #[test]
    fn only_a_launch_with_a_conversation_counts_as_resumed() {
        let mut with_conversation = pane("/p");
        with_conversation.command = Some("idnma run claude".into());
        with_conversation.conversation = Some(UUID.into());
        let mut command_only = pane("/p");
        command_only.command = Some("idnma run codex".into());
        let mut shell_with_stale_conversation = pane("/p");
        shell_with_stale_conversation.conversation = Some(UUID.into());
        let mut tampered = pane("/p");
        tampered.command = Some("idnma run claude".into());
        tampered.conversation = Some("not-a-uuid".into());
        let saved = session(vec![
            with_conversation,
            command_only,
            shell_with_stale_conversation,
            tampered,
        ]);
        let plan = plan_session(&saved, |_| true).unwrap();
        let resumed: Vec<bool> = plan.panes.iter().map(|pane| pane.resumed).collect();
        assert_eq!(resumed, [true, false, false, false]);
    }

    #[test]
    fn a_resumed_pane_carries_its_conversation_and_command_for_the_next_restart() {
        let mut resumed = pane("/p");
        resumed.command = Some("idnma run 'claude' --model 'haiku'".into());
        resumed.conversation = Some(UUID.into());
        let mut without_conversation = pane("/p");
        without_conversation.command = Some("idnma run codex".into());
        let mut tampered = pane("/p");
        tampered.command = Some("idnma run claude".into());
        tampered.conversation = Some("not-a-uuid".into());
        let plan = plan_session(
            &session(vec![resumed, without_conversation, tampered, pane("/p")]),
            |_| true,
        )
        .unwrap();
        let options: Vec<Option<&str>> = plan
            .panes
            .iter()
            .map(|pane| pane.resume_option.as_deref())
            .collect();
        let expected = format!("{UUID} idnma run 'claude' --model 'haiku'");
        assert_eq!(options, [Some(expected.as_str()), None, None, None]);
    }

    #[test]
    fn the_journal_keeps_the_failure_detail_the_notice_hides() {
        assert_eq!(SkipReason::FolderGone.journal_reason(), "folder gone");
        let failed = SkipReason::Failed("tmux said no".into());
        assert_eq!(failed.journal_reason(), "tmux said no");
        assert!(!failed.label().contains("tmux"));
    }

    #[test]
    fn a_conversation_that_is_not_a_uuid_is_dropped_from_the_launch() {
        let mut tampered = pane("/p");
        tampered.command = Some("idnma run claude".into());
        tampered.conversation = Some("x; rm -rf ~".into());
        let plan = plan_session(&session(vec![tampered]), |_| true).unwrap();
        assert_eq!(plan.panes[0].launch.as_deref(), Some("idnma run claude"));
    }

    #[test]
    fn a_hidden_pane_whose_folder_is_gone_falls_back_to_the_project() {
        let mut hidden = pane("/p/gone");
        hidden.hidden_at = Some(5);
        let saved = session(vec![pane("/p"), hidden]);
        let plan = plan_session(&saved, |path| path != Path::new("/p/gone")).unwrap();
        assert_eq!(plan.panes[1].cwd, PathBuf::from("/p"));
        assert_eq!(plan.panes[1].hidden_at, Some(5));
    }

    #[test]
    fn an_ideation_pane_resumes_with_the_ideation_command() {
        let config = crate::config::IdeationConfig {
            dir: "/Users/Shared/erom-ideation".into(),
            model: Some("fable".into()),
            effort: Some("max".into()),
            label: "Fable".into(),
        };
        let command = crate::ideation::ideation_command(&config, "erom-ideation-3");
        let mut ideation = pane("/p");
        ideation.command = Some(command.clone());
        ideation.conversation = Some(UUID.into());
        let plan = plan_session(&session(vec![ideation]), |_| true).unwrap();
        let expected = format!("{command} --resume {UUID}");
        assert_eq!(plan.panes[0].launch.as_deref(), Some(expected.as_str()));
    }

    #[test]
    fn a_session_with_only_hidden_panes_gets_one_visible_and_active() {
        let mut first = pane("/p");
        first.hidden_at = Some(5);
        let mut second = pane("/p");
        second.hidden_at = Some(6);
        second.active = true;
        let plan = plan_session(&session(vec![first, second]), |_| true).unwrap();
        assert_eq!(plan.panes[0].hidden_at, None);
        assert_eq!(plan.panes[1].hidden_at, Some(6));
        let active: Vec<bool> = plan.panes.iter().map(|pane| pane.active).collect();
        assert_eq!(active, [true, false]);
    }

    #[test]
    fn visible_panes_keep_one_active_and_the_first_visible_wins_when_none_is() {
        let mut hidden_active = pane("/p");
        hidden_active.hidden_at = Some(5);
        hidden_active.active = true;
        let visible = pane("/p");
        let plan = plan_session(&session(vec![hidden_active, visible]), |_| true).unwrap();
        let active: Vec<bool> = plan.panes.iter().map(|pane| pane.active).collect();
        assert_eq!(active, [false, true]);

        let mut a = pane("/p");
        a.active = true;
        let mut b = pane("/p");
        b.active = true;
        let plan = plan_session(&session(vec![a, b]), |_| true).unwrap();
        let active: Vec<bool> = plan.panes.iter().map(|pane| pane.active).collect();
        assert_eq!(active, [true, false]);
    }

    #[test]
    fn a_session_without_panes_cannot_be_planned() {
        assert!(matches!(
            plan_session(&session(Vec::new()), |_| true),
            Err(SkipReason::Failed(_))
        ));
    }

    #[test]
    fn notice_lines_name_the_count_and_each_failure() {
        assert_eq!(
            notice_lines(4, &[]),
            ["4 sessions restaurées après le redémarrage"]
        );
        assert_eq!(
            notice_lines(1, &[("Claude 2".into(), SkipReason::FolderGone)]),
            [
                "1 session restaurée après le redémarrage",
                "Non restaurée : Claude 2 (dossier introuvable)"
            ]
        );
        assert_eq!(
            notice_lines(0, &[("Shell".into(), SkipReason::Failed("x".into()))]),
            [
                "Aucune session restaurée après le redémarrage",
                "Non restaurée : Shell (création impossible)"
            ]
        );
    }
}
