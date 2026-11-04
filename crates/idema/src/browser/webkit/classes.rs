//! Les classes Objective-C du navigateur : la vue de la page, ses délégués (navigation, interface, téléchargement,
//! fenêtres de popup) et l'observateur KVO. Les schémas `define_class!` viennent des exemples d'objc2 0.6 et de
//! wry 0.57 (Apache-2.0). Tout ce qui se passe est rapporté à l'onglet par un `PageEvent` sur un canal : aucune
//! classe ici ne touche à GPUI.

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::path::PathBuf;

use block2::{DynBlock, RcBlock};
use futures::channel::mpsc::UnboundedSender;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool, ProtocolObject};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, Message, define_class, msg_send};
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSEvent, NSModalResponse, NSModalResponseOK, NSOpenPanel,
    NSTextField, NSWindowDelegate,
};
use objc2_foundation::{
    NSArray, NSData, NSDictionary, NSError, NSHTTPURLResponse, NSKeyValueChangeKey,
    NSKeyValueObservingOptions, NSNotification, NSObject, NSObjectNSKeyValueObserverRegistration,
    NSObjectProtocol, NSPoint, NSRect, NSSize, NSString, NSURL, NSURLResponse, NSUUID,
};
use objc2_web_kit::{
    WKDownload, WKDownloadDelegate, WKFrameInfo, WKNavigation, WKNavigationAction,
    WKNavigationDelegate, WKNavigationResponse, WKNavigationResponsePolicy, WKOpenPanelParameters,
    WKUIDelegate, WKWebView, WKWebViewConfiguration, WKWindowFeatures,
};

use super::downloads::{discard_staging, finalize, staging_path, staging_root};
use super::popup::Popup;
use super::{
    AutoAnswer, DialogAction, DialogKind, PageEvent, ResendGuard, ResponseAction, dialog_action,
    downloads_dir, history_key, host_of_url, is_command_only, is_ignored_load_error,
    page_may_open_popup, page_may_open_tab, popup_size, response_action, scheme_of_url,
};
use crate::hud;

// ---------------------------------------------------------------------------------------------------------------------
// La vue de la page.

pub(super) struct IdemaWebViewIvars {
    events: UnboundedSender<PageEvent>,
    /// Reconnaît le renvoi d'un `⌘←` ou `⌘→` que la page a laissé (voir `keyDown:`).
    resend: RefCell<ResendGuard>,
}

define_class!(
    /// Une `WKWebView` qui prévient l'onglet quand un clic tombe dans la page : le panneau y rend alors le focus
    /// GPUI au volet, sans quoi `⌘T`, `⌘W`, `⌘L` partiraient à l'élément GPUI focalisé avant. Elle voit aussi passer
    /// `⌘←` et `⌘→` que la page n'a pas pris : ce sont reculer et avancer.
    #[unsafe(super(WKWebView))]
    #[name = "IdemaWebView"]
    #[ivars = IdemaWebViewIvars]
    pub(super) struct IdemaWebView;

    // SAFETY: `mouseDown:`, `rightMouseDown:` et `keyDown:` sont des méthodes de `NSResponder` à un `NSEvent`, sans retour :
    // les signatures déclarées sont celles des sélecteurs surchargés.
    impl IdemaWebView {
        // `⌘←` et `⌘→` : GPUI prend une touche liée avant la page (`performKeyEquivalent:`), et le champ éditable de la
        // page ne les garderait pas. Les actions du volet les laissent donc passer (`cx.propagate()`) tant que la page a
        // le clavier : elles arrivent à la page d'abord. WebKit les juge dans son processus de contenu ; ceux qu'il
        // laisse, il les renvoie à l'application et `keyDown:` les revoit (même code, même date) : c'est alors reculer ou
        // avancer, et la frappe s'arrête là. Les modificateurs d'une flèche portent `Fonction` et `PavéNumérique` :
        // `is_command_only` ne compare que `⌘`, `⇧`, `⌥` et `⌃`.
        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            let command_only = is_command_only(event.modifierFlags());
            if let Some(key) = history_key(event.keyCode(), command_only)
                && self
                    .ivars()
                    .resend
                    .borrow_mut()
                    .is_resend(event.keyCode(), event.timestamp())
            {
                let _ = self.ivars().events.unbounded_send(PageEvent::HistoryKey(key));
                return;
            }
            // SAFETY: on rappelle l'implémentation de `WKWebView` avec le même événement.
            unsafe { msg_send![super(self), keyDown: event] }
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            let _ = self.ivars().events.unbounded_send(PageEvent::FocusedByClick);
            // SAFETY: on rappelle l'implémentation de `WKWebView` avec le même événement.
            unsafe { msg_send![super(self), mouseDown: event] }
        }

        // Le clic droit ouvre le menu contextuel de la page : le clavier et le focus GPUI doivent suivre aussi.
        #[unsafe(method(rightMouseDown:))]
        fn right_mouse_down(&self, event: &NSEvent) {
            let _ = self.ivars().events.unbounded_send(PageEvent::FocusedByClick);
            // SAFETY: on rappelle l'implémentation de `WKWebView` avec le même événement.
            unsafe { msg_send![super(self), rightMouseDown: event] }
        }
    }
);

