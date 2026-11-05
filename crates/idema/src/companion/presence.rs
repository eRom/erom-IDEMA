//! La présence de Romain : l'inactivité du clavier et de la souris, et l'écran verrouillé. Absent, aucune session n'est
//! « vue » (la spec §3.2) : une session laissée au centre peut alors passer à « attend » et notifier le téléphone.
//!
//! Les deux lectures marchent depuis un process ordinaire, sans demande de droit (sonde du 05/11, rejouée dans la VM :
//! l'inactivité y monte sans entrée et retombe à un mouvement synthétique).

use std::ffi::c_void;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_foundation::{NSDictionary, NSNumber, NSString};

/// `kCGEventSourceStateHIDSystemState` : l'état matériel, avant toute application.
const HID_SYSTEM_STATE: i32 = 1;
/// `kCGAnyInputEventType` : `~0`, tous les types d'événement d'entrée.
const ANY_INPUT_EVENT_TYPE: u32 = u32::MAX;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventSourceSecondsSinceLastEventType(state: i32, event_type: u32) -> f64;
    /// Un `CFDictionaryRef` (+1) ou nul. Ponté sans coût vers `NSDictionary`.
    fn CGSessionCopyCurrentDictionary() -> *mut c_void;
}

/// Secondes écoulées depuis le dernier événement de clavier ou de souris. Jamais négatif.
pub fn hid_idle_seconds() -> f64 {
    // SAFETY: appel pur, sans pointeur ni état partagé.
    let seconds =
        unsafe { CGEventSourceSecondsSinceLastEventType(HID_SYSTEM_STATE, ANY_INPUT_EVENT_TYPE) };
    if seconds.is_finite() {
        seconds.max(0.0)
    } else {
        0.0
    }
}

/// L'écran est-il verrouillé ? Faux quand la session ne le dit pas (aucune session graphique, par exemple).
pub fn screen_locked() -> bool {
    // SAFETY: `CGSessionCopyCurrentDictionary` rend une référence retenue (+1) ou nul ; `NSDictionary` en est le pont
    // sans coût, et `Retained::from_raw` en reprend la propriété (libérée à la sortie de la fonction).
    let session: Option<Retained<NSDictionary<NSString, AnyObject>>> =
        unsafe { Retained::from_raw(CGSessionCopyCurrentDictionary().cast()) };
    let Some(session) = session else {
        return false;
    };
    session
        .objectForKey(&NSString::from_str("CGSSessionScreenIsLocked"))
        .and_then(|value| value.downcast_ref::<NSNumber>().map(NSNumber::boolValue))
        .unwrap_or(false)
}

/// Romain est-il absent ? Au-delà du seuil sans entrée, ou écran verrouillé. Un seuil de 0 : toujours absent (le
/// réglage `away_after_seconds = 0` de la config).
pub fn is_away(idle_seconds: f64, locked: bool, threshold_seconds: u64) -> bool {
    threshold_seconds == 0 || locked || idle_seconds >= threshold_seconds as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn away_rules() {
        assert!(!is_away(119.0, false, 120), "just under the threshold");
        assert!(is_away(120.0, false, 120), "at the threshold");
        assert!(is_away(0.0, true, 120), "a locked screen is away at once");
        assert!(is_away(0.0, false, 0), "threshold 0 means always away");
        assert!(!is_away(0.0, false, 1), "fresh input is present");
    }

    #[test]
    fn hid_idle_reads_a_number() {
        let idle = hid_idle_seconds();
        assert!(idle.is_finite() && idle >= 0.0, "{idle}");
        // Ne panique pas, quelle que soit la session (verrouillée ou non, graphique ou non).
        let _ = screen_locked();
    }
}
