//! La boucle des notifications, côté service : elle reçoit les événements d'épisode, la présence de Romain et les
//! déclarations « je regarde ce pane », demande à `Episodes` ce qui est dû, et envoie. Rien n'y bloque le runtime : les
//! captures de pane et les envois (curl) passent par `spawn_blocking`, et chaque envoi est une tâche à part dont le
//! résultat revient par la même file que les événements.

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use futures::channel::mpsc::UnboundedSender as CommandSender;
use futures::future::join_all;
use idema_tmux::server::TmuxServer;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::{oneshot, watch};

use super::access::Access;
use super::devices::Subscription;
use super::episodes::{Due, EpisodeKind, Episodes, OpenOutcome, Policy};
use super::fleet::Fleet;
use super::notify_text::{payload, title, useful_lines};
use super::push_send::{Delivery, PushSender};
use super::screen;
use super::{AckOrigin, PaneCommand, ServiceEvent, log_safe};
use crate::config::CompanionConfig;
use crate::hud;

/// Les lignes de l'écran que l'on capture au-dessus de son bas pour y chercher le corps de la notification.
const CAPTURE_HISTORY: u16 = 30;
/// Le corps de la notification : les trois dernières lignes utiles.
const BODY_LINES: usize = 3;

/// Ce que la boucle reçoit.
pub enum Message {
    /// Un événement de l'espace de travail (épisode, présence, saisie du téléphone, config).
    Event(ServiceEvent),
    /// La page déclare le pane qu'elle montre au premier plan, ou plus aucun (`PUT presence`).
    Watch(Option<(String, u32)>),
    /// Un envoi est fini.
    Finished {
        session: String,
        kind: EpisodeKind,
        verdict: Verdict,
    },
}

/// Ce qu'un épisode retient de l'envoi, tous appareils confondus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Au moins un appareil a reçu la notification : l'épisode est notifié.
    Sent,
    /// Aucun n'a pu la recevoir, et au moins un échec est passager : renvoi à la fin de l'anti-rafale.
    RetryLater,
    /// Aucun appareil à joindre, ou réponses sans appel : l'épisode ne sera plus envoyé.
    Done,
}

/// Les réglages de notification d'une section `[companion]`.
pub fn policy_of(config: &CompanionConfig) -> Policy {
    Policy {
        notify_waiting: config.notify_waiting,
        notify_error: config.notify_error,
    }
}

/// Ce que la boucle utilise du service.
pub struct Notifier {
    pub access: Arc<Access>,
    /// `None` : la clé VAPID est illisible, aucun envoi n'est possible.
    pub push: Option<Arc<PushSender>>,
    pub fleet: watch::Receiver<Arc<Fleet>>,
    pub tmux: Arc<TmuxServer>,
    /// Le fil d'interface : un pane regardé s'y acquitte.
    pub commands: CommandSender<PaneCommand>,
    /// La file de la boucle elle-même : chaque envoi y dépose son verdict.
    pub inbox: UnboundedSender<Message>,
}

/// Fait vivre les épisodes jusqu'à la fermeture de la file (le service s'arrête avec son runtime).
pub async fn run(notifier: Arc<Notifier>, mut inbox: UnboundedReceiver<Message>, policy: Policy) {
    let mut episodes = Episodes::new(policy);
    loop {
        let now = Instant::now();
        let subscribers = notifier.access.subscriptions().len();
        let (ready, held) = episodes.due(now, subscribers);
        for (due, reason) in held {
            hud::log_event(&format!(
                "companion push held kind={} session={} reason={}",
                due.kind.as_str(),
                log_safe(&due.session),
                reason.as_str()
            ));
        }
        for due in ready {
            tokio::spawn(deliver(Arc::clone(&notifier), due));
        }
        let message = match episodes.next_wakeup(now) {
            Some(at) => {
                match tokio::time::timeout_at(tokio::time::Instant::from_std(at), inbox.recv())
                    .await
                {
                    Ok(message) => message,
                    // Une anti-rafale ou une déclaration de pane regardé a pris fin : un tour de plus.
                    Err(_) => continue,
                }
            }
            None => inbox.recv().await,
        };
        let Some(message) = message else {
            return;
        };
        apply(message, &mut episodes, &notifier);
    }
}