impl IdemaWebView {
    pub(super) fn new(
        mtm: MainThreadMarker,
        frame: NSRect,
        configuration: &WKWebViewConfiguration,
        events: UnboundedSender<PageEvent>,
    ) -> Retained<Self> {
        let this = mtm.alloc::<Self>().set_ivars(IdemaWebViewIvars {
            events,
            resend: RefCell::new(ResendGuard::default()),
        });
        // SAFETY: initialiseur désigné de `WKWebView`, appelé une fois sur un objet alloué, sur le fil principal.
        unsafe { msg_send![super(this), initWithFrame: frame, configuration: configuration] }
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// Le délégué : navigation, interface, téléchargements, fenêtres de popup.

pub(super) struct PageDelegateIvars {
    events: UnboundedSender<PageEvent>,
    popups: RefCell<Vec<Popup>>,
    /// Les téléchargements en cours et le fichier choisi pour chacun.
    downloads: RefCell<Vec<(Retained<WKDownload>, PathBuf)>>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "IdemaPageDelegate"]
    #[ivars = PageDelegateIvars]
    pub(super) struct PageDelegate;

    // SAFETY: `NSObject` et ses méthodes de protocole de base ; la classe n'ajoute rien à `NSObjectProtocol`.
    unsafe impl NSObjectProtocol for PageDelegate {}

    // SAFETY: chaque méthode ci-dessous porte la signature de son sélecteur dans `WKNavigationDelegate` (objc2 la
    // vérifie à l'enregistrement en débogage, et le test `the_three_classes_register...` le déclenche) ; la classe
    // est `MainThreadOnly`, WebKit n'appelle un délégué que sur le fil principal.
    unsafe impl WKNavigationDelegate for PageDelegate {
        #[unsafe(method(webView:decidePolicyForNavigationResponse:decisionHandler:))]
        fn decide_policy_for_response(
            &self,
            _web_view: &WKWebView,
            navigation_response: &WKNavigationResponse,
            decision_handler: &DynBlock<dyn Fn(WKNavigationResponsePolicy)>,
        ) {
            // SAFETY: accesseurs en lecture d'une réponse que WebKit vient de donner.
            let (is_main_frame, can_show, response) = unsafe {
                (
                    navigation_response.isForMainFrame(),
                    navigation_response.canShowMIMEType(),
                    navigation_response.response(),
                )
            };
            let policy = match response_action(
                is_main_frame,
                can_show,
                content_disposition(&response).as_deref(),
            ) {
                ResponseAction::Allow => WKNavigationResponsePolicy::Allow,
                ResponseAction::Download => WKNavigationResponsePolicy::Download,
                ResponseAction::Cancel => WKNavigationResponsePolicy::Cancel,
            };
            decision_handler.call((policy,));
        }

        #[unsafe(method(webView:navigationResponse:didBecomeDownload:))]
        fn navigation_response_did_become_download(
            &self,
            _web_view: &WKWebView,
            _navigation_response: &WKNavigationResponse,
            download: &WKDownload,
        ) {
            self.adopt_download(download);
        }

        // Un lien `<a download>` ou un export `blob:` devient un téléchargement par l'action de navigation, pas par la
        // réponse : sans cette méthode, son `WKDownload` n'aurait pas de délégué et ne serait jamais rapporté.
        #[unsafe(method(webView:navigationAction:didBecomeDownload:))]
        fn navigation_action_did_become_download(
            &self,
            _web_view: &WKWebView,
            _navigation_action: &WKNavigationAction,
            download: &WKDownload,
        ) {
            self.adopt_download(download);
        }

        #[unsafe(method(webView:didCommitNavigation:))]
        fn did_commit_navigation(&self, web_view: &WKWebView, _navigation: Option<&WKNavigation>) {
            if self.is_popup(web_view) {
                self.retitle_popup(web_view);
            } else {
                self.send(PageEvent::Committed(current_url(web_view)));
            }
        }

        #[unsafe(method(webView:didFinishNavigation:))]
        fn did_finish_navigation(&self, web_view: &WKWebView, _navigation: Option<&WKNavigation>) {
            if !self.is_popup(web_view) {
                self.send(PageEvent::Finished(current_url(web_view)));
            }
        }

        #[unsafe(method(webView:didFailProvisionalNavigation:withError:))]
        fn did_fail_provisional_navigation(
            &self,
            web_view: &WKWebView,
            _navigation: Option<&WKNavigation>,
            error: &NSError,
        ) {
            self.report_failure(web_view, error);
        }

        #[unsafe(method(webView:didFailNavigation:withError:))]
        fn did_fail_navigation(
            &self,
            web_view: &WKWebView,
            _navigation: Option<&WKNavigation>,
            error: &NSError,
        ) {
            self.report_failure(web_view, error);
        }

        #[unsafe(method(webViewWebContentProcessDidTerminate:))]
        fn web_content_process_did_terminate(&self, web_view: &WKWebView) {
            if !self.is_popup(web_view) {
                self.send(PageEvent::Terminated);
            }
        }
    }

    // SAFETY: signatures des sélecteurs de `WKUIDelegate`. `createWebView...` n'est dans aucune famille de comptage
    // (`new`, `alloc`, `copy`, `init`) : `method_id` (objc2 0.6.5) rend la vue en +0 autoreleased, comme WebKit l'attend
    // d'un tel sélecteur, ou nil. La `Retained` que `Popup` garde est la référence propre d'IdemA.
    unsafe impl WKUIDelegate for PageDelegate {
        // La vue rendue naît de la configuration reçue : c'est ce qui garde `window.opener` (rapport de sonde §6).
        #[unsafe(method_id(webView:createWebViewWithConfiguration:forNavigationAction:windowFeatures:))]
        fn create_web_view(
            &self,
            _web_view: &WKWebView,
            configuration: &WKWebViewConfiguration,
            action: &WKNavigationAction,
            features: &WKWindowFeatures,
        ) -> Option<Retained<WKWebView>> {
            self.open_window(configuration, action, features)
        }

        #[unsafe(method(webViewDidClose:))]
        fn web_view_did_close(&self, web_view: &WKWebView) {
            // Le `window.close()` de la page principale n'est pas une popup : l'onglet reste à l'utilisateur.
            if let Some(popup) = self.take_popup(|popup| std::ptr::eq(&*popup.webview, web_view)) {
                popup.shut_down(true);
                self.send(PageEvent::PopupClosed);
            }
        }

        #[unsafe(method(webView:runJavaScriptAlertPanelWithMessage:initiatedByFrame:completionHandler:))]
        fn run_alert_panel(
            &self,
            web_view: &WKWebView,
            message: &NSString,
            _frame: &WKFrameInfo,
            completion_handler: &DynBlock<dyn Fn()>,
        ) {
            if auto_answer(web_view, DialogKind::Alert).is_some() {
                completion_handler.call(());
                return;
            }
            let handler = completion_handler.copy();
            present_alert(web_view, message, false, move |_| handler.call(()));
        }

        #[unsafe(method(webView:runJavaScriptConfirmPanelWithMessage:initiatedByFrame:completionHandler:))]
        fn run_confirm_panel(
            &self,
            web_view: &WKWebView,
            message: &NSString,
            _frame: &WKFrameInfo,
            completion_handler: &DynBlock<dyn Fn(Bool)>,
        ) {
            if auto_answer(web_view, DialogKind::Confirm).is_some() {
                completion_handler.call((Bool::NO,));
                return;
            }
            let handler = completion_handler.copy();
            present_alert(web_view, message, true, move |accepted| {
                handler.call((Bool::new(accepted),));
            });
        }

        // `prompt()` : une feuille avec un champ de saisie, le texte par défaut déjà dedans. Annuler rend nil, que la
        // page lit comme `null`.
        #[unsafe(method(webView:runJavaScriptTextInputPanelWithPrompt:defaultText:initiatedByFrame:completionHandler:))]
        fn run_text_input_panel(
            &self,
            web_view: &WKWebView,
            prompt: &NSString,
            default_text: Option<&NSString>,
            _frame: &WKFrameInfo,
            completion_handler: &DynBlock<dyn Fn(*mut NSString)>,
        ) {
            if auto_answer(web_view, DialogKind::Prompt).is_some() {
                completion_handler.call((std::ptr::null_mut(),));
                return;
            }
            let handler = completion_handler.copy();
            present_prompt(web_view, prompt, default_text, move |text| {
                let text = text.map(|text| NSString::from_str(&text));
                let pointer = text.as_ref().map_or(std::ptr::null_mut(), |text| {
                    Retained::as_ptr(text).cast_mut()
                });
                handler.call((pointer,));
            });
        }

        // `<input type=file>` : le sélecteur de fichiers du système, en feuille sur la fenêtre d'IdemA. Annuler rend nil.
        #[unsafe(method(webView:runOpenPanelWithParameters:initiatedByFrame:completionHandler:))]
        fn run_open_panel(
            &self,
            web_view: &WKWebView,
            parameters: &WKOpenPanelParameters,
            _frame: &WKFrameInfo,
            completion_handler: &DynBlock<dyn Fn(*mut NSArray<NSURL>)>,
        ) {
            if auto_answer(web_view, DialogKind::FilePicker).is_some() {
                completion_handler.call((std::ptr::null_mut(),));
                return;
            }
            let handler = completion_handler.copy();
            // SAFETY: simples accesseurs de `WKOpenPanelParameters`.
            let (multiple, directories) = unsafe {
                (
                    parameters.allowsMultipleSelection(),
                    parameters.allowsDirectories(),
                )
            };
            present_open_panel(web_view, multiple, directories, move |urls| {
                let pointer = urls.as_ref().map_or(std::ptr::null_mut(), |urls| {
                    Retained::as_ptr(urls).cast_mut()
                });
                handler.call((pointer,));
            });
        }
    }

    // SAFETY: signatures des sélecteurs de `WKDownloadDelegate` ; le bloc de destination reçoit une URL de fichier ou
    // nil (annulation).
    unsafe impl WKDownloadDelegate for PageDelegate {
        #[unsafe(method(download:decideDestinationUsingResponse:suggestedFilename:completionHandler:))]
        fn decide_destination(
            &self,
            download: &WKDownload,
            _response: &NSURLResponse,
            suggested_filename: &NSString,
            completion_handler: &DynBlock<dyn Fn(*mut NSURL)>,
        ) {
            // WebKit ne reçoit jamais un chemin que TCC protège : le fichier arrive dans un dossier de cache à lui, et
            // rejoint `~/Downloads` sur un fil à part une fois complet (`download_did_finish`).
            let uuid = NSUUID::new().UUIDString().to_string();
            let path = staging_root()
                .map(|root| staging_path(&root, &uuid, &suggested_filename.to_string()))
                .filter(|path| std::fs::create_dir_all(path.parent().unwrap_or(path)).is_ok());
            let Some(path) = path else {
                // Une URL nulle annule le téléchargement.
                completion_handler.call((std::ptr::null_mut(),));
                self.send(PageEvent::DownloadFailed(
                    "dossier de dépôt introuvable".to_owned(),
                ));
                return;
            };
            let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
            self.ivars()
                .downloads
                .borrow_mut()
                .push((download.retain(), path));
            completion_handler.call((Retained::as_ptr(&url).cast_mut(),));
        }

        #[unsafe(method(downloadDidFinish:))]
        fn download_did_finish(&self, download: &WKDownload) {
            let Some(staging) = self.take_download(download) else {
                return;
            };
            // Le fichier est complet : il rejoint `~/Downloads` hors du fil principal, qui n'attend jamais TCC.
            let events = self.ivars().events.clone();
            std::thread::spawn(move || {
                let event = match finalize(&staging, downloads_dir().as_deref()) {
                    Ok(path) => PageEvent::DownloadFinished(path),
                    Err(cause) => PageEvent::DownloadFailed(cause),
                };
                let _ = events.unbounded_send(event);
            });
        }

        #[unsafe(method(download:didFailWithError:resumeData:))]
        fn download_did_fail(
            &self,
            download: &WKDownload,
            error: &NSError,
            _resume_data: Option<&NSData>,
        ) {
            let staging = self.take_download(download);
            if let Some(staging) = &staging {
                discard_staging(staging);
            }
            let known = staging.is_some();
            // Une destination refusée par `decide_destination` a déjà été rapportée : son annulation ne l'est pas deux fois.
            if known || !is_ignored_load_error(&error.domain().to_string(), error.code()) {
                self.send(PageEvent::DownloadFailed(
                    error.localizedDescription().to_string(),
                ));
            }
        }
    }

    // SAFETY: signature de `windowWillClose:` dans `NSWindowDelegate` ; la notification porte la fenêtre qui se ferme.
    unsafe impl NSWindowDelegate for PageDelegate {
        // La croix de la fenêtre d'une popup : la même sortie que `webViewDidClose:`, sans refermer la fenêtre.
        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, notification: &NSNotification) {
            let Some(object) = notification.object() else {
                return;
            };
            let closing = Retained::as_ptr(&object).cast::<c_void>();
            let popup = self.take_popup(|popup| {
                std::ptr::eq(Retained::as_ptr(&popup.window).cast::<c_void>(), closing)
            });
            if let Some(popup) = popup {
                popup.shut_down(false);
                self.send(PageEvent::PopupClosed);
            }
        }
    }
);

