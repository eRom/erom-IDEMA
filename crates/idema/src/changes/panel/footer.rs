//! Le pied du volet Changes (spec P3 §4.6) : la branche, Push, le message de commit et Commit.

use super::*;

use futures::StreamExt;
use gpui::{AnyElement, FontWeight, relative};
use gpui_component::input::{Textarea, TextareaState};

use crate::changes::{BranchInfo, Group};
use crate::commit_message::client::{
    CancelHandle, DEFAULT_ENDPOINT, GenerationError, GenerationRequest, GenerationUpdate,
    spawn_generation,
};
use crate::commit_message::prompt::{self, PromptStats};
use crate::config::CommitMessageConfig;
use crate::errors::{GitFailure, explain_generation};
use crate::help_bubble::HelpBubble;
use crate::theme::{
    CONTROL, ERROR, FIELD_BACKGROUND, FIELD_RING, FOCUS, SEPARATOR, TEXT_DISABLED, TEXT_FULL,
    TEXT_MIN, TEXT_PRIMARY, TEXT_SECONDARY, TREE_BACKGROUND, black,
};

/// La bulle d'aide de Push quand le dépôt n'a pas de remote `origin`.
const NO_ORIGIN: &str = "Aucun remote origin.";
/// La bulle de Push quand la branche suit autre chose que `origin/<branche>` : Push n'enverrait pas là où elle suit.
const UPSTREAM_NOT_ORIGIN: &str = "L'amont n'est pas sur origin.";
/// La bulle de Push quand la branche distante suivie a été supprimée.
const UPSTREAM_GONE: &str = "L'amont n'existe plus.";
/// Ce que dit la ligne de branche en HEAD détachée.
const DETACHED: &str = "HEAD détachée";

/// Les cotes du pied (maquette) : retrait 10 et écart 8, le champ du message de 58 de haut (rayon 7, retrait 8 plus le
/// contour de 1, comme les champs de Fichiers), Push de 22, Commit de 26.
const FOOTER_PADDING: f32 = 10.;
const FOOTER_GAP: f32 = 8.;
const MESSAGE_HEIGHT: f32 = 58.;
const MESSAGE_RADIUS: f32 = 7.;
const MESSAGE_PADDING_X: f32 = 8.;
const MESSAGE_PADDING_Y: f32 = 6.;
const MESSAGE_TEXT: f32 = 12.5;
const MESSAGE_LINE: f32 = 17.;
const PUSH_HEIGHT: f32 = 22.;
const COMMIT_HEIGHT: f32 = 26.;
const BRANCH_ICON: f32 = 14.;
/// Le bouton « Générer le message » (maquette) : une case de 26, rayon 6, et l'icône de 17.
const GENERATE_SIZE: f32 = 26.;
const GENERATE_ICON: f32 = 17.;
/// Le raccourci de la génération, tel que la bulle l'écrit.
const GENERATE_SHORTCUT: &str = "\u{2325}\u{21E5}";
/// Les bulles du bouton de génération, hors celles d'un bouton éteint.
const GENERATE_HELP: &str = "Générer le message";
const STOP_HELP: &str = "Arrêter la génération";
const NO_API_KEY: &str = "Ajoute api_key dans la section [commit_message] de config.toml.";
const CONFLICT: &str = "Conflit en cours.";
const NOTHING_CHECKED: &str = "Aucun fichier coché.";

/// Ce que dit le bouton Push.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushState {
    /// Rien à pousser, le bouton n'a pas de raison d'être : la branche n'existe pas encore (aucun commit).
    Hidden,
    /// Éteint, avec le texte de sa bulle d'aide (vide : pas de bulle, la raison se lit sur l'écran).
    Disabled(&'static str),
    /// Allumé, avec le nombre de commits d'avance sur la branche amont.
    Ahead(u32),
    /// Allumé sans compteur : la branche n'a pas d'amont, Push la publie.
    Publish,
}

/// L'état de Push. Sans `origin` il est éteint, quoi qu'il y ait à pousser ; en HEAD détachée aussi ; sans amont, Push
/// publie la branche. Avec un amont, Push n'envoie que vers `origin/<branche>` (écart E64) : un autre amont, ou un amont
/// supprimé, l'éteint avec sa bulle ; une branche suivie sans commit d'avance n'a rien à pousser.
pub fn push_state(branch: &BranchInfo, has_origin: bool) -> PushState {
    if !has_origin {
        PushState::Disabled(NO_ORIGIN)
    } else if branch.head.is_none() {
        PushState::Disabled("")
    } else if !branch.has_commits {
        PushState::Hidden
    } else if let Some(upstream) = &branch.upstream {
        let expected = branch.head.as_deref().map(|head| format!("origin/{head}"));
        if expected.as_deref() != Some(upstream.as_str()) {
            PushState::Disabled(UPSTREAM_NOT_ORIGIN)
        } else if branch.upstream_gone {
            PushState::Disabled(UPSTREAM_GONE)
        } else if branch.ahead == 0 {
            PushState::Disabled("")
        } else {
            PushState::Ahead(branch.ahead)
        }
    } else {
        PushState::Publish
    }
}