/// Un message de plus dans l'état des épisodes.
fn apply(message: Message, episodes: &mut Episodes, notifier: &Notifier) {
    let now = Instant::now();
    match message {
        Message::Event(event) => match event {
            ServiceEvent::EpisodeOpened {
                session,
                pane,
                kind,
            } => {
                if episodes.open(&session, pane, kind, now) == OpenOutcome::AutoAck {
                    acknowledge(&notifier.commands, session, pane);
                }
            }
            ServiceEvent::EpisodeClosed { session, kind } => episodes.close(&session, kind),
            ServiceEvent::PhoneInteraction { session } => episodes.phone_interaction(&session),
            ServiceEvent::Presence { away } => episodes.set_away(away),
            ServiceEvent::ConfigChanged(config) => episodes.set_policy(policy_of(&config)),
            // Les gestes de Personnaliser regardent l'accès, pas les épisodes.
            ServiceEvent::StartPairing
            | ServiceEvent::CancelPairing
            | ServiceEvent::ForgetDevice { .. } => {}
        },
        Message::Watch(Some((session, pane))) => {
            if episodes.watch(&session, pane, now) == OpenOutcome::AutoAck {
                acknowledge(&notifier.commands, session, pane);
            }
        }
        Message::Watch(None) => episodes.unwatch(),
        Message::Finished {
            session,
            kind,
            verdict,
        } => match verdict {
            Verdict::Sent => episodes.delivered(&session, kind, now),
            Verdict::Done => episodes.abandon(&session, kind),
            Verdict::RetryLater => {
                if !episodes.retry_later(&session, kind, now) {
                    hud::log_event(&format!(
                        "companion push failed kind={} session={} reason=gave_up",
                        kind.as_str(),
                        log_safe(&session)
                    ));
                }
            }
        },
    }
}

/// Acquitte ce pane au nom de la page qui le regarde. La réponse n'intéresse personne.
fn acknowledge(commands: &CommandSender<PaneCommand>, session: String, pane: u32) {
    let (reply, _) = oneshot::channel();
    let _ = commands.unbounded_send(PaneCommand::Ack {
        session,
        pane,
        origin: AckOrigin::Watched,
        reply,
    });
}

/// Envoie la notification d'un épisode, puis rend son verdict à la boucle.
async fn deliver(notifier: Arc<Notifier>, due: Due) {
    let verdict = send_episode(&notifier, &due).await;
    let _ = notifier.inbox.send(Message::Finished {
        session: due.session,
        kind: due.kind,
        verdict,
    });
}

async fn send_episode(notifier: &Arc<Notifier>, due: &Due) -> Verdict {
    let label = log_safe(&due.session);
    let Some(push) = notifier.push.clone() else {
        hud::log_event(&format!(
            "companion push failed kind={} session={label} reason=push_unavailable",
            due.kind.as_str()
        ));
        return Verdict::Done;
    };
    // La capture lance tmux : hors du runtime. Un pane disparu entre-temps donne une notification sans corps.
    let tmux = Arc::clone(&notifier.tmux);
    let pane = due.pane;
    let lines = tokio::task::spawn_blocking(move || {
        screen::capture(&tmux, pane, CAPTURE_HISTORY)
            .map(|screen| useful_lines(&screen.text, BODY_LINES))
            .unwrap_or_default()
    })
    .await
    .unwrap_or_default();
    // La flotte est lue après la capture : le badge est celui d'après la transition.
    let fleet = notifier.fleet.borrow().clone();
    let title = title_of(&fleet, &due.session, due.kind);
    let full = payload(
        due.kind,
        &title,
        &lines,
        &due.session,
        due.pane,
        fleet.badge,
    );
    let shrunk = payload(
        due.kind,
        &title,
        &lines[lines.len().saturating_sub(1)..],
        &due.session,
        due.pane,
        fleet.badge,
    );
    let sends = notifier
        .access
        .subscriptions()
        .into_iter()
        .map(|subscription| {
            send_one(
                Arc::clone(notifier),
                Arc::clone(&push),
                due.clone(),
                subscription,
                full.clone(),
                shrunk.clone(),
            )
        });
    let results = join_all(sends).await;
    if results.contains(&Delivery::Sent) {
        Verdict::Sent
    } else if results.contains(&Delivery::RetryLater) {
        Verdict::RetryLater
    } else {
        Verdict::Done
    }
}