impl PageDelegate {
    pub(super) fn new(mtm: MainThreadMarker, events: UnboundedSender<PageEvent>) -> Retained<Self> {
        let this = mtm.alloc::<Self>().set_ivars(PageDelegateIvars {
            events,
            popups: RefCell::new(Vec::new()),
            downloads: RefCell::new(Vec::new()),
        });
        // SAFETY: `init` de `NSObject` sur un objet alloué.
        unsafe { msg_send![super(this), init] }
    }

    /// Une demande d'ouverture de fenêtre de la page : une popup si elle donne une taille (`width` et `height`),
    /// sinon un onglet (`target=_blank`, `window.open` simple), et WebKit ne reçoit alors aucune vue. Une adresse
    /// d'un autre schéma que le web est refusée dans les deux cas (la popup vide ou `about:blank` passe).
    fn open_window(
        &self,
        configuration: &WKWebViewConfiguration,
        action: &WKNavigationAction,
        features: &WKWindowFeatures,
    ) -> Option<Retained<WKWebView>> {
        // SAFETY: accesseurs en lecture de l'action et des caractéristiques que WebKit vient de donner.
        let (width, height, url) = unsafe {
            (
                features.width().map(|number| number.doubleValue()),
                features.height().map(|number| number.doubleValue()),
                action.request().URL().and_then(|url| url.absoluteString()),
            )
        };
        let url = url.map(|url| url.to_string()).unwrap_or_default();
        let (Some(width), Some(height)) = (width, height) else {
            if page_may_open_tab(&url) {
                self.send(PageEvent::OpenTab(url));
            } else if !url.is_empty() {
                Self::log_refused_open(&url);
            }
            return None;
        };
        // Une popup d'un autre schéma que le web (un `file:` surtout) n'ouvre rien : WebKit ne reçoit aucune vue.
        if !page_may_open_popup(&url) {
            Self::log_refused_open(&url);
            return None;
        }
        let host = host_of_url(&url).unwrap_or_default();
        let popup = Popup::open(
            self.mtm(),
            configuration,
            popup_size(width, height),
            &host,
            self,
        );
        let webview = popup.webview.clone();
        self.ivars().popups.borrow_mut().push(popup);
        self.send(PageEvent::PopupOpened(host));
        Some(webview)
    }