/// Ce que dit le bouton « Générer le message » (spec B-11 §4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenerateState {
    /// Pas de section `[commit_message]` allumée : le pied n'a pas de bouton.
    Hidden,
    /// Éteint, avec le texte de sa bulle d'aide.
    Disabled(&'static str),
    Ready,
    /// Une génération court : le bouton est un Stop.
    Generating,
}

/// L'état du bouton de génération. `settings` est la section allumée (`CommitMessageSettings::active`). Une case mixte suffit
/// pour générer : une part du fichier est dans l'index.
pub fn generate_state(
    settings: Option<&CommitMessageConfig>,
    changes: &[Change],
    generating: bool,
) -> GenerateState {
    let Some(settings) = settings else {
        return GenerateState::Hidden;
    };
    if generating {
        return GenerateState::Generating;
    }
    if settings
        .api_key
        .as_deref()
        .is_none_or(|key| key.trim().is_empty())
    {
        return GenerateState::Disabled(NO_API_KEY);
    }
    if changes
        .iter()
        .any(|change| change.group == Group::Conflicts)
    {
        return GenerateState::Disabled(CONFLICT);
    }
    if !changes
        .iter()
        .any(|change| matches!(change.check, Check::Checked | Check::Mixed))
    {
        return GenerateState::Disabled(NOTHING_CHECKED);
    }
    GenerateState::Ready
}

/// Ce qui s'écrit avant le premier fragment : un saut de ligne quand le champ porte déjà un sujet (Zed), rien sinon.
pub fn append_prefix(field: &str) -> &'static str {
    if prompt::subject_line(field).is_some() {
        "\n"
    } else {
        ""
    }
}

/// Un fragment appartient-il à la génération courante ? Un fragment d'une génération arrêtée, arrivé après coup, est jeté.
pub fn accepts_update(current: u64, update_generation: u64) -> bool {
    current == update_generation
}

/// Le nom d'une issue en erreur au journal (spec B-11 §4.6).
fn outcome_name(error: GenerationError) -> &'static str {
    match error {
        GenerationError::KeyRejected => "key_rejected",
        GenerationError::ModelUnavailable => "model_unavailable",
        GenerationError::QuotaExceeded => "quota_exceeded",
        GenerationError::Unreachable => "unreachable",
        GenerationError::CurlMissing => "curl_missing",
        GenerationError::Failed => "failed",
    }
}

/// La ligne du journal d'une génération finie (spec B-11 §4.6) : jamais le prompt, la réponse ni la clé. Sans lectures Git
/// revenues (`stats` vide), le diff compte pour rien.
fn generation_line(
    model: &str,
    stats: Option<&PromptStats>,
    took_ms: u128,
    outcome: &str,
) -> String {
    let (diff_bytes, compressed, omitted) = stats.map_or((0, false, false), |stats| {
        (stats.diff_bytes, stats.compressed, stats.omitted)
    });
    format!(
        "commit_message model={model} diff_bytes={diff_bytes} compressed={compressed} omitted={omitted} took_ms={took_ms} outcome={outcome}"
    )
}

/// Commit est allumé quand le message n'est pas vide et qu'une part de l'index est cochée (une case mixte compte : une
/// part du fichier est dans l'index), et qu'aucune génération n'écrit dans le champ.
pub fn commit_enabled(message: &str, changes: &[Change], generating: bool) -> bool {
    !generating
        && !message.trim().is_empty()
        && changes
            .iter()
            .any(|change| matches!(change.check, Check::Checked | Check::Mixed))
}

/// Le champ se vide après un commit réussi seulement s'il porte encore le message commité : ce que l'utilisateur a tapé depuis
/// le départ du commit n'est pas à perdre.
fn should_clear_message(current: &str, committed: &str) -> bool {
    current == committed
}

/// Ajoute `text` à la fin du champ, sans toucher à ce qui y est : la sélection de l'utilisateur reste où elle est, sauf un curseur déjà
/// en fin de texte, qui suit le texte qui arrive.
fn append_text(field: &Entity<TextareaState>, text: &str, window: &mut Window, cx: &mut App) {
    field.update(cx, |state, cx| {
        let end = state.text().len();
        let before = state.selected_range();
        state.set_selected_range(end..end, cx);
        state.insert(text.to_string(), window, cx);
        if before.end < end {
            state.set_selected_range(before, cx);
        }
    });
}