/// Envoie à un appareil et applique la réponse à son abonnement (table de la spec §3.7). Une charge refusée pour sa
/// taille (413) est réduite à une ligne et renvoyée une fois.
async fn send_one(
    notifier: Arc<Notifier>,
    push: Arc<PushSender>,
    due: Due,
    subscription: Subscription,
    full: String,
    shrunk: String,
) -> Delivery {
    let label = log_safe(&due.session);
    let kind = due.kind.as_str();
    let mut body = full;
    let mut second_try = false;
    loop {
        let (sender, subscription_sent, session) =
            (Arc::clone(&push), subscription.clone(), due.session.clone());
        let attempt = tokio::task::spawn_blocking(move || {
            sender.deliver(&subscription_sent, &session, body.as_bytes())
        })
        .await;
        let (delivery, status) = attempt.unwrap_or((Delivery::Failed, None));
        let status_word = status.map_or_else(|| "none".to_owned(), |code| code.to_string());
        match delivery {
            Delivery::Sent => {
                hud::log_event(&format!("companion push sent kind={kind} session={label}"));
            }
            Delivery::TooLarge if !second_try => {
                hud::log_event(&format!(
                    "companion push failed kind={kind} session={label} reason=too_large status={status_word}"
                ));
                second_try = true;
                body = shrunk.clone();
                continue;
            }
            Delivery::Gone => {
                hud::log_event(&format!(
                    "companion push failed kind={kind} session={label} reason=gone status={status_word}"
                ));
                forget_subscription(&notifier.access, &subscription, status).await;
            }
            other => hud::log_event(&format!(
                "companion push failed kind={kind} session={label} reason={} status={status_word}",
                other.as_str()
            )),
        }
        return delivery;
    }
}

/// Le service push a dit l'abonnement mort (ou l'abonnement gardé est inutilisable) : il est retiré de l'appareil, sur
/// un fil bloquant (`devices.json` s'écrit).
async fn forget_subscription(
    access: &Arc<Access>,
    subscription: &Subscription,
    status: Option<u16>,
) {
    let reason = status.map_or_else(|| "invalid".to_owned(), |code| format!("http_{code}"));
    let (access, endpoint) = (Arc::clone(access), subscription.endpoint.clone());
    let removed = tokio::task::spawn_blocking(move || access.remove_endpoint(&endpoint)).await;
    if matches!(removed, Ok(Ok(true))) {
        hud::log_event(&format!("companion subscription removed reason={reason}"));
    }
}

