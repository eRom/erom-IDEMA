//! La section « Compagnon » de « Personnaliser » : l'interrupteur du compagnon iPhone, l'état de son service, l'adresse
//! tailnet et son QR code, l'appairage par code, la liste des appareils et les réglages de notification. Les réglages
//! passent par `ConfigEdit` comme ceux des autres sections IdemA ; l'état du service, lui, ne se lit jamais sur le disque :
//! le service le publie dans un `tokio::sync::watch`, que cette section suit par une tâche qui n'existe que tant que
//! `[companion]` est dans la config (compagnon éteint : aucune tâche, aucune minuterie).

use std::time::{Duration, SystemTime};

use chrono::{DateTime, Local};
use gpui::{
    AnyElement, Bounds, ClipboardItem, Context, FontWeight, SharedString, Task, canvas, div, fill,
    point, prelude::*, size,
};

use super::controls::{button, column_title, setting_row};
use super::idema_sections::Field;
use super::nav::label;
use super::qr::qr_modules;
use super::toml_write::COMPANION_DEFAULT_PORT;
use super::view::SettingsView;
use crate::assets::Icon;
use crate::companion::tailscale::{TailnetState, default_serve_command, typeable_command};
use crate::companion::{DeviceSummary, ListenState, PairingDisplay, ServiceEvent, ServiceStatus};
use crate::project_panel::banner::error_banner;
use crate::theme::{
    ERROR_TEXT, FIELD_BACKGROUND, RUNNING, SEPARATOR, TEXT_FULL, TEXT_MIN, TEXT_PRIMARY,
    TEXT_SECONDARY, Theme, WAITING, black, color, white,
};

/// Le délai entre deux tentatives de la veille quand aucun service ne tourne encore (la section vient d'être allumée :
/// l'espace de travail relit la config à la seconde) ou qu'il a été remplacé (port changé).
const FEED_RETRY: Duration = Duration::from_millis(500);
/// Combien de temps « Copié » remplace « Copier ».
const COPIED_FOR: Duration = Duration::from_secs(2);

const SERVICE_STARTING: &str = "Démarrage du service…";
const TAILNET_SEARCHING: &str = "Recherche de l'adresse…";
const FUNNEL_ALERT: &str = "Exposé sur Internet par Funnel : à couper";
const QR_CAPTION: &str =
    "À viser avec l'appareil photo de l'iPhone pour ouvrir la page dans Safari.";
const COMMAND_CAPTION: &str = "Commande à lancer dans un terminal :";
const PAIRING_HELP: &str =
    "Un code à 6 chiffres, valable 10 minutes, à saisir dans la page ouverte sur l'iPhone.";
const AWAY_HELP: &str =
    "Sans clavier ni souris depuis ce délai, ou écran verrouillé. 0 : toujours absent.";
const NO_DEVICE: &str = "Aucun appareil appairé.";
const TAILNET_LABEL: &str = "Adresse tailnet";
const DEVICES_LABEL: &str = "Appareils";
const CHECK: &str = "Vérifier";
const CANCEL: &str = "Annuler";
const FORGET: &str = "Oublier";
const COPY: &str = "Copier";
const COPIED: &str = "Copié";

/// Côté du QR code dessiné, zone de silence comprise.
const QR_SIDE: f32 = 176.;
/// Largeur de la zone de silence, en modules (la norme en demande quatre).
const QR_QUIET: usize = 4;
const NAME_COLUMN: f32 = 150.;
const DATE_COLUMN: f32 = 140.;
const ACTIVITY_COLUMN: f32 = 150.;
const NOTIFY_COLUMN: f32 = 110.;

