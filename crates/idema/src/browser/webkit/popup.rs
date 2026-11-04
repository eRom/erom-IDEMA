//! Les fenêtres de popup : une `window.open` avec une taille (connexion OAuth, paiement) ouvre une vraie fenêtre
//! AppKit autour d'une vue construite avec la configuration reçue, ce qui garde `window.opener`.

use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_app_kit::{NSBackingStoreType, NSWindow, NSWindowStyleMask};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};
use objc2_web_kit::{WKWebView, WKWebViewConfiguration};

use super::classes::PageDelegate;

/// Une popup ouverte : sa vue et la fenêtre qui la porte. Le délégué de la page les garde jusqu'à leur fermeture.
pub(super) struct Popup {
    pub(super) webview: Retained<WKWebView>,
    pub(super) window: Retained<NSWindow>,
}

impl Popup {
    /// Ouvre la fenêtre et rend la popup. La vue naît de `configuration`, telle que WebKit l'a donnée.
    pub(super) fn open(
        mtm: MainThreadMarker,
        configuration: &WKWebViewConfiguration,
        (width, height): (f64, f64),
        host: &str,
        delegate: &PageDelegate,
    ) -> Self {
        let frame = NSRect::new(NSPoint::new(0., 0.), NSSize::new(width, height));
        super::enable_developer_extras(configuration);
        // SAFETY: `configuration` est celle que WebKit vient de donner à `createWebView...` ; le fil est le fil principal
        // (`mtm`), et la vue est neuve.
        let webview = unsafe {
            WKWebView::initWithFrame_configuration(mtm.alloc::<WKWebView>(), frame, configuration)
        };
        // SAFETY: les délégués sont faibles côté WebKit ; le `PageDelegate` vit autant que la page qui garde cette popup.
        unsafe {
            webview.setNavigationDelegate(Some(ProtocolObject::from_ref(delegate)));
            webview.setUIDelegate(Some(ProtocolObject::from_ref(delegate)));
            webview.setInspectable(true);
        }
        // SAFETY: fenêtre neuve, créée sur le fil principal avec des paramètres valides.
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                mtm.alloc::<NSWindow>(),
                frame,
                NSWindowStyleMask::Titled
                    | NSWindowStyleMask::Closable
                    | NSWindowStyleMask::Resizable,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        // SAFETY: sans cela, `close` libérerait la fenêtre une seconde fois quand `Popup` tombe.
        unsafe { window.setReleasedWhenClosed(false) };
        window.setContentView(Some(&webview));
        window.setDelegate(Some(ProtocolObject::from_ref(delegate)));
        window.setTitle(&NSString::from_str(host));
        window.center();
        // Une popup est une fenêtre à part : la fenêtre principale d'IdemA n'est jamais passée au premier plan ici.
        window.makeKeyAndOrderFront(None);
        Self { webview, window }
    }

    /// Le titre de la fenêtre est l'hôte de la page affichée (spec §4.9) ; il suit les redirections et les navigations.
    pub(super) fn retitle(&self, host: &str) {
        self.window.setTitle(&NSString::from_str(host));
    }

    /// Détache les délégués et arrête le chargement ; ferme la fenêtre si elle ne se ferme pas déjà d'elle-même
    /// (`windowWillClose:` l'est : la fermer encore serait une ré-entrance). Après cet appel plus aucun rappel n'arrive.
    pub(super) fn shut_down(self, close_window: bool) {
        // SAFETY: fil principal ; `None` retire simplement les délégués faibles.
        unsafe {
            self.webview.stopLoading();
            self.webview.setNavigationDelegate(None);
            self.webview.setUIDelegate(None);
        }
        self.window.setDelegate(None);
        if close_window {
            self.window.close();
        }
    }
}