    /// Le journal d'une ouverture refusée ne garde que le schéma : ni chemin, ni requête, ni fragment.
    fn log_refused_open(url: &str) {
        hud::log_event(&format!(
            "browser page open refused scheme={}",
            scheme_of_url(url).as_deref().unwrap_or("-")
        ));
    }

    /// Ferme explicitement les popups encore ouvertes : leur délégué (celui-ci) meurt avec la page, et une fenêtre
    /// restée ouverte parlerait à un délégué disparu. Rien n'est rapporté : la page, elle, n'écoute plus.
    pub(super) fn close_popups(&self) {
        let popups = std::mem::take(&mut *self.ivars().popups.borrow_mut());
        for popup in popups {
            popup.shut_down(true);
        }
    }

    fn send(&self, event: PageEvent) {
        let _ = self.ivars().events.unbounded_send(event);
    }

    /// Cette vue est-elle une popup ? Ses navigations ne sont pas celles de l'onglet.
    fn is_popup(&self, web_view: &WKWebView) -> bool {
        self.ivars()
            .popups
            .borrow()
            .iter()
            .any(|popup| std::ptr::eq(&*popup.webview, web_view))
    }

    /// Sort de la liste la première popup qui répond au test. L'emprunt est rendu avant tout appel à AppKit.
    fn take_popup(&self, matches: impl Fn(&Popup) -> bool) -> Option<Popup> {
        let mut popups = self.ivars().popups.borrow_mut();
        let index = popups.iter().position(matches)?;
        Some(popups.remove(index))
    }