/// L'état de la section, tenu par `SettingsView`.
pub(super) struct CompanionState {
    /// Ce que le service dit de lui ; `None` : aucun service ne tourne.
    status: Option<ServiceStatus>,
    /// Pourquoi aucun service ne tourne alors que `[companion]` est dans la config (voir `Workspace::companion_feed`).
    blocked: Option<&'static str>,
    /// La veille de l'état du service : vivante seulement tant que `[companion]` est dans la config.
    feed: Option<Task<()>>,
    /// Le tic d'une seconde du compte à rebours : vivant seulement tant qu'un code est affiché.
    ticker: Option<Task<()>>,
    /// Le QR code de l'adresse affichée, gardé tant qu'elle ne change pas : (adresse, modules).
    qr: Option<(String, Vec<Vec<bool>>)>,
    /// La commande à lancer, telle qu'elle se tape d'ici (voir `typeable_command`), calculée quand l'état change.
    command: Option<String>,
    copied: bool,
    _copied_reset: Option<Task<()>>,
}

impl CompanionState {
    pub(super) fn new() -> Self {
        Self {
            status: None,
            blocked: None,
            feed: None,
            ticker: None,
            qr: None,
            command: None,
            copied: false,
            _copied_reset: None,
        }
    }
}

// ---- Les phrases ------------------------------------------------------------------------------------------------------

/// La raison d'un service qui ne tourne pas, en une phrase : jamais le texte d'une erreur système.
fn failure_text(port: u16, reason: &str) -> String {
    let word = reason.split(':').next().unwrap_or(reason).trim();
    match word {
        "port_in_use" => format!("Port {port} déjà pris"),
        "tmux_unavailable" => "tmux est introuvable : le service ne démarre pas.".to_string(),
        "devices_unavailable" => "La liste des appareils est illisible.".to_string(),
        "bind_failed" => format!("Impossible d'ouvrir le port {port}."),
        // `thread_spawn_failed`, `runtime_failed` et tout mot que cette version ne connaît pas.
        _ => "Le service n'a pas pu démarrer.".to_string(),
    }
}

/// Pourquoi l'adresse tailnet n'a pas pu être lue (`TailnetState::Unknown`), en une phrase.
fn unknown_tailnet_text(reason: &str) -> &'static str {
    match reason.split(':').next().unwrap_or(reason).trim() {
        "binary_not_found" => "Tailscale est introuvable sur ce Mac.",
        "timeout" => "Tailscale ne répond pas.",
        _ => "L'adresse tailnet n'a pas pu être lue.",
    }
}

/// Le compte à rebours d'un code, `mm:ss`.
fn countdown(remaining: Duration) -> String {
    let seconds = remaining.as_secs();
    format!("{:02}:{:02}", seconds / 60, seconds % 60)
}

/// Il y a combien de temps, en français et en grosses mailles : l'écran se repeint à la seconde, pas besoin de plus.
fn ago(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    match seconds {
        0..=59 => "à l'instant".to_string(),
        60..=3599 => format!("il y a {} min", seconds / 60),
        3600..=86399 => format!("il y a {} h", seconds / 3600),
        _ => format!("il y a {} j", seconds / 86400),
    }
}

/// Une date d'appairage, `05/11/2026 18:12`, à l'heure du Mac.
fn date_text(at: SystemTime) -> String {
    DateTime::<Local>::from(at)
        .format("%d/%m/%Y %H:%M")
        .to_string()
}

// ---- La veille de l'état du service ----------------------------------------------------------------------------------

impl SettingsView {
    /// Aligne la veille sur la config : elle naît avec `[companion]` et meurt avec. Sans effet quand elle est déjà comme
    /// il faut. La tâche lit l'état du service par la fenêtre principale (jamais ici, qui peut être appelée pendant sa
    /// mise à jour) puis s'abonne à son `watch` : l'écran se repeint à chaque changement, sans lecture périodique.
    pub(super) fn sync_companion_feed(&mut self, cx: &mut Context<Self>) {
        let enabled = self
            .idema
            .config()
            .as_ref()
            .is_ok_and(|config| config.companion.is_some());
        if !enabled {
            let was_running = self.companion.feed.take().is_some();
            self.companion.ticker = None;
            if was_running || self.companion.status.is_some() || self.companion.blocked.is_some() {
                self.companion.status = None;
                self.companion.blocked = None;
                self.companion.qr = None;
                self.companion.command = None;
                cx.notify();
            }
            return;
        }
        if self.companion.feed.is_some() {
            return;
        }
        let workspace = self.workspace_handle();
        self.companion.feed = Some(cx.spawn(async move |this, cx| {
            loop {
                let (receiver, blocked) = workspace
                    .update(cx, |workspace, _, _| workspace.companion_feed())
                    .unwrap_or((None, None));
                match receiver {
                    Some(mut receiver) => loop {
                        let status = receiver.borrow_and_update().clone();
                        if this
                            .update(cx, |this, cx| this.apply_companion(Some(status), None, cx))
                            .is_err()
                        {
                            return;
                        }
                        // L'émetteur tombe quand le service est remplacé (port changé) : on se réabonne au suivant.
                        if receiver.changed().await.is_err() {
                            break;
                        }
                    },
                    None => {
                        if this
                            .update(cx, |this, cx| this.apply_companion(None, blocked, cx))
                            .is_err()
                        {
                            return;
                        }
                    }
                }
                cx.background_executor().timer(FEED_RETRY).await;
            }
        }));
    }

