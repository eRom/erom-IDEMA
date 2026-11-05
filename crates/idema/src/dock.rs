//! Badge du Dock et son d'attente (spec §5.4). GPUI n'expose ni l'un ni l'autre : AppKit, par objc2.

use std::sync::atomic::{AtomicUsize, Ordering};

use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSSound};
use objc2_foundation::NSString;

use crate::hud;

/// Son système discret joué à chaque passage en « attend » d'une session non vue.
const WAIT_SOUND: &str = "Tink";

/// Texte du badge : rien à zéro, sinon le nombre.
pub fn badge_label(count: usize) -> Option<String> {
    (count > 0).then(|| count.to_string())
}

/// La dernière valeur posée ; `usize::MAX` tant qu'aucune ne l'a été.
static LAST_BADGE: AtomicUsize = AtomicUsize::new(usize::MAX);

/// Retient `count` et dit s'il diffère de la valeur précédente.
fn badge_changed(count: usize) -> bool {
    LAST_BADGE.swap(count, Ordering::Relaxed) != count
}

/// Pose le badge du Dock et écrit `dock badge count=<n>` au journal quand la valeur change. Sans effet hors du fil
/// principal, où AppKit l'interdit.
pub fn set_badge(count: usize) {
    let Some(marker) = MainThreadMarker::new() else {
        return;
    };
    if badge_changed(count) {
        hud::log_event(&format!("dock badge count={count}"));
    }
    let label = badge_label(count).map(|text| NSString::from_str(&text));
    NSApplication::sharedApplication(marker)
        .dockTile()
        .setBadgeLabel(label.as_deref());
}

/// Joue le son d'attente. Faux si le son n'a pas pu partir.
pub fn play_wait_sound() -> bool {
    NSSound::soundNamed(&NSString::from_str(WAIT_SOUND)).is_some_and(|sound| sound.play())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_badge_disappears_at_zero() {
        assert_eq!(badge_label(0), None);
        assert_eq!(badge_label(3).as_deref(), Some("3"));
    }

    #[test]
    fn the_badge_log_fires_on_change_only() {
        // Une valeur que ce test est seul à poser : le compteur est global au process.
        assert!(badge_changed(4242));
        assert!(!badge_changed(4242));
        assert!(badge_changed(4243));
        assert!(badge_changed(4242));
    }
}