/// Le titre de la notification d'une session : l'agent est le premier de la session, à défaut son nom affiché ; le
/// projet est le nom du dossier (le libellé de la section pour un bouillon).
pub fn title_of(fleet: &Fleet, session: &str, kind: EpisodeKind) -> String {
    let Some(entry) = fleet.sessions.iter().find(|entry| entry.name == session) else {
        return title(kind, None, session, "");
    };
    let project = if entry.ideation {
        fleet.ideation_label.clone().unwrap_or_default()
    } else {
        fleet
            .projects
            .iter()
            .find(|project| project.path == entry.project)
            .map_or_else(
                || crate::project::display_name(Path::new(&entry.project)),
                |project| project.name.clone(),
            )
    };
    title(
        kind,
        entry.agents.first().map(|agent| agent.label.as_str()),
        &entry.display_name,
        &project,
    )
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::companion::fleet::{FleetAgent, FleetPane, FleetProject, FleetSession};
    use crate::companion::test_support::{FakePush, Harness, PhoneKeys, TempDir, fake_tmux};
    use crate::companion::webpush;

    const JSON: (&str, &str) = ("Content-Type", "application/json");
    const IPHONE: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 19_0 like Mac OS X)";
    const IPAD: &str = "Mozilla/5.0 (iPad; CPU OS 19_0 like Mac OS X)";
    const SESSION: &str = "idema-ab12cd34";
    const SENTINEL: &str = "idema-5e271e10";

    /// Ce que le faux `tmux` rend pour `capture-pane` : un menu, avec des couleurs et un cadre.
    const SCREEN: &str = "\u{1b}[38;5;246m╭──────────╮\u{1b}[39m\n\u{1b}[38;5;246m│\u{1b}[39m Do you want to proceed?   \u{1b}[38;5;246m│\u{1b}[39m\n\u{1b}[38;5;246m│\u{1b}[39m ❯ 1. Yes                \u{1b}[38;5;246m│\u{1b}[39m\n\u{1b}[38;5;246m│\u{1b}[39m   2. No                 \u{1b}[38;5;246m│\u{1b}[39m\n\u{1b}[38;5;246m╰──────────╯\u{1b}[39m\n\n";

    fn session_of(name: &str, agent: Option<&str>) -> FleetSession {
        FleetSession {
            name: name.into(),
            display_name: "Refonte".into(),
            project: "/p/erom-seo".into(),
            ideation: false,
            state: "waiting",
            state_since: 0,
            seen: false,
            agents: agent
                .map(|label| FleetAgent {
                    label: label.into(),
                    logo: None,
                })
                .into_iter()
                .collect(),
            panes: vec![FleetPane {
                id: 4,
                active: true,
                state: "waiting",
            }],
        }
    }

    fn fleet_of(sessions: Vec<FleetSession>, badge: usize) -> Fleet {
        Fleet {
            mac: "Mac".into(),
            generated_at: 1,
            badge,
            ideation_label: Some("Idéation".into()),
            projects: vec![FleetProject {
                path: "/p/erom-seo".into(),
                name: "erom-seo".into(),
            }],
            sessions,
        }
    }

    fn opened(session: &str, pane: u32, kind: EpisodeKind) -> ServiceEvent {
        ServiceEvent::EpisodeOpened {
            session: session.into(),
            pane,
            kind,
        }
    }

    /// Un service avec un faux service push, un faux tmux et une flotte d'une session « Codex » ; les appareils
    /// `names` sont appairés et abonnés (un point d'accès chacun, `/QXX-<nom>`).
    struct Rig {
        service: Harness,
        fake: FakePush,
        devices: Vec<(String, PhoneKeys, String)>,
    }

    impl Rig {
        fn start(reply: (u16, &str), names: &[(&str, &str)]) -> Self {
            let fake = FakePush::start(reply.0, reply.1);
            let home = TempDir::new("notify");
            let (tmux, _log) = fake_tmux(&home.0, SCREEN.as_bytes());
            let service = Harness::start_full(home, true, Some(tmux), Some(fake.url()));
            service
                .fleet
                .send(Arc::new(fleet_of(
                    vec![
                        session_of(SESSION, Some("Codex")),
                        session_of(SENTINEL, Some("Claude")),
                    ],
                    2,
                )))
                .unwrap();
            let devices = names
                .iter()
                .map(|(name, agent)| {
                    let cookie = service.pair(agent);
                    let keys = PhoneKeys::new();
                    let endpoint = format!("https://web.push.apple.com/QXX-{name}");
                    let put = service.request(
                        "PUT",
                        "/idema-api/push/subscription",
                        &[JSON, ("Cookie", &cookie)],
                        Some(&keys.subscription_body(&endpoint)),
                    );
                    assert_eq!(put.status, 200, "{}", put.text());
                    ((*name).to_owned(), keys, cookie)
                })
                .collect();
            Self {
                service,
                fake,
                devices,
            }
        }

        fn send(&self, event: ServiceEvent) {
            self.service.events.send(event).unwrap();
        }

        fn away(&self) {
            self.send(ServiceEvent::Presence { away: true });
        }

        /// Les messages reçus par le faux service pour ce point d'accès, déchiffrés pour l'appareil.
        fn messages_for(&self, name: &str) -> Vec<serde_json::Value> {
            let (_, keys, _) = self.devices.iter().find(|d| d.0 == name).unwrap();
            self.fake
                .requests()
                .iter()
                .filter(|request| request.path == format!("/QXX-{name}"))
                .map(|request| serde_json::from_slice(&keys.decrypt(&request.body)).unwrap())
                .collect()
        }

        fn declare(&self, body: &str) -> crate::companion::test_support::Reply {
            let cookie = &self.devices[0].2;
            self.service.request(
                "PUT",
                "/idema-api/presence",
                &[JSON, ("Cookie", cookie)],
                Some(body),
            )
        }
    }

    #[test]
    fn an_away_episode_reaches_every_device_with_its_own_content() {
        let rig = Rig::start((201, ""), &[("phone", IPHONE), ("tablet", IPAD)]);
        // Romain est présent (état de départ) : l'épisode ouvert reste en attente, puis part à son absence, sans
        // nouvelle transition.
        rig.send(opened(SESSION, 4, EpisodeKind::Waiting));
        rig.away();
        let requests = rig.fake.wait_for(2);
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(rig.fake.requests().len(), 2, "one request per device");
        for request in &requests {
            assert_eq!(
                request.header("Topic"),
                Some(webpush::topic(SESSION).as_str())
            );
            assert_eq!(request.header("Urgency"), Some("high"));
            assert_eq!(request.header("TTL"), Some("3600"));
        }
        let phone = rig.messages_for("phone");
        let tablet = rig.messages_for("tablet");
        assert_eq!(phone.len(), 1);
        assert_eq!(phone, tablet, "the same message, encrypted for each");
        let message = &phone[0];
        assert_eq!(message["title"], "Codex attend · erom-seo");
        assert_eq!(message["body"], "Do you want to proceed?\n❯ 1. Yes\n2. No");
        assert_eq!(message["type"], "waiting");
        assert_eq!(message["session"], SESSION);
        assert_eq!(message["pane"], 4);
        assert_eq!(message["badge"], 2);
        assert_eq!(message["url"], format!("/#/s/{SESSION}/4"));
        assert_ne!(
            rig.fake.requests()[0].body,
            rig.fake.requests()[1].body,
            "each device gets its own ciphertext"
        );
    }

    #[test]
    fn an_error_is_sent_as_an_error_and_once() {
        let rig = Rig::start((201, ""), &[("phone", IPHONE)]);
        rig.away();
        rig.send(opened(SESSION, 4, EpisodeKind::Error));
        rig.fake.wait_for(1);
        // Rouvrir le même épisode, ou une présence répétée, ne renvoie rien : un épisode n'est notifié qu'une fois.
        rig.send(opened(SESSION, 4, EpisodeKind::Error));
        rig.send(ServiceEvent::Presence { away: true });
        rig.send(opened(SENTINEL, 4, EpisodeKind::Waiting));
        rig.fake.wait_for(2);
        let messages = rig.messages_for("phone");
        assert_eq!(messages.len(), 2, "the error once, the sentinel once");
        assert_eq!(messages[0]["title"], "Codex en erreur · erom-seo");
        assert_eq!(messages[0]["type"], "error");
        assert_eq!(messages[1]["session"], SENTINEL);
    }

    #[test]
    fn a_watched_pane_is_acknowledged_not_notified() {
        let mut rig = Rig::start((201, ""), &[("phone", IPHONE)]);
        let (acks, received) = std::sync::mpsc::channel();
        crate::companion::test_support::serve_commands(&mut rig.service, move |command| {
            if let PaneCommand::Ack {
                session,
                pane,
                origin,
                reply,
            } = command
            {
                let _ = acks.send((session, pane, origin));
                let _ = reply.send(true);
            }
        });
        rig.away();
        assert_eq!(
            rig.declare(&format!(r#"{{"session":"{SESSION}","pane":4}}"#))
                .status,
            200
        );

        // Le pane regardé qui se met à attendre est acquitté au nom de la page, et rien ne part pour lui. Une autre
        // session, elle, est notifiée : sa notification, arrivée, prouve que la première a été traitée avant.
        rig.send(opened(SESSION, 4, EpisodeKind::Waiting));
        rig.send(opened(SENTINEL, 9, EpisodeKind::Waiting));
        let requests = rig.fake.wait_for(1);
        assert_eq!(
            requests[0].header("Topic"),
            Some(webpush::topic(SENTINEL).as_str())
        );
        assert_eq!(
            received.recv_timeout(Duration::from_secs(2)).unwrap(),
            (SESSION.to_owned(), 4, AckOrigin::Watched)
        );
        assert!(received.try_recv().is_err(), "one acknowledgement");
        assert_eq!(rig.fake.requests().len(), 1);

        // Plus de déclaration (`null`) : la règle normale reprend pour ce pane.
        assert_eq!(rig.declare("null").status, 200);
        rig.send(ServiceEvent::EpisodeClosed {
            session: SESSION.into(),
            kind: EpisodeKind::Waiting,
        });
        rig.send(opened(SESSION, 4, EpisodeKind::Waiting));
        let requests = rig.fake.wait_for(2);
        assert_eq!(
            requests[1].header("Topic"),
            Some(webpush::topic(SESSION).as_str())
        );
    }

    #[test]
    fn a_declaration_must_be_well_formed_and_from_a_paired_device() {
        let rig = Rig::start((201, ""), &[("phone", IPHONE)]);
        for body in [
            "{}",
            r#"{"session":"","pane":1}"#,
            r#"{"session":"s","pane":-1}"#,
            "[]",
            "x",
        ] {
            assert_eq!(rig.declare(body).status, 400, "{body}");
        }
        let stranger = rig
            .service
            .request("PUT", "/idema-api/presence", &[JSON], Some("null"));
        assert_eq!(stranger.status, 401);
    }

    #[test]
    fn a_dead_subscription_is_removed_and_nothing_is_sent_again() {
        let rig = Rig::start((410, ""), &[("phone", IPHONE)]);
        rig.away();
        rig.send(opened(SESSION, 4, EpisodeKind::Waiting));
        rig.fake.wait_for(1);
        // Le retrait est écrit : la page ne se dit plus abonnée.
        let cookie = &rig.devices[0].2;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let me = rig
                .service
                .request("GET", "/idema-api/me", &[("Cookie", cookie)], None)
                .json();
            if me["subscribed"] == false {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "still subscribed: {me}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        // Aucun appareil abonné : l'épisode suivant est retenu, rien ne part.
        rig.send(opened(SENTINEL, 4, EpisodeKind::Waiting));
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(rig.fake.requests().len(), 1);
    }

    /// `devices.json` est un fichier : un abonnement qu'une main a changé pour un hôte hors liste ne marchera jamais.
    /// Envoyer l'épisode le retire (`reason=invalid`, sans code HTTP) sans qu'aucune requête ne parte.
    #[test]
    fn a_stored_subscription_that_cannot_be_sent_is_removed_without_a_request() {
        let Rig {
            service,
            fake,
            devices,
        } = Rig::start((201, ""), &[("phone", IPHONE)]);
        let home = service.stop();
        let file = home.0.join("companion/devices.json");
        let text = std::fs::read_to_string(&file).unwrap();
        let edited = text.replace(
            "https://web.push.apple.com/QXX-phone",
            "https://evil.example/x",
        );
        assert_ne!(text, edited);
        std::fs::write(&file, edited).unwrap();

        let (tmux, _log) = fake_tmux(&home.0, SCREEN.as_bytes());
        let service = Harness::start_full(home, true, Some(tmux), Some(fake.url()));
        service
            .fleet
            .send(Arc::new(fleet_of(
                vec![session_of(SESSION, Some("Codex"))],
                1,
            )))
            .unwrap();
        let cookie = &devices[0].2;
        let me = |service: &Harness| {
            service
                .request("GET", "/idema-api/me", &[("Cookie", cookie)], None)
                .json()
        };
        assert_eq!(me(&service)["subscribed"], true);
        service
            .events
            .send(ServiceEvent::Presence { away: true })
            .unwrap();
        service
            .events
            .send(opened(SESSION, 4, EpisodeKind::Waiting))
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while me(&service)["subscribed"] == true {
            assert!(std::time::Instant::now() < deadline, "never removed");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            fake.requests().is_empty(),
            "nothing was sent to the fake service"
        );
    }

    #[test]
    fn a_too_large_message_is_resent_once_with_one_line() {
        let rig = Rig::start((413, ""), &[("phone", IPHONE)]);
        rig.away();
        rig.send(opened(SESSION, 4, EpisodeKind::Waiting));
        let requests = rig.fake.wait_for(2);
        std::thread::sleep(Duration::from_millis(300));
        let requests = {
            let all = rig.fake.requests();
            assert_eq!(all.len(), 2, "sent once more, not more");
            let _ = requests;
            all
        };
        assert!(requests[1].body.len() < requests[0].body.len());
        // Le renvoi ne porte que la dernière ligne.
        let keys = &rig.devices[0].1;
        let second: serde_json::Value =
            serde_json::from_slice(&keys.decrypt(&requests[1].body)).unwrap();
        assert_eq!(second["body"], "2. No");
        assert_eq!(second["title"], "Codex attend · erom-seo");
    }

    #[test]
    fn config_changes_reach_the_running_service_without_dropping_streams() {
        let rig = Rig::start((201, ""), &[("phone", IPHONE)]);
        let cookie = rig.devices[0].2.clone();
        let mut stream = crate::companion::test_support::open_stream(
            rig.service.port,
            "/idema-api/events",
            &[("Cookie", cookie.as_str())],
        );
        assert!(
            stream.next_frame(Duration::from_secs(2)).is_some(),
            "first snapshot"
        );

        // « attend » n'est plus notifié : l'épisode ouvert reste retenu, l'erreur de la même session part.
        let config = |text: &str| -> CompanionConfig { toml::from_str(text).unwrap() };
        rig.away();
        rig.send(ServiceEvent::ConfigChanged(config(
            "notify_waiting = false",
        )));
        rig.send(opened(SESSION, 4, EpisodeKind::Waiting));
        rig.send(opened(SENTINEL, 4, EpisodeKind::Error));
        let requests = rig.fake.wait_for(1);
        assert_eq!(
            requests[0].header("Topic"),
            Some(webpush::topic(SENTINEL).as_str())
        );
        assert_eq!(rig.fake.requests().len(), 1, "the waiting episode is held");

        // Réactivé à chaud : l'épisode resté ouvert part, sans nouvelle transition.
        rig.send(ServiceEvent::ConfigChanged(config("")));
        let requests = rig.fake.wait_for(2);
        assert_eq!(
            requests[1].header("Topic"),
            Some(webpush::topic(SESSION).as_str())
        );

        // Le flux ouvert n'est pas tombé : une nouvelle flotte lui parvient.
        rig.service
            .fleet
            .send(Arc::new(fleet_of(vec![session_of(SESSION, None)], 7)))
            .unwrap();
        let frame = stream
            .next_frame(Duration::from_secs(3))
            .expect("the stream is alive");
        assert!(frame.contains("\"badge\":7"), "{frame}");

        // L'adresse publique suit aussi : une fois connue, seule son origine est admise en écriture.
        let (first, second) = (
            "https://mac.tailnet.ts.net:8455",
            "https://autre.tailnet.ts.net",
        );
        let write = |origin: &str| {
            rig.service
                .request(
                    "PUT",
                    "/idema-api/presence",
                    &[JSON, ("Cookie", &cookie), ("Origin", origin)],
                    Some("null"),
                )
                .status
        };
        let settle = |origin: &str, wanted: u16| {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while write(origin) != wanted {
                assert!(
                    std::time::Instant::now() < deadline,
                    "{origin} never gave {wanted}"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        rig.send(ServiceEvent::ConfigChanged(config(&format!(
            "public_url = \"{first}\""
        ))));
        settle(second, 403);
        assert_eq!(write(first), 200);
        rig.send(ServiceEvent::ConfigChanged(config(&format!(
            "public_url = \"{second}\""
        ))));
        settle(first, 403);
        assert_eq!(write(second), 200);
    }

    #[test]
    fn titles_follow_the_fleet() {
        let fleet = fleet_of(
            vec![
                session_of("a", Some("Codex")),
                session_of("b", None),
                FleetSession {
                    ideation: true,
                    project: String::new(),
                    ..session_of("c", Some("Claude"))
                },
                FleetSession {
                    project: "/elsewhere/folder-name".into(),
                    ..session_of("d", Some("Claude"))
                },
            ],
            0,
        );
        let waiting = EpisodeKind::Waiting;
        assert_eq!(title_of(&fleet, "a", waiting), "Codex attend · erom-seo");
        assert_eq!(
            title_of(&fleet, "b", EpisodeKind::Error),
            "Refonte en erreur · erom-seo"
        );
        assert_eq!(title_of(&fleet, "c", waiting), "Claude attend · Idéation");
        assert_eq!(
            title_of(&fleet, "d", waiting),
            "Claude attend · folder-name"
        );
        // Une session qui n'est plus dans la flotte : son nom, sans projet.
        assert_eq!(title_of(&fleet, "gone", waiting), "gone attend");
    }
}