impl ChangesPanel {
    /// Le pied : la branche et Push, le message, puis Commit. Il est posé sous la liste (maquette), ou en bas du volet quand
    /// la liste n'est pas peinte (`standalone` : il prend alors le creux de la liste lui-même).
    pub(super) fn render_footer(
        &self,
        theme: &Theme,
        standalone: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Load::Loaded(snapshot) = &self.load else {
            return div().into_any_element();
        };
        let branch = snapshot
            .branch
            .head
            .clone()
            .unwrap_or_else(|| DETACHED.to_string());
        let push = push_state(&snapshot.branch, snapshot.has_origin);
        let message = self.message().read(cx).value();
        // Commit se juge sur les cases telles que la liste les montre (le clic compte aussitôt), pas sur la dernière
        // lecture : sinon il resterait éteint jusqu'à la relecture qui confirme la case.
        let displayed = list_view::with_pending(&snapshot.changes, &self.pending_checks);
        let generating = self.generation.is_some();
        let generate = generate_state(
            cx.try_global::<CommitMessageSettings>()
                .and_then(CommitMessageSettings::active),
            &displayed,
            generating,
        );
        // Pendant le commit le bouton garde son allure : seul son texte change, et il ne répond plus.
        let commit_live = self.committing || commit_enabled(&message, &displayed, generating);
        let branch_row = div()
            .flex()
            .items_center()
            .gap(theme.px(6.))
            .text_size(theme.text(12.))
            .text_color(white(TEXT_SECONDARY))
            .child(
                svg()
                    .path(Icon::ForkRight.path())
                    .size(theme.px(BRANCH_ICON))
                    .flex_none()
                    .text_color(white(TEXT_SECONDARY)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .child(branch),
            )
            .children(self.render_push(push, theme, cx));
        let commit_button = {
            let label = if self.committing {
                "Commit\u{2026}"
            } else if generating {
                "Génération\u{2026}"
            } else {
                "Commit"
            };
            let button = div()
                .id("changes-commit")
                .flex_none()
                .flex()
                .items_center()
                .h(theme.px(COMMIT_HEIGHT))
                .px(theme.px(12.))
                .rounded(theme.px(6.))
                .text_size(theme.text(12.5))
                .font_weight(FontWeight::MEDIUM)
                .child(label);
            if commit_live {
                button
                    .bg(color(FOCUS))
                    .text_color(white(TEXT_FULL))
                    .cursor_pointer()
                    .hover(|style| style.opacity(0.88))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _: &MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            this.commit(window, cx);
                        }),
                    )
            } else {
                button
                    .bg(focus(TOGGLE_ACTIVE))
                    .text_color(white(TEXT_DISABLED))
            }
        };
        div()
            .flex_none()
            .w_full()
            .flex()
            .flex_col()
            .gap(theme.px(FOOTER_GAP))
            .p(theme.px(FOOTER_PADDING))
            .border_t_1()
            .border_color(white(SEPARATOR))
            .when(standalone, |footer| footer.bg(black(TREE_BACKGROUND)))
            .child(branch_row)
            .child(self.render_message_box(theme, cx))
            .child(
                div()
                    .flex()
                    .items_center()
                    .children(self.render_generate(generate, theme, cx))
                    // Un ressort plutôt que `justify_between` : sans bouton de génération, Commit reste à droite.
                    .child(div().flex_1())
                    .child(commit_button),
            )
            .into_any_element()
    }

    /// Le bouton de génération (maquette : une case de 26, l'icône `edit_note` à 55 %, fond à 7 % et icône blanche au
    /// survol), ou rien sans section allumée. Éteint, il garde son icône et une bulle dit pourquoi ; pendant une génération
    /// il devient un Stop rouge.
    fn render_generate(
        &self,
        state: GenerateState,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        // L'état Stop prend un autre `id` que l'état prêt : GPUI rattache la bulle à l'élément, et avec le même `id` la bulle
        // « Générer » resterait affichée souris immobile après le clic.
        let button = |id: &'static str| {
            div()
                .id(id)
                .group("changes-generate")
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .size(theme.px(GENERATE_SIZE))
                .rounded(theme.px(6.))
        };
        let icon = |icon: Icon| svg().path(icon.path()).size(theme.px(GENERATE_ICON));
        Some(match state {
            GenerateState::Hidden => return None,
            GenerateState::Disabled(help) => button("changes-generate")
                .child(icon(Icon::EditNote).text_color(white(TEXT_DISABLED)))
                .tooltip(move |_, cx| cx.new(|_| HelpBubble::new(help)).into())
                .into_any_element(),
            GenerateState::Ready => button("changes-generate")
                .cursor_pointer()
                .hover(|style| style.bg(white(HOVER_STRONG)))
                .child(
                    icon(Icon::EditNote)
                        .text_color(white(TEXT_MIN))
                        .group_hover("changes-generate", |style| {
                            style.text_color(white(TEXT_FULL))
                        }),
                )
                .tooltip(move |_, cx| {
                    cx.new(|_| HelpBubble::with_keys(GENERATE_HELP, GENERATE_SHORTCUT))
                        .into()
                })
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        this.generate(window, cx);
                    }),
                )
                .into_any_element(),
            GenerateState::Generating => button("changes-generate-stop")
                .cursor_pointer()
                .hover(|style| style.bg(white(HOVER_STRONG)))
                .child(icon(Icon::Stop).text_color(color(ERROR)))
                .tooltip(move |_, cx| cx.new(|_| HelpBubble::new(STOP_HELP)).into())
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                        this.stop_generation("stopped", cx);
                    }),
                )
                .into_any_element(),
        })
    }

    /// Le bouton Push, ou rien quand il n'a pas de raison d'être. Allumé, il pousse au clic ; éteint, il garde son texte
    /// et, sans `origin`, une bulle dit pourquoi.
    fn render_push(
        &self,
        push: PushState,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let (label, live, help) = match push {
            PushState::Hidden => return None,
            PushState::Disabled(help) => ("Push".to_string(), false, help),
            PushState::Ahead(count) => (format!("\u{2191}{count} Push"), true, ""),
            PushState::Publish => ("Push".to_string(), true, ""),
        };
        let label = if self.pushing {
            "Push\u{2026}".to_string()
        } else {
            label
        };
        let button = div()
            .id("changes-push")
            .flex_none()
            .flex()
            .items_center()
            .h(theme.px(PUSH_HEIGHT))
            .px(theme.px(8.))
            .rounded(theme.px(5.))
            .bg(white(HOVER_STRONG))
            .text_size(theme.text(11.5))
            .child(label);
        Some(if live {
            button
                .text_color(white(TEXT_PRIMARY))
                .cursor_pointer()
                .hover(|style| style.bg(white(CONTROL)))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        this.push(window, cx);
                    }),
                )
                .into_any_element()
        } else {
            button
                .text_color(white(TEXT_DISABLED))
                .when(!help.is_empty(), |button| {
                    button.tooltip(move |_, cx| cx.new(|_| HelpBubble::new(help)).into())
                })
                .into_any_element()
        })
    }

    /// La boîte du message : le champ multi-ligne du kit dans le creux des champs de Fichiers. `↩` va à la ligne (le kit),
    /// `⌘↩` commite (`Commit`, lié à `ChangesMessage > Input`). Un appui gauche donne le focus au champ ; un appui droit
    /// y est arrêté (pas de menu de la racine).
    fn render_message_box(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let field_focus = self.message().focus_handle(cx);
        div()
            .id("changes-message-box")
            .key_context(MESSAGE_KEY_CONTEXT)
            .flex_none()
            .w_full()
            .h(theme.px(MESSAGE_HEIGHT))
            .px(theme.px(MESSAGE_PADDING_X))
            .py(theme.px(MESSAGE_PADDING_Y))
            .rounded(theme.px(MESSAGE_RADIUS))
            .border_1()
            .border_color(white(FIELD_RING))
            .bg(black(FIELD_BACKGROUND))
            .cursor_text()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |_, _: &MouseDownEvent, window, cx| {
                    window.focus(&field_focus, cx);
                    // Le corps du volet reprendrait le focus à la remontée de l'appui.
                    window.prevent_default();
                }),
            )
            .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
            .on_action(cx.listener(|this, _: &Commit, window, cx| this.commit(window, cx)))
            .on_action(cx.listener(|this, _: &GenerateMessage, window, cx| {
                this.generate(window, cx);
            }))
            .child(
                Textarea::new(self.message())
                    .appearance(false)
                    .bordered(false)
                    .h(relative(1.))
                    .px_0()
                    .py_0()
                    .text_size(theme.text(MESSAGE_TEXT))
                    .line_height(theme.text(MESSAGE_LINE))
                    .text_color(white(TEXT_FULL)),
            )
    }

    /// Commit (bouton ou `⌘↩`) : commite l'index avec le message, hors du fil d'interface. Il ne répond pas quand il est
    /// éteint, qu'une génération du message écrit dans le champ, qu'un commit est en cours, ou qu'une case est encore en train
    /// d'être écrite (l'index ne dirait pas tout).
    pub(super) fn commit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.generation.is_some() || self.committing || self.writing || !self.writes.is_empty() {
            return;
        }
        let Load::Loaded(snapshot) = &self.load else {
            return;
        };
        let message = self.message().read(cx).value().to_string();
        // Les cases comme la liste les montre : `⌘↩` juste après un clic, pendant la relecture qui le confirme, ne se perd
        // pas (l'écriture de la case est finie, la lecture de l'index n'est pas encore revenue).
        let displayed = list_view::with_pending(&snapshot.changes, &self.pending_checks);
        if !commit_enabled(&message, &displayed, false) {
            return;
        }
        let root = snapshot.root.clone();
        // Le champ du message qui part, par sa clé : la cible peut changer avant le retour de Git, et c'est le message de
        // cette cible-ci qui se vide, pas celui de la cible alors montrée.
        let key = self.target.dir(&self.project).to_path_buf();
        self.committing = true;
        cx.notify();
        // Le geste, au journal avant l'écriture : AC-72 exige cette ligne devant toute commande qui écrit. Jamais le texte.
        hud::log_event("changes gesture commit");
        cx.spawn_in(window, async move |this, cx| {
            let committed = message.clone();
            let result = cx
                .background_spawn(async move { ops::commit(&root, &message) })
                .await;
            let _ = this.update_in(cx, |panel, window, cx| {
                panel.commit_done(result, &committed, &key, window, cx)
            });
        })
        .detach();
    }

    /// Le commit est revenu. Réussi, le champ de la cible commitée (`key`) se vide, sauf si l'utilisateur y a tapé autre chose
    /// pendant que Git travaillait (`committed` est le message parti) ; refusé (un hook, une signature), le message reste et
    /// la bulle donne la sortie de Git. Dans les deux cas la liste se relit. Le journal ne garde jamais le message, ni sa
    /// longueur.
    fn commit_done(
        &mut self,
        result: Result<(), ops::GitFailureRaw>,
        committed: &str,
        key: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.committing = false;
        match result {
            Ok(()) => {
                hud::log_event("changes commit done");
                if let Some(field) = self.messages.get(key) {
                    let current = field.read(cx).value().to_string();
                    if should_clear_message(&current, committed) {
                        field.update(cx, |message, cx| message.clean(window, cx));
                    }
                }
            }
            Err(error) => {
                hud::log_event("changes commit failed");
                let Explained { text, detail } = explain_git(error.failure, &error.raw);
                alert(window, cx, &text, detail.as_deref());
            }
        }
        cx.notify();
        self.refresh(cx);
    }

    /// La fonction est-elle toujours allumée ? Un `config.toml` rechargé sans section `[commit_message]` l'éteint.
    pub(super) fn generation_allowed(&self, cx: &Context<Self>) -> bool {
        cx.try_global::<CommitMessageSettings>()
            .and_then(CommitMessageSettings::active)
            .is_some()
    }

    /// Générer le message (bouton ou `⌥⇥`) : lit l'index hors du fil d'interface, envoie le prompt à Gemini et écrit la
    /// réponse au fil de l'eau à la fin du champ de la cible où le geste a commencé. Éteint, le geste ne fait rien ; pendant
    /// une génération il l'arrête d'abord, puis en relance une (Zed).
    pub(super) fn generate(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(settings) = cx
            .try_global::<CommitMessageSettings>()
            .and_then(CommitMessageSettings::active)
            .cloned()
        else {
            return;
        };
        // L'index doit être dit entièrement : ni commit en cours, ni case encore en train d'être écrite.
        if self.committing || self.writing || !self.writes.is_empty() {
            return;
        }
        self.stop_generation("stopped", cx);
        let Load::Loaded(snapshot) = &self.load else {
            return;
        };
        let displayed = list_view::with_pending(&snapshot.changes, &self.pending_checks);
        let Some(api_key) = settings.api_key.clone() else {
            return;
        };
        if generate_state(Some(&settings), &displayed, false) != GenerateState::Ready {
            return;
        }
        let root = snapshot.root.clone();
        // Le champ qui reçoit le texte, par sa clé : la cible peut changer en route (la génération s'arrête alors), et
        // jamais le texte n'ira dans le champ alors montré.
        let key = self.target.dir(&self.project).to_path_buf();
        let field = self.message().read(cx).value().to_string();
        self.generation_counter += 1;
        let id = self.generation_counter;
        let model = settings.model.clone();
        // Le geste, au journal avant les lectures de l'index. Jamais le texte du champ.
        hud::log_event("changes gesture generate");
        let task = cx.spawn_in(window, async move |this, cx| {
            let gather_settings = settings.clone();
            let gathered = cx
                .background_spawn(async move { prompt::gather(&root, &gather_settings, &field) })
                .await;
            let (prompt, stats) = match gathered {
                Ok(gathered) => gathered,
                Err(error) => {
                    let _ = this.update_in(cx, |panel, window, cx| {
                        panel.fail_generation(id, GenerationError::Failed, &error.raw, window, cx)
                    });
                    return;
                }
            };
            let request = GenerationRequest {
                endpoint: settings
                    .endpoint
                    .clone()
                    .unwrap_or_else(|| DEFAULT_ENDPOINT.to_string()),
                model: settings.model.clone(),
                api_key,
                prompt,
            };
            let (cancel, mut updates) = spawn_generation(request);
            // La poignée part dans le volet ; si la génération n'est plus la courante, elle y est lâchée, donc annulée.
            if this
                .update(cx, |panel, _| panel.generation_started(id, cancel, stats))
                .ok()
                != Some(true)
            {
                return;
            }
            while let Some(update) = updates.next().await {
                let applied = this.update_in(cx, |panel, window, cx| {
                    panel.generation_update(id, update, window, cx)
                });
                if applied.is_err() {
                    return;
                }
            }
            // Le canal s'est fermé sans `Done` ni `Failed` (le fil de lecture est mort) : la génération ne reste pas pendue.
            let _ = this.update(cx, |panel, cx| {
                if panel
                    .generation
                    .as_ref()
                    .is_some_and(|running| running.id == id)
                {
                    panel.stop_generation("failed", cx);
                }
            });
        });
        self.generation = Some(RunningGeneration {
            id,
            key,
            cancel: None,
            started: Instant::now(),
            model,
            stats: None,
            received: false,
            _task: task,
        });
        cx.notify();
    }

    /// Les lectures de l'index sont revenues et curl est lancé : la poignée d'annulation et les chiffres du journal rejoignent
    /// la génération. `false` : ce n'est plus la courante (arrêtée pendant les lectures), la poignée est lâchée donc annulée.
    fn generation_started(&mut self, id: u64, cancel: CancelHandle, stats: PromptStats) -> bool {
        match &mut self.generation {
            Some(running) if accepts_update(running.id, id) => {
                running.cancel = Some(cancel);
                running.stats = Some(stats);
                true
            }
            _ => false,
        }
    }

    /// Un message de la génération `id`. Un message d'une génération qui n'est plus la courante est jeté. Un fragment de texte
    /// va à la fin du champ de la clé de la génération ; la réponse n'est pas nettoyée.
    fn generation_update(
        &mut self,
        id: u64,
        update: GenerationUpdate,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(running) = &mut self.generation else {
            return;
        };
        if !accepts_update(running.id, id) {
            return;
        }
        match update {
            GenerationUpdate::Text(text) => {
                if text.is_empty() {
                    return;
                }
                let Some(field) = self.messages.get(&running.key) else {
                    return;
                };
                // Le saut de ligne qui sépare du texte présent précède le premier fragment seulement.
                let prefix = if running.received {
                    ""
                } else {
                    append_prefix(&field.read(cx).value())
                };
                running.received = true;
                append_text(field, &format!("{prefix}{text}"), window, cx);
                cx.notify();
            }
            GenerationUpdate::Done => self.stop_generation("ok", cx),
            GenerationUpdate::Failed { error, raw } => {
                self.fail_generation(id, error, &raw, window, cx);
            }
        }
    }

    /// La génération `id` a échoué (Gemini, curl, ou la lecture de l'index) : elle se termine, le texte déjà reçu reste, et la
    /// bulle dit pourquoi.
    fn fail_generation(
        &mut self,
        id: u64,
        error: GenerationError,
        raw: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(running) = &self.generation else {
            return;
        };
        if !accepts_update(running.id, id) {
            return;
        }
        let model = running.model.clone();
        self.stop_generation(outcome_name(error), cx);
        let Explained { text, detail } = explain_generation(error, raw, &model);
        alert(window, cx, &text, detail.as_deref());
    }

    /// Termine la génération en cours, s'il y en a une : curl est coupé, le texte déjà reçu reste dans le champ, et le journal
    /// note l'issue (`ok`, `stopped`, ou le nom de l'erreur). Les fragments qui arriveraient encore sont jetés.
    pub(super) fn stop_generation(&mut self, outcome: &str, cx: &mut Context<Self>) {
        let Some(running) = self.generation.take() else {
            return;
        };
        if let Some(cancel) = &running.cancel {
            cancel.cancel();
        }
        hud::log_event(&generation_line(
            &running.model,
            running.stats.as_ref(),
            running.started.elapsed().as_millis(),
            outcome,
        ));
        cx.notify();
    }

    /// Push (bouton) : pousse la branche vers `origin/<branche>`, avec `-u` quand elle n'a pas d'amont. Rien n'est relancé ni
    /// forcé.
    pub(super) fn push(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pushing {
            return;
        }
        let Load::Loaded(snapshot) = &self.load else {
            return;
        };
        if !matches!(
            push_state(&snapshot.branch, snapshot.has_origin),
            PushState::Ahead(_) | PushState::Publish
        ) {
            return;
        }
        let Some(branch) = snapshot.branch.head.clone() else {
            return;
        };
        let root = snapshot.root.clone();
        let has_upstream = snapshot.branch.upstream.is_some();
        self.pushing = true;
        cx.notify();
        hud::log_event("changes gesture push");
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move { ops::push(&root, &branch, has_upstream) })
                .await;
            let _ = this.update_in(cx, |panel, window, cx| panel.push_done(result, window, cx));
        })
        .detach();
    }

    /// Le push est revenu : le compteur se relit. Refusé parce que la branche distante a avancé, ou échoué (droits,
    /// identifiants), la bulle donne la sortie de Git.
    fn push_done(
        &mut self,
        result: Result<(), ops::GitFailureRaw>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pushing = false;
        match result {
            Ok(()) => hud::log_event("changes push done"),
            Err(error) => {
                hud::log_event(if error.failure == GitFailure::PushRejected {
                    "changes push refused"
                } else {
                    "changes push failed"
                });
                let Explained { text, detail } = explain_git(error.failure, &error.raw);
                alert(window, cx, &text, detail.as_deref());
            }
        }
        cx.notify();
        self.refresh(cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::changes::{Check, Group, Letter};

    fn branch(head: Option<&str>, upstream: Option<&str>, ahead: u32) -> BranchInfo {
        BranchInfo {
            head: head.map(str::to_string),
            has_commits: true,
            upstream: upstream.map(str::to_string),
            upstream_gone: false,
            ahead,
        }
    }

    fn change(check: Check) -> Change {
        Change {
            path: PathBuf::from("a.md"),
            orig_path: None,
            group: Group::Modified,
            letter: Letter::Modified,
            check,
        }
    }

    #[test]
    fn commit_is_enabled_only_with_a_message_and_a_checked_file() {
        let checked = [change(Check::Unchecked), change(Check::Checked)];
        let unchecked = [change(Check::Unchecked), change(Check::NoBox)];
        assert!(commit_enabled("docs: notes", &checked, false));
        // Un message fait d'espaces et de retours à la ligne seulement est vide.
        assert!(!commit_enabled("  \n\t ", &checked, false));
        assert!(!commit_enabled("", &checked, false));
        // Sans fichier coché, rien à commiter.
        assert!(!commit_enabled("docs: notes", &unchecked, false));
        assert!(!commit_enabled("docs: notes", &[], false));
        // Une case mixte (stagé puis retouché) a une part dans l'index.
        assert!(commit_enabled(
            "docs: notes",
            &[change(Check::Mixed)],
            false
        ));
    }

    #[test]
    fn commit_is_refused_while_generating() {
        let checked = [change(Check::Checked)];
        assert!(commit_enabled("docs: x", &checked, false));
        assert!(!commit_enabled("docs: x", &checked, true));
    }

    /// La section `[commit_message]` allumée, avec ou sans clé.
    fn settings(api_key: Option<&str>) -> CommitMessageSettings {
        let key = api_key.map_or(String::new(), |key| format!("api_key = \"{key}\"\n"));
        let text = format!("[commit_message]\nenabled = true\n{key}");
        CommitMessageSettings(crate::config::parse_config(&text).unwrap().commit_message)
    }

    fn conflict() -> Change {
        Change {
            group: Group::Conflicts,
            letter: Letter::Conflict,
            ..change(Check::NoBox)
        }
    }

    #[test]
    fn the_button_is_hidden_without_an_active_section() {
        let checked = [change(Check::Checked)];
        assert_eq!(generate_state(None, &checked, false), GenerateState::Hidden);
        // Section présente mais éteinte : l'état se lit sur `active()`, qui ne la rend pas.
        let off = CommitMessageSettings(
            crate::config::parse_config(
                "[commit_message]\nenabled = false\napi_key = \"fake-key-for-tests\"\n",
            )
            .unwrap()
            .commit_message,
        );
        assert_eq!(
            generate_state(off.active(), &checked, false),
            GenerateState::Hidden
        );
        // Même pendant une génération, sans section allumée (config rechargée) il n'y a plus de bouton.
        assert_eq!(generate_state(None, &checked, true), GenerateState::Hidden);
    }

    #[test]
    fn the_button_says_why_it_is_off() {
        let checked = [change(Check::Checked)];
        let keyed = settings(Some("fake-key-for-tests"));
        let keyless = settings(None);
        let blank = settings(Some("   "));
        for settings in [&keyless, &blank] {
            assert_eq!(
                generate_state(settings.active(), &checked, false),
                GenerateState::Disabled(
                    "Ajoute api_key dans la section [commit_message] de config.toml."
                )
            );
        }
        // Un conflit dans la cible, même avec une case cochée à côté.
        assert_eq!(
            generate_state(keyed.active(), &[change(Check::Checked), conflict()], false),
            GenerateState::Disabled("Conflit en cours.")
        );
        // Aucune case cochée ni mixte.
        assert_eq!(
            generate_state(
                keyed.active(),
                &[change(Check::Unchecked), change(Check::NoBox)],
                false
            ),
            GenerateState::Disabled("Aucun fichier coché.")
        );
        assert_eq!(
            generate_state(keyed.active(), &[], false),
            GenerateState::Disabled("Aucun fichier coché.")
        );
        // La clé manquante passe avant le reste : c'est la première chose à régler.
        assert_eq!(
            generate_state(keyless.active(), &[], false),
            GenerateState::Disabled(
                "Ajoute api_key dans la section [commit_message] de config.toml."
            )
        );
    }

    #[test]
    fn a_mixed_box_is_enough_to_generate() {
        let keyed = settings(Some("fake-key-for-tests"));
        assert_eq!(
            generate_state(keyed.active(), &[change(Check::Mixed)], false),
            GenerateState::Ready
        );
        assert_eq!(
            generate_state(keyed.active(), &[change(Check::Checked)], false),
            GenerateState::Ready
        );
    }

    #[test]
    fn a_running_generation_turns_the_button_into_a_stop() {
        let keyed = settings(Some("fake-key-for-tests"));
        // Même sans case cochée : Stop doit rester possible.
        assert_eq!(
            generate_state(keyed.active(), &[], true),
            GenerateState::Generating
        );
    }

    #[test]
    fn a_newline_separates_the_response_from_a_subject_already_typed() {
        assert_eq!(append_prefix(""), "");
        assert_eq!(append_prefix("  \n"), "");
        assert_eq!(append_prefix("docs: notes"), "\n");
        assert_eq!(append_prefix("docs: notes\n\ncorps"), "\n");
    }

    #[test]
    fn late_updates_of_a_stopped_generation_are_dropped() {
        assert!(!accepts_update(3, 2));
        assert!(accepts_update(3, 3));
        assert!(!accepts_update(3, 4));
    }

    #[test]
    fn the_journal_line_names_the_outcome_and_nothing_else() {
        let stats = PromptStats {
            diff_bytes: 1234,
            compressed: true,
            omitted: false,
        };
        assert_eq!(
            generation_line("gemini-x", Some(&stats), 87, "ok"),
            "commit_message model=gemini-x diff_bytes=1234 compressed=true omitted=false took_ms=87 outcome=ok"
        );
        // Arrêtée avant le retour de Git : aucun chiffre du diff.
        assert_eq!(
            generation_line("gemini-x", None, 5, "stopped"),
            "commit_message model=gemini-x diff_bytes=0 compressed=false omitted=false took_ms=5 outcome=stopped"
        );
    }

    #[test]
    fn every_generation_error_has_its_own_snake_case_outcome() {
        let named = [
            (GenerationError::KeyRejected, "key_rejected"),
            (GenerationError::ModelUnavailable, "model_unavailable"),
            (GenerationError::QuotaExceeded, "quota_exceeded"),
            (GenerationError::Unreachable, "unreachable"),
            (GenerationError::CurlMissing, "curl_missing"),
            (GenerationError::Failed, "failed"),
        ];
        for (error, name) in named {
            assert_eq!(outcome_name(error), name);
        }
    }

    #[test]
    fn the_message_is_cleared_only_when_it_is_still_the_committed_one() {
        assert!(should_clear_message("Ajoute les notes", "Ajoute les notes"));
        // L'utilisateur a continué de taper pendant que Git commitait : ce texte est à lui.
        assert!(!should_clear_message(
            "Ajoute les notes, suite",
            "Ajoute les notes"
        ));
    }

    #[test]
    fn push_button_state() {
        // Branche suivie, rien d'avance : éteint, sans bulle.
        assert_eq!(
            push_state(&branch(Some("main"), Some("origin/main"), 0), true),
            PushState::Disabled("")
        );
        assert_eq!(
            push_state(&branch(Some("main"), Some("origin/main"), 2), true),
            PushState::Ahead(2)
        );
        // Pas d'amont : Push publie.
        assert_eq!(
            push_state(&branch(Some("pub"), None, 0), true),
            PushState::Publish
        );
        // Pas d'origin : éteint, et la bulle le dit, même avec des commits d'avance.
        assert_eq!(
            push_state(&branch(Some("main"), Some("origin/main"), 2), false),
            PushState::Disabled("Aucun remote origin.")
        );
        assert_eq!(
            push_state(&branch(Some("main"), None, 0), false),
            PushState::Disabled("Aucun remote origin.")
        );
        // HEAD détachée : éteint.
        assert_eq!(
            push_state(&branch(None, None, 0), true),
            PushState::Disabled("")
        );
        // Un amont qui n'est pas origin/<branche> : éteint, avec sa bulle, même avec des commits d'avance.
        for upstream in ["origin/other", "upstream/main", "main"] {
            assert_eq!(
                push_state(&branch(Some("main"), Some(upstream), 2), true),
                PushState::Disabled("L'amont n'est pas sur origin."),
                "{upstream}"
            );
        }
        // La branche suit son homonyme d'origin, mais il a été supprimé : éteint, avec sa bulle.
        let gone = BranchInfo {
            upstream_gone: true,
            ..branch(Some("feat"), Some("origin/feat"), 0)
        };
        assert_eq!(
            push_state(&gone, true),
            PushState::Disabled("L'amont n'existe plus.")
        );
        // Aucun commit : la branche n'existe pas encore.
        let unborn = BranchInfo {
            has_commits: false,
            ..branch(Some("main"), None, 0)
        };
        assert_eq!(push_state(&unborn, true), PushState::Hidden);
    }
}