    /// Branche un `WKDownload` neuf sur ce délégué, qu'il naisse d'une réponse ou d'une action de navigation.
    fn adopt_download(&self, download: &WKDownload) {
        // SAFETY: le délégué de téléchargement est faible ; le `PageDelegate` vit autant que la page.
        unsafe { download.setDelegate(Some(ProtocolObject::from_ref(self))) };
    }

    /// Annule les téléchargements en cours et retire leurs fichiers partiels : le dépôt (dossier de cache propre à chaque
    /// téléchargement) part quand WebKit a fini d'annuler. `~/Downloads` n'est pas touché : rien n'y est encore posé.
    pub(super) fn cancel_downloads(&self) {
        let downloads = std::mem::take(&mut *self.ivars().downloads.borrow_mut());
        for (download, path) in downloads {
            let remove_partial = RcBlock::new(move |_resume_data: *mut NSData| {
                discard_staging(&path);
            });
            // SAFETY: fil principal ; sans délégué plus aucun rappel ne revient ici, le bloc est copié par WebKit.
            unsafe {
                download.setDelegate(None);
                download.cancel(Some(&remove_partial));
            }
        }
    }

    /// Le titre d'une popup est l'hôte de la page qu'elle affiche. Sans hôte (`about:blank`, `file:`), il reste tel quel.
    fn retitle_popup(&self, web_view: &WKWebView) {
        let Some(host) = host_of_url(&current_url(web_view)) else {
            return;
        };
        let popups = self.ivars().popups.borrow();
        if let Some(popup) = popups
            .iter()
            .find(|popup| std::ptr::eq(&*popup.webview, web_view))
        {
            popup.retitle(&host);
        }
    }