    /// Range ce que le service vient de dire et prépare ce que le rendu en tire (QR, commande, tic du compte à rebours).
    fn apply_companion(
        &mut self,
        status: Option<ServiceStatus>,
        blocked: Option<&'static str>,
        cx: &mut Context<Self>,
    ) {
        if self.companion.status == status && self.companion.blocked == blocked {
            return;
        }
        let port = status
            .as_ref()
            .and_then(|status| match status.listen {
                ListenState::Listening { port } | ListenState::Failed { port, .. } => Some(port),
                ListenState::Starting => None,
            })
            .unwrap_or(COMPANION_DEFAULT_PORT);
        let path = std::env::var("PATH").ok();
        let tailnet = status.as_ref().and_then(|status| status.tailnet.clone());
        match &tailnet {
            Some(TailnetState::Exposed(endpoint)) => {
                if self.companion.qr.as_ref().map(|(url, _)| url) != Some(&endpoint.url) {
                    self.companion.qr = Some((endpoint.url.clone(), qr_modules(&endpoint.url)));
                }
                self.companion.command = None;
            }
            Some(TailnetState::NotExposed { command }) => {
                self.companion.qr = None;
                self.companion.command = Some(typeable_command(command, path.as_deref()));
            }
            Some(TailnetState::Unknown(_)) => {
                self.companion.qr = None;
                self.companion.command = Some(typeable_command(
                    &default_serve_command(port),
                    path.as_deref(),
                ));
            }
            None => {
                self.companion.qr = None;
                self.companion.command = None;
            }
        }
        let pairing = status
            .as_ref()
            .is_some_and(|status| status.pairing.is_some());
        self.companion.status = status;
        self.companion.blocked = blocked;
        if !pairing {
            self.companion.ticker = None;
        } else if self.companion.ticker.is_none() {
            self.companion.ticker = Some(cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(Duration::from_secs(1)).await;
                    let alive = this.update(cx, |this, cx| {
                        cx.notify();
                        this.companion.ticker.is_some()
                    });
                    if !matches!(alive, Ok(true)) {
                        return;
                    }
                }
            }));
        }
        cx.notify();
    }

    /// Ouverture de la section ou retour au premier plan : l'adresse tailnet est relue (un `tailscale serve` a pu être
    /// lancé entre-temps dans un terminal).
    pub(super) fn recheck_tailnet(&mut self, cx: &mut Context<Self>) {
        if self.companion.status.is_some() {
            self.send_companion(ServiceEvent::RecheckTailnet, cx);
        }
    }

    fn send_companion(&mut self, event: ServiceEvent, cx: &mut Context<Self>) {
        let _ = self
            .workspace_handle()
            .update(cx, |workspace, _, _| workspace.companion_send(event));
    }

    fn copy_command(&mut self, cx: &mut Context<Self>) {
        let Some(command) = self.companion.command.clone() else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(command));
        self.companion.copied = true;
        self.companion._copied_reset = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(COPIED_FOR).await;
            let _ = this.update(cx, |this, cx| {
                this.companion.copied = false;
                cx.notify();
            });
        }));
        cx.notify();
    }
}

// ---- Le rendu ---------------------------------------------------------------------------------------------------------

impl SettingsView {
    /// Le corps de la section : l'interrupteur seul tant que le compagnon est éteint (la section n'existe pas dans la
    /// config, le service non plus), le reste dans l'ordre de la spec (§3.2) quand il est allumé.
    pub(super) fn render_companion_section(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let content = match self.idema.config().clone() {
            Err(error) => self
                .render_config_error(&error, theme, cx)
                .into_any_element(),
            Ok(config) => {
                let mut rows = div().flex().flex_col().child(self.toggle_row(
                    label::COMPANION_ENABLED,
                    Field::CompanionEnabled,
                    config.companion.is_some(),
                    theme,
                    cx,
                ));
                if let Some(section) = config.companion {
                    rows = rows.child(self.service_row(section.port, theme));
                    // L'adresse, l'appairage et les appareils ne vivent que par un service qui écoute : en échec (port
                    // pris, tmux absent) ou au démarrage, ils n'auraient rien à montrer ni aucun geste qui aboutisse.
                    let listening = self.companion.status.as_ref().is_some_and(|status| {
                        matches!(status.listen, ListenState::Listening { .. })
                    });
                    if listening {
                        rows = rows
                            .child(self.tailnet_row(theme, cx))
                            .children(self.tailnet_extras(theme, cx))
                            .child(self.pairing_row(theme, cx))
                            .children(self.pairing_code(theme, cx))
                            .child(self.devices_list(theme, cx));
                    }
                    rows = rows
                        .child(self.toggle_row(
                            label::NOTIFY_WAITING,
                            Field::CompanionNotifyWaiting,
                            section.notify_waiting,
                            theme,
                            cx,
                        ))
                        .child(self.toggle_row(
                            label::NOTIFY_ERROR,
                            Field::CompanionNotifyError,
                            section.notify_error,
                            theme,
                            cx,
                        ))
                        .child(self.field_row(
                            label::AWAY_AFTER,
                            Some(AWAY_HELP),
                            Field::CompanionAwayAfter,
                            80.,
                            theme,
                            cx,
                        ));
                }
                rows.into_any_element()
            }
        };
        div()
            .id(("settings-body", super::nav::Section::Companion as usize))
            .size_full()
            .overflow_y_scroll()
            .px(theme.px(20.))
            .pb(theme.px(20.))
            .child(content)
            .into_any_element()
    }

    /// Une ligne de la section qui n'est pas un réglage : même gabarit que `setting_row`, trait de séparation compris.
    fn plain_row(&self, theme: &Theme) -> gpui::Div {
        div()
            .flex()
            .items_center()
            .gap(theme.px(8.))
            .py(theme.px(10.))
            .border_b_1()
            .border_color(white(SEPARATOR))
    }

    /// L'état du service : la pastille et une phrase. Verte à l'écoute, rouge en échec, jaune le temps du démarrage.
    fn service_row(&self, configured_port: u16, theme: &Theme) -> gpui::Div {
        let (dot, text, failed) = match (&self.companion.status, self.companion.blocked) {
            (Some(status), _) => match &status.listen {
                ListenState::Listening { port } => {
                    (RUNNING, format!("À l'écoute sur 127.0.0.1:{port}"), false)
                }
                ListenState::Failed { port, reason } => {
                    (ERROR_TEXT, failure_text(*port, reason), true)
                }
                ListenState::Starting => (WAITING, SERVICE_STARTING.to_string(), false),
            },
            (None, Some(reason)) => (ERROR_TEXT, failure_text(configured_port, reason), true),
            (None, None) => (WAITING, SERVICE_STARTING.to_string(), false),
        };
        self.plain_row(theme)
            .child(
                div()
                    .flex_none()
                    .size(theme.px(8.))
                    .rounded_full()
                    .bg(color(dot)),
            )
            .child(
                div()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(if failed {
                        color(ERROR_TEXT)
                    } else {
                        white(TEXT_PRIMARY)
                    })
                    .child(SharedString::from(text)),
            )
    }