    fn take_download(&self, download: &WKDownload) -> Option<PathBuf> {
        let mut downloads = self.ivars().downloads.borrow_mut();
        let index = downloads
            .iter()
            .position(|(known, _)| std::ptr::eq(&**known, download))?;
        Some(downloads.remove(index).1)
    }

    fn report_failure(&self, web_view: &WKWebView, error: &NSError) {
        if self.is_popup(web_view) {
            return;
        }
        let code = error.code();
        if is_ignored_load_error(&error.domain().to_string(), code) {
            return;
        }
        let url = failing_url(error);
        self.send(PageEvent::Failed {
            code,
            host: url.as_deref().and_then(host_of_url),
            url,
            description: error.localizedDescription().to_string(),
        });
    }
}

/// L'adresse courante de la vue, vide quand WebKit n'en a pas encore.
fn current_url(web_view: &WKWebView) -> String {
    // SAFETY: accesseur en lecture, fil principal.
    unsafe { web_view.URL() }
        .and_then(|url| url.absoluteString())
        .map(|url| url.to_string())
        .unwrap_or_default()
}

/// L'adresse qui a échoué, lue dans la clé standard de l'erreur.
fn failing_url(error: &NSError) -> Option<String> {
    let user_info = error.userInfo();
    // WebKit pose `NSErrorFailingURLKey` (un `NSURL`, mesuré) ; la clé en texte est celle de CFNetwork, en repli.
    let from_url = user_info
        .objectForKey(&NSString::from_str("NSErrorFailingURLKey"))
        .and_then(|value| value.downcast::<NSURL>().ok())
        .and_then(|url| url.absoluteString())
        .map(|url| url.to_string());
    from_url.or_else(|| {
        let value = user_info.objectForKey(&NSString::from_str("NSErrorFailingURLStringKey"))?;
        let url = value.downcast::<NSString>().ok()?;
        Some(url.to_string())
    })
}

/// L'en-tête `Content-Disposition` d'une réponse HTTP ; `None` pour une réponse qui ne l'est pas (`file:`, `data:`).
fn content_disposition(response: &NSURLResponse) -> Option<String> {
    response
        .downcast_ref::<NSHTTPURLResponse>()?
        .valueForHTTPHeaderField(&NSString::from_str("Content-Disposition"))
        .map(|value| value.to_string())
}

/// La page est-elle à l'écran ? Dans une fenêtre, et ni elle ni un de ses ancêtres cachés : le volet fermé ou un autre
/// onglet actif cachent la vue native (`WebPage::set_visible`).
fn is_on_screen(web_view: &WKWebView) -> bool {
    web_view.window().is_some() && !web_view.isHiddenOrHasHiddenAncestor()
}

/// Faut-il répondre à la page sans feuille ? `Some` : oui, et la ligne du journal est écrite (l'hôte seulement, jamais
/// le chemin ni la requête). L'appelant livre alors la réponse que le type de boîte demande (`AutoAnswer`).
fn auto_answer(web_view: &WKWebView, kind: DialogKind) -> Option<AutoAnswer> {
    let DialogAction::Answer(answer) = dialog_action(kind, is_on_screen(web_view)) else {
        return None;
    };
    let host = host_of_url(&current_url(web_view)).unwrap_or_else(|| "-".to_owned());
    hud::log_event(&format!(
        "browser dialog auto-answered kind={} host={host}",
        kind.label()
    ));
    Some(answer)
}

/// La réponse d'une feuille à WebKit, qui exige un appel de son `completionHandler` et un seul. `answer` la livre ; si le
/// bloc de la feuille est lâché sans réponse (page ou fenêtre détruite pendant que la feuille est ouverte), le `Drop`
/// livre l'annulation (`None`) : WebKit lève une exception quand un `completionHandler` meurt sans avoir été appelé.
struct SheetAnswer<T: 'static> {
    done: Cell<Option<SheetDone<T>>>,
}

/// Ce que la page attend d'une feuille : la réponse, ou `None` pour l'annulation.
type SheetDone<T> = Box<dyn FnOnce(Option<T>)>;

impl<T: 'static> SheetAnswer<T> {
    fn new(done: impl FnOnce(Option<T>) + 'static) -> Self {
        Self {
            done: Cell::new(Some(Box::new(done))),
        }
    }

    /// Livre `answer` (`None` : annulé) ; sans effet si la réponse est déjà partie.
    fn answer(&self, answer: Option<T>) {
        if let Some(done) = self.done.take() {
            done(answer);
        }
    }
}