    /// « Adresse tailnet » : l'adresse détectée, ou ce qui manque pour en avoir une, et « Vérifier ».
    fn tailnet_row(&mut self, theme: &Theme, cx: &mut Context<Self>) -> gpui::Div {
        let help = match self
            .companion
            .status
            .as_ref()
            .and_then(|status| status.tailnet.as_ref())
        {
            None => TAILNET_SEARCHING.to_string(),
            Some(TailnetState::Exposed(endpoint)) => endpoint.url.clone(),
            Some(TailnetState::NotExposed { .. }) => {
                format!("Pas encore exposé sur le tailnet. {COMMAND_CAPTION}")
            }
            Some(TailnetState::Unknown(reason)) => {
                format!("{} {COMMAND_CAPTION}", unknown_tailnet_text(reason))
            }
        };
        let check = button(
            "settings-companion-recheck",
            CHECK,
            Some(Icon::Refresh),
            false,
            theme,
        )
        .on_click(cx.listener(|this, _, _, cx| this.recheck_tailnet(cx)));
        setting_row(TAILNET_LABEL, Some(&help), check, None, theme)
    }

    /// Sous la ligne de l'adresse : l'alerte Funnel et le QR code quand elle est exposée ; la commande à lancer et
    /// « Copier » sinon.
    fn tailnet_extras(&mut self, theme: &Theme, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let tailnet = self
            .companion
            .status
            .as_ref()
            .and_then(|status| status.tailnet.clone());
        match tailnet {
            None => Vec::new(),
            Some(TailnetState::Exposed(endpoint)) => {
                let mut extras = Vec::new();
                if endpoint.funnel {
                    extras.push(
                        div()
                            .pt(theme.px(10.))
                            .child(error_banner(FUNNEL_ALERT.into(), None, Vec::new(), theme))
                            .into_any_element(),
                    );
                }
                if let Some((_, modules)) = &self.companion.qr
                    && !modules.is_empty()
                {
                    extras.push(
                        div()
                            .flex()
                            .items_center()
                            .gap(theme.px(16.))
                            .py(theme.px(12.))
                            .border_b_1()
                            .border_color(white(SEPARATOR))
                            .child(qr_view(modules.clone(), theme))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_size(theme.text(12.))
                                    .text_color(white(TEXT_MIN))
                                    .child(QR_CAPTION),
                            )
                            .into_any_element(),
                    );
                }
                extras
            }
            Some(_) => {
                let Some(command) = self.companion.command.clone() else {
                    return Vec::new();
                };
                let copy = button(
                    "settings-companion-copy",
                    if self.companion.copied { COPIED } else { COPY },
                    Some(if self.companion.copied {
                        Icon::Check
                    } else {
                        Icon::ContentCopy
                    }),
                    false,
                    theme,
                )
                .on_click(cx.listener(|this, _, _, cx| this.copy_command(cx)));
                let mono = cx
                    .global::<gpui_component::Theme>()
                    .mono_font_family
                    .clone();
                vec![
                    div()
                        .flex()
                        .items_center()
                        .gap(theme.px(8.))
                        .py(theme.px(10.))
                        .border_b_1()
                        .border_color(white(SEPARATOR))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .px(theme.px(10.))
                                .py(theme.px(6.))
                                .rounded(theme.px(6.))
                                .bg(black(FIELD_BACKGROUND))
                                .font_family(mono)
                                .text_size(theme.text(12.))
                                .text_color(white(TEXT_FULL))
                                .child(SharedString::from(command)),
                        )
                        .child(copy)
                        .into_any_element(),
                ]
            }
        }
    }

    /// « Appairer un appareil » (ou « Annuler » quand un code est affiché).
    fn pairing_row(&mut self, theme: &Theme, cx: &mut Context<Self>) -> gpui::Div {
        let pairing = self
            .companion
            .status
            .as_ref()
            .is_some_and(|status| status.pairing.is_some());
        let control = if pairing {
            button("settings-companion-cancel", CANCEL, None, false, theme).on_click(
                cx.listener(|this, _, _, cx| this.send_companion(ServiceEvent::CancelPairing, cx)),
            )
        } else {
            button(
                "settings-companion-pair",
                label::PAIR_DEVICE,
                None,
                true,
                theme,
            )
            .on_click(
                cx.listener(|this, _, _, cx| this.send_companion(ServiceEvent::StartPairing, cx)),
            )
        };
        setting_row(DEVICES_LABEL, Some(PAIRING_HELP), control, None, theme)
    }

    /// Le code d'appairage en cours : mono 28/600, avec son compte à rebours en secondaire.
    fn pairing_code(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let PairingDisplay { code, expires_at } =
            self.companion.status.as_ref()?.pairing.clone()?;
        let remaining = expires_at
            .duration_since(SystemTime::now())
            .unwrap_or_default();
        let mono = cx
            .global::<gpui_component::Theme>()
            .mono_font_family
            .clone();
        Some(
            self.plain_row(theme)
                .gap(theme.px(16.))
                .child(
                    div()
                        .font_family(mono)
                        .text_size(theme.text(28.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(white(TEXT_FULL))
                        .child(SharedString::from(code)),
                )
                .child(
                    div()
                        .text_color(white(TEXT_SECONDARY))
                        .child(SharedString::from(countdown(remaining))),
                )
                .into_any_element(),
        )
    }

    /// La liste des appareils appairés : nom, date d'appairage, dernière activité, notifications, « Oublier ».
    fn devices_list(&mut self, theme: &Theme, cx: &mut Context<Self>) -> gpui::Div {
        let devices: Vec<DeviceSummary> = self
            .companion
            .status
            .as_ref()
            .map(|status| status.devices.clone())
            .unwrap_or_default();
        let mut list = div().flex().flex_col();
        if devices.is_empty() {
            return list.child(
                div()
                    .py(theme.px(10.))
                    .border_b_1()
                    .border_color(white(SEPARATOR))
                    .text_color(white(TEXT_MIN))
                    .child(NO_DEVICE),
            );
        }
        list = list.child(
            div()
                .flex()
                .items_center()
                .gap(theme.px(8.))
                .h(theme.px(28.))
                .border_b_1()
                .border_color(white(SEPARATOR))
                .child(
                    column_title("Nom", theme)
                        .w(theme.px(NAME_COLUMN))
                        .flex_none(),
                )
                .child(
                    column_title("Appairé le", theme)
                        .w(theme.px(DATE_COLUMN))
                        .flex_none(),
                )
                .child(
                    column_title("Dernière activité", theme)
                        .w(theme.px(ACTIVITY_COLUMN))
                        .flex_none(),
                )
                .child(
                    column_title("Notifications", theme)
                        .w(theme.px(NOTIFY_COLUMN))
                        .flex_none(),
                ),
        );
        let now = SystemTime::now();
        for (index, device) in devices.into_iter().enumerate() {
            let id = device.id.clone();
            let forget = button(
                SharedString::from(format!("settings-companion-forget-{index}")),
                FORGET,
                None,
                false,
                theme,
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.send_companion(ServiceEvent::ForgetDevice { id: id.clone() }, cx);
            }));
            list = list.child(
                div()
                    .flex()
                    .items_center()
                    .gap(theme.px(8.))
                    .h(theme.px(30.))
                    .border_b_1()
                    .border_color(white(SEPARATOR))
                    .child(
                        div()
                            .w(theme.px(NAME_COLUMN))
                            .flex_none()
                            .truncate()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(white(TEXT_PRIMARY))
                            .child(SharedString::from(device.name)),
                    )
                    .child(
                        div()
                            .w(theme.px(DATE_COLUMN))
                            .flex_none()
                            .text_color(white(TEXT_SECONDARY))
                            .child(SharedString::from(date_text(device.paired_at))),
                    )
                    .child(
                        div()
                            .w(theme.px(ACTIVITY_COLUMN))
                            .flex_none()
                            .text_color(white(TEXT_SECONDARY))
                            .child(SharedString::from(ago(now
                                .duration_since(device.last_seen_at)
                                .unwrap_or_default()))),
                    )
                    .child(
                        div()
                            .w(theme.px(NOTIFY_COLUMN))
                            .flex_none()
                            .text_color(white(TEXT_SECONDARY))
                            .child(if device.subscribed { "oui" } else { "non" }),
                    )
                    .child(div().flex_1())
                    .child(forget),
            );
        }
        list
    }
}

/// Le QR code, dessiné module par module : un fond blanc (la zone de silence en fait partie), un carré noir par module
/// sombre. Le côté d'un module est un nombre entier de pixels, pour des bords nets : le code reste lisible par l'appareil
/// photo, même mis à l'échelle de l'interface.
fn qr_view(modules: Vec<Vec<bool>>, theme: &Theme) -> impl IntoElement {
    let count = modules.len();
    let total = count + 2 * QR_QUIET;
    div()
        .flex_none()
        .size(theme.px(QR_SIDE))
        .rounded(theme.px(6.))
        .overflow_hidden()
        .child(
            canvas(
                |_, _, _| (),
                move |bounds, (), window, _| {
                    window.paint_quad(fill(bounds, white(1.)));
                    let module = (bounds.size.width.min(bounds.size.height) / total as f32).floor();
                    let origin = point(
                        bounds.origin.x + (bounds.size.width - module * total as f32) / 2.,
                        bounds.origin.y + (bounds.size.height - module * total as f32) / 2.,
                    );
                    for (y, row) in modules.iter().enumerate() {
                        for (x, dark) in row.iter().enumerate() {
                            if *dark {
                                let corner = point(
                                    origin.x + module * (x + QR_QUIET) as f32,
                                    origin.y + module * (y + QR_QUIET) as f32,
                                );
                                window.paint_quad(fill(
                                    Bounds::new(corner, size(module, module)),
                                    black(1.),
                                ));
                            }
                        }
                    }
                },
            )
            .size_full(),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_listen_failure_reads_as_one_short_french_sentence() {
        assert_eq!(failure_text(3910, "port_in_use"), "Port 3910 déjà pris");
        assert_eq!(
            failure_text(3910, "devices_unavailable: Permission denied (os error 13)"),
            "La liste des appareils est illisible."
        );
        assert_eq!(
            failure_text(4000, "bind_failed: Address not available (os error 49)"),
            "Impossible d'ouvrir le port 4000."
        );
        assert_eq!(
            failure_text(3910, "tmux_unavailable"),
            "tmux est introuvable : le service ne démarre pas."
        );
        for reason in [
            "thread_spawn_failed",
            "runtime_failed: no reactor",
            "something new",
        ] {
            assert_eq!(
                failure_text(3910, reason),
                "Le service n'a pas pu démarrer.",
                "{reason}"
            );
        }
        // Jamais le texte brut d'une erreur système à l'écran.
        for reason in [
            "devices_unavailable: os error 13",
            "bind_failed: os error 49",
        ] {
            assert!(!failure_text(3910, reason).contains("os error"), "{reason}");
        }
    }

    #[test]
    fn an_unreadable_tailnet_is_explained_without_the_raw_reason() {
        assert_eq!(
            unknown_tailnet_text("binary_not_found"),
            "Tailscale est introuvable sur ce Mac."
        );
        assert_eq!(unknown_tailnet_text("timeout"), "Tailscale ne répond pas.");
        assert_eq!(
            unknown_tailnet_text("spawn_failed: PermissionDenied"),
            "L'adresse tailnet n'a pas pu être lue."
        );
    }

    #[test]
    fn the_countdown_reads_minutes_and_seconds() {
        assert_eq!(countdown(Duration::from_secs(600)), "10:00");
        assert_eq!(countdown(Duration::from_secs(599)), "09:59");
        assert_eq!(countdown(Duration::from_secs(61)), "01:01");
        assert_eq!(countdown(Duration::ZERO), "00:00");
    }

    #[test]
    fn elapsed_time_reads_in_the_coarsest_fitting_unit() {
        assert_eq!(ago(Duration::from_secs(5)), "à l'instant");
        assert_eq!(ago(Duration::from_secs(60)), "il y a 1 min");
        assert_eq!(ago(Duration::from_secs(3599)), "il y a 59 min");
        assert_eq!(ago(Duration::from_secs(3600)), "il y a 1 h");
        assert_eq!(ago(Duration::from_secs(86400 * 3 + 5)), "il y a 3 j");
    }
}