impl<T: 'static> Drop for SheetAnswer<T> {
    fn drop(&mut self) {
        self.answer(None);
    }
}

/// Une alerte JavaScript en feuille sur la fenêtre de la vue. `done` reçoit `true` pour le premier bouton (OK).
/// Sans fenêtre ni fil principal, la page est tout de suite délivrée : `done(false)`.
fn present_alert(
    web_view: &WKWebView,
    message: &NSString,
    with_cancel: bool,
    done: impl FnOnce(bool) + 'static,
) {
    let (Some(mtm), Some(window)) = (MainThreadMarker::new(), web_view.window()) else {
        done(false);
        return;
    };
    let alert = NSAlert::new(mtm);
    alert.setMessageText(message);
    alert.addButtonWithTitle(&NSString::from_str("OK"));
    if with_cancel {
        add_cancel_button(&alert);
    }
    let answer = SheetAnswer::new(move |answer: Option<bool>| done(answer.unwrap_or(false)));
    let block = RcBlock::new(move |response: NSModalResponse| {
        answer.answer(Some(response == NSAlertFirstButtonReturn));
    });
    alert.beginSheetModalForWindow_completionHandler(&window, Some(&block));
}

/// Le `prompt()` de la page : une feuille avec un champ de saisie. `done` reçoit le texte saisi pour OK, `None` pour
/// Annuler (la page lit `null`) et quand il n'y a ni fenêtre ni fil principal.
fn present_prompt(
    web_view: &WKWebView,
    message: &NSString,
    default_text: Option<&NSString>,
    done: impl FnOnce(Option<String>) + 'static,
) {
    let (Some(mtm), Some(window)) = (MainThreadMarker::new(), web_view.window()) else {
        done(None);
        return;
    };
    let alert = NSAlert::new(mtm);
    alert.setMessageText(message);
    alert.addButtonWithTitle(&NSString::from_str("OK"));
    add_cancel_button(&alert);
    let field = NSTextField::initWithFrame(
        mtm.alloc::<NSTextField>(),
        NSRect::new(NSPoint::new(0., 0.), NSSize::new(300., 24.)),
    );
    if let Some(text) = default_text {
        field.setStringValue(text);
    }
    alert.setAccessoryView(Some(&field));
    // Le champ prend le clavier dès que la feuille s'affiche : la saisie directe, et ↩ pour OK.
    alert.layout();
    alert.window().setInitialFirstResponder(Some(&field));
    let answer = SheetAnswer::new(done);
    let block = RcBlock::new(move |response: NSModalResponse| {
        let text = (response == NSAlertFirstButtonReturn).then(|| field.stringValue().to_string());
        answer.answer(text);
    });
    alert.beginSheetModalForWindow_completionHandler(&window, Some(&block));
}

/// Le bouton « Annuler » d'une feuille, qui répond à Échap : AppKit ne donne Échap qu'à un bouton titré « Cancel » dans la
/// langue du système, jamais à un libellé français.
fn add_cancel_button(alert: &NSAlert) {
    let cancel = alert.addButtonWithTitle(&NSString::from_str("Annuler"));
    cancel.setKeyEquivalent(&NSString::from_str("\u{1b}"));
}

/// Le sélecteur de fichiers de `<input type=file>`, en feuille sur la fenêtre de la vue. `done` reçoit les fichiers
/// choisis pour OK, `None` pour Annuler et quand il n'y a ni fenêtre ni fil principal. Jamais d'activation de
/// l'application : la feuille est portée par la fenêtre d'IdemA, déjà au premier plan.
fn present_open_panel(
    web_view: &WKWebView,
    multiple: bool,
    directories: bool,
    done: impl FnOnce(Option<Retained<NSArray<NSURL>>>) + 'static,
) {
    let (Some(mtm), Some(window)) = (MainThreadMarker::new(), web_view.window()) else {
        done(None);
        return;
    };
    let panel = NSOpenPanel::openPanel(mtm);
    panel.setCanChooseFiles(true);
    panel.setCanChooseDirectories(directories);
    panel.setAllowsMultipleSelection(multiple);
    let answer = SheetAnswer::new(done);
    let chosen = panel.clone();
    let block = RcBlock::new(move |response: NSModalResponse| {
        answer.answer((response == NSModalResponseOK).then(|| chosen.URLs()));
    });
    panel.beginSheetModalForWindow_completionHandler(&window, &block);
}

// ---------------------------------------------------------------------------------------------------------------------
// L'observateur KVO : titre, adresse, chargement, progression, historique.

const OBSERVED_KEYS: [&str; 6] = [
    "title",
    "URL",
    "loading",
    "estimatedProgress",
    "canGoBack",
    "canGoForward",
];

pub(super) struct PageObserverIvars {
    web_view: Retained<WKWebView>,
    events: UnboundedSender<PageEvent>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "IdemaPageObserver"]
    #[ivars = PageObserverIvars]
    pub(super) struct PageObserver;

    // SAFETY: `NSObject` et ses méthodes de protocole de base ; la classe n'ajoute rien à `NSObjectProtocol`.
    unsafe impl NSObjectProtocol for PageObserver {}

    // SAFETY: `observeValueForKeyPath:ofObject:change:context:` est la méthode de `NSObject(NSKeyValueObserving)` ;
    // la signature déclarée est la sienne, et KVO l'appelle sur le fil de la vue observée (le principal).
    impl PageObserver {
        #[unsafe(method(observeValueForKeyPath:ofObject:change:context:))]
        fn observe_value(
            &self,
            key_path: Option<&NSString>,
            _object: Option<&AnyObject>,
            _change: Option<&NSDictionary<NSKeyValueChangeKey, AnyObject>>,
            _context: *mut c_void,
        ) {
            let Some(key_path) = key_path else {
                return;
            };
            let web_view = &self.ivars().web_view;
            // SAFETY: accesseurs en lecture de la vue que cet observateur garde en vie ; KVO tourne sur le fil principal.
            let event = unsafe {
                match key_path.to_string().as_str() {
                    "title" => PageEvent::Title(web_view.title().map(|title| title.to_string())),
                    "URL" => PageEvent::Url(
                        web_view
                            .URL()
                            .and_then(|url| url.absoluteString())
                            .map(|url| url.to_string()),
                    ),
                    "loading" | "estimatedProgress" => PageEvent::Loading {
                        loading: web_view.isLoading(),
                        progress: web_view.estimatedProgress(),
                    },
                    "canGoBack" | "canGoForward" => PageEvent::History {
                        back: web_view.canGoBack(),
                        forward: web_view.canGoForward(),
                    },
                    _ => return,
                }
            };
            let _ = self.ivars().events.unbounded_send(event);
        }
    }
);

impl PageObserver {
    pub(super) fn new(
        mtm: MainThreadMarker,
        web_view: &Retained<WKWebView>,
        events: UnboundedSender<PageEvent>,
    ) -> Retained<Self> {
        let this = mtm.alloc::<Self>().set_ivars(PageObserverIvars {
            web_view: web_view.clone(),
            events,
        });
        // SAFETY: `init` de `NSObject` sur un objet alloué.
        let observer: Retained<Self> = unsafe { msg_send![super(this), init] };
        for key in OBSERVED_KEYS {
            // SAFETY: l'observateur répond à `observeValueForKeyPath:...` ; chaque inscription est défaite au `Drop`.
            unsafe {
                web_view.addObserver_forKeyPath_options_context(
                    &observer,
                    &NSString::from_str(key),
                    NSKeyValueObservingOptions::New,
                    std::ptr::null_mut(),
                );
            }
        }
        observer
    }
}

impl Drop for PageObserver {
    fn drop(&mut self) {
        for key in OBSERVED_KEYS {
            // SAFETY: chacune de ces clés a été inscrite dans `new` et ne l'a pas été retirée depuis.
            unsafe {
                self.ivars()
                    .web_view
                    .removeObserver_forKeyPath(self, &NSString::from_str(key));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2::{ClassType, sel};

    // `define_class!` n'enregistre une classe qu'à son premier usage, et panique si la déclaration est invalide :
    // ce test la force, avant tout écran. Les rappels eux-mêmes ne se jouent qu'avec une fenêtre (essai hors GPUI du
    // rapport de la tâche 5).
    #[test]
    fn the_three_classes_register_with_the_selectors_webkit_calls() {
        assert!(IdemaWebView::class().superclass() == Some(WKWebView::class()));
        for selector in [sel!(mouseDown:), sel!(rightMouseDown:)] {
            assert!(IdemaWebView::class().instance_method(selector).is_some());
        }
        assert!(PageObserver::class().superclass() == Some(NSObject::class()));
        let delegate = PageDelegate::class();
        for selector in [
            sel!(webView:decidePolicyForNavigationResponse:decisionHandler:),
            sel!(webView:navigationAction:didBecomeDownload:),
            sel!(webView:navigationResponse:didBecomeDownload:),
            sel!(webView:createWebViewWithConfiguration:forNavigationAction:windowFeatures:),
            sel!(download:decideDestinationUsingResponse:suggestedFilename:completionHandler:),
            sel!(windowWillClose:),
        ] {
            assert!(delegate.instance_method(selector).is_some());
        }
    }
}
