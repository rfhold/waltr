//! Main-thread-owned macOS adapter around Wry's URL-only navigation delegate.
#![allow(
    unsafe_code,
    reason = "Application-local WKNavigationDelegate FFI for exact macOS frame confinement"
)]

use std::{cell::RefCell, collections::HashMap};

use block2::DynBlock;
use objc2::{
    DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send,
    rc::Retained,
    runtime::{AnyObject, ProtocolObject, Sel},
    sel,
};
use objc2_foundation::{NSObject, NSObjectProtocol};
use objc2_web_kit::{
    WKNavigationAction, WKNavigationActionPolicy, WKNavigationDelegate, WKNavigationResponse,
    WKNavigationResponsePolicy, WKWebView,
};
use tauri::Manager;
use url::Url;

use crate::native_apps::Bootstrap;

#[derive(Clone, Debug)]
struct FramePolicy {
    container: Url,
    #[cfg(feature = "native-apps-test")]
    probe: Url,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Decision {
    Cancel,
    Forward,
    Allow,
}

impl FramePolicy {
    fn new(bootstrap: &Bootstrap) -> Result<Self, String> {
        fn endpoint(value: &str, path: &str) -> Result<Url, String> {
            let url = strict_url(value).ok_or("invalid native asset address")?;
            if url.scheme() != "http"
                || url.host_str() != Some("127.0.0.1")
                || url.port().is_none()
                || url.path() != path
                || url.query().is_some()
                || url.fragment().is_some()
            {
                return Err("invalid native asset address".to_owned());
            }
            Ok(url)
        }
        Ok(Self {
            container: endpoint(&bootstrap.container_url, "/container.html")?,
            #[cfg(feature = "native-apps-test")]
            probe: endpoint(&bootstrap.probe_url, "/probe/container.html")?,
        })
    }

    fn asset(&self, url: &Url) -> bool {
        let container = url.origin() == self.container.origin()
            && ((url.path() == "/container.html" && container_fragment(url.fragment()))
                || (valid_view_path(url.path()) && url.fragment().is_none()));
        #[cfg(feature = "native-apps-test")]
        let probe = url.origin() == self.probe.origin()
            && url.path() == "/probe/container.html"
            && url.fragment().is_none();
        #[cfg(not(feature = "native-apps-test"))]
        let probe = false;
        url.query().is_none() && (container || probe)
    }

    fn decide(
        &self,
        value: &str,
        main: Option<bool>,
        download: bool,
        source: Option<&str>,
    ) -> Decision {
        if download {
            return Decision::Cancel;
        }
        let Some(main) = main else {
            return Decision::Cancel;
        };
        let Some(url) = strict_url(value) else {
            return Decision::Cancel;
        };
        if main {
            return if packaged(&url) {
                Decision::Forward
            } else {
                Decision::Cancel
            };
        }
        if self.asset(&url) {
            return Decision::Allow;
        }
        // WebKit creates empty frames and srcdoc frames without an HTTP resource. Only a
        // trusted top frame or this generation's exact outer document may create these.
        if matches!(value, "about:blank" | "about:srcdoc")
            && source
                .and_then(strict_url)
                .is_some_and(|source| packaged(&source) || self.asset(&source))
        {
            return Decision::Allow;
        }
        Decision::Cancel
    }

    fn response(&self, value: &str, main: bool, download: bool) -> Decision {
        // Empty/srcdoc documents have no network response or source frame identity.
        // Their action must already pass the source check; they cannot redirect to HTTP.
        if !main && !download && matches!(value, "about:blank" | "about:srcdoc") {
            Decision::Allow
        } else {
            self.decide(value, Some(main), download, None)
        }
    }
}

fn valid_view_path(path: &str) -> bool {
    path.strip_prefix("/views/").is_some_and(|id| {
        !id.is_empty()
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    })
}

fn strict_url(value: &str) -> Option<Url> {
    if value.contains('\\') || value.chars().any(char::is_whitespace) {
        return None;
    }
    let url = Url::parse(value).ok()?;
    (url.username().is_empty() && url.password().is_none()).then_some(url)
}

fn packaged(url: &Url) -> bool {
    url.scheme() == "tauri"
        && url.host_str() == Some("localhost")
        && url.port().is_none()
        && url.fragment().is_none()
}

fn container_fragment(fragment: Option<&str>) -> bool {
    let Some(fragment) = fragment else {
        return true;
    };
    let fields: Vec<_> = url::form_urlencoded::parse(fragment.as_bytes()).collect();
    fields.len() == 2
        && fields[0].0 == "hostOrigin"
        && fields[0].1 == "tauri://localhost"
        && fields[1].0 == "generation"
        && fields[1].1.len() == 36
        && fields[1].1.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

struct DelegateIvars {
    original: Retained<ProtocolObject<dyn WKNavigationDelegate>>,
    policy: RefCell<FramePolicy>,
}

define_class!(
    // SAFETY: NSObject has no subclassing obligations. All ivars and callbacks stay on
    // the main thread, as required by WKNavigationDelegate. No associated-object cycle.
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[ivars = DelegateIvars]
    struct FrameDelegate;

    unsafe impl NSObjectProtocol for FrameDelegate {
        #[unsafe(method(respondsToSelector:))]
        fn responds(&self, selector: Sel) -> bool {
            // The pinned Wry delegate uses the legacy action selector. Refuse to advertise
            // the preference variant, which otherwise supersedes our frame-aware gate.
            if selector == sel!(webView:decidePolicyForNavigationAction:preferences:decisionHandler:) { false } else {
                (unsafe { msg_send![super(self), respondsToSelector: selector] }) || self.ivars().original.respondsToSelector(selector)
            }
        }
    }

    impl FrameDelegate {
        // SAFETY: Objective-C fast forwarding returns an unowned object kept alive by ivars.
        // Unknown optional methods retain their original ABI and are not reimplemented here.
        #[unsafe(method(forwardingTargetForSelector:))]
        fn forwarding_target(&self, selector: Sel) -> Option<&AnyObject> {
            self.ivars().original.respondsToSelector(selector).then(|| <ProtocolObject<dyn WKNavigationDelegate> as AsRef<AnyObject>>::as_ref(&self.ivars().original))
        }
    }

    unsafe impl WKNavigationDelegate for FrameDelegate {
        #[unsafe(method(webView:decidePolicyForNavigationAction:decisionHandler:))]
        unsafe fn action(&self, view: &WKWebView, action: &WKNavigationAction, handler: &DynBlock<dyn Fn(WKNavigationActionPolicy)>) {
            // SAFETY: WebKit supplies these objects and a live decision block on the main
            // thread. Each branch either calls the block once or transfers it to Wry once.
            unsafe {
                let target = action.targetFrame().map(|frame| frame.isMainFrame());
                let value = action.request().URL().and_then(|url| url.absoluteString()).map(|value| value.to_string());
                let source = action.sourceFrame().and_then(|frame| frame.request().URL()).and_then(|url| url.absoluteString()).map(|value| value.to_string());
                let download = action.respondsToSelector(sel!(shouldPerformDownload)) && action.shouldPerformDownload();
                let decision = value.as_deref().map_or(Decision::Cancel, |value| self.ivars().policy.borrow().decide(value, target, download, source.as_deref()));
                match decision {
                    Decision::Cancel => {
                         #[cfg(feature = "native-apps-test")]
                         eprintln!("WALTR_NATIVE_PROBE navigation-denied.{}", match target { None => "popup", Some(true) => "main", Some(false) => "subframe" });
                        handler.call((WKNavigationActionPolicy::Cancel,));
                    },
                    Decision::Allow => handler.call((WKNavigationActionPolicy::Allow,)),
                    Decision::Forward => self.ivars().original.webView_decidePolicyForNavigationAction_decisionHandler(view, action, handler),
                }
            }
        }

        #[unsafe(method(webView:decidePolicyForNavigationResponse:decisionHandler:))]
        unsafe fn response(&self, view: &WKWebView, response: &WKNavigationResponse, handler: &DynBlock<dyn Fn(WKNavigationResponsePolicy)>) {
            // SAFETY: As above. Responses recheck the resolved URL (including redirects).
            unsafe {
                let value = response.response().URL().and_then(|url| url.absoluteString()).map(|value| value.to_string());
                let decision = value.as_deref().map_or(Decision::Cancel, |value| self.ivars().policy.borrow().response(value, response.isForMainFrame(), !response.canShowMIMEType()));
                match decision {
                    Decision::Cancel => {
                        #[cfg(feature = "native-apps-test")]
                        eprintln!("WALTR_NATIVE_PROBE response-denied");
                        handler.call((WKNavigationResponsePolicy::Cancel,));
                    },
                    Decision::Allow => handler.call((WKNavigationResponsePolicy::Allow,)),
                    Decision::Forward => self.ivars().original.webView_decidePolicyForNavigationResponse_decisionHandler(view, response, handler),
                }
            }
        }
    }
);

struct Owner {
    view: Retained<WKWebView>,
    delegate: Retained<FrameDelegate>,
}

thread_local! {
    // WKWebView.navigationDelegate is weak. Wry's original delegate may itself retain
    // the view, so never associate this owner with the view or the original delegate.
    static OWNERS: RefCell<HashMap<String, Owner>> = RefCell::new(HashMap::new());
}

fn attach(
    label: &str,
    view: &WKWebView,
    policy: FramePolicy,
    mtm: MainThreadMarker,
) -> Result<(), String> {
    OWNERS.with(|owners| {
        let mut owners = owners.borrow_mut();
        if let Some(owner) = owners.get(label) {
            if !std::ptr::eq(owner.view.as_ref(), view) {
                return Err("native navigation view changed".to_owned());
            }
            // SAFETY: Main-thread access to this live view's weak delegate slot.
            if !unsafe { view.navigationDelegate() }.is_some_and(|current| {
                Retained::as_ptr(&current).cast::<()>()
                    == Retained::as_ptr(&owner.delegate).cast::<()>()
            }) {
                return Err("native navigation delegate was displaced".to_owned());
            }
            *owner.delegate.ivars().policy.borrow_mut() = policy;
            return Ok(());
        }
        // SAFETY: with_webview provides the live WKWebView on its owning main thread.
        unsafe {
            let original = view
                .navigationDelegate()
                .ok_or("missing Wry navigation delegate")?;
            if original.respondsToSelector(
                sel!(webView:decidePolicyForNavigationAction:preferences:decisionHandler:),
            ) || !original
                .respondsToSelector(sel!(webView:decidePolicyForNavigationAction:decisionHandler:))
                || !original.respondsToSelector(
                    sel!(webView:decidePolicyForNavigationResponse:decisionHandler:),
                )
            {
                return Err("unsupported Wry navigation delegate".to_owned());
            }
            let allocated = FrameDelegate::alloc(mtm).set_ivars(DelegateIvars {
                original,
                policy: RefCell::new(policy),
            });
            let delegate: Retained<FrameDelegate> = msg_send![super(allocated), init];
            let retained_view = Retained::retain(std::ptr::from_ref(view).cast_mut())
                .ok_or("native view disappeared")?;
            view.setNavigationDelegate(Some(ProtocolObject::from_ref::<FrameDelegate>(&delegate)));
            if !view.navigationDelegate().is_some_and(|installed| {
                Retained::as_ptr(&installed).cast::<()>()
                    == Retained::as_ptr(&delegate).cast::<()>()
            }) {
                view.setNavigationDelegate(Some(&delegate.ivars().original));
                return Err("native navigation delegate was not installed".to_owned());
            }
            owners.insert(
                label.to_owned(),
                Owner {
                    view: retained_view,
                    delegate,
                },
            );
        }
        Ok(())
    })
}

pub(crate) async fn install(app: &tauri::AppHandle, bootstrap: &Bootstrap) -> Result<(), String> {
    let policy = FramePolicy::new(bootstrap)?;
    let window = app
        .get_webview_window("main")
        .ok_or("native window is unavailable")?;
    let (sender, receiver) = tokio::sync::oneshot::channel();
    window
        .with_webview(move |platform| {
            let result = MainThreadMarker::new()
                .ok_or_else(|| "native navigation install is not on main thread".to_owned())
                .and_then(|mtm| {
                    // SAFETY: Tauri's macOS PlatformWebview::inner is the live WKWebView pointer.
                    let view = unsafe { &*platform.inner().cast::<WKWebView>() };
                    attach("main", view, policy, mtm)
                });
            if sender.send(result).is_err() {
                detach("main");
            }
        })
        .map_err(|_| "native navigation install could not be scheduled".to_owned())?;
    // Scheduling is not acknowledgement; bootstrap must await the actual native setter.
    receiver
        .await
        .map_err(|_| "native navigation installation was cancelled".to_owned())??;
    #[cfg(feature = "native-apps-test")]
    eprintln!("WALTR_NATIVE_PROBE navigation-installed");
    Ok(())
}

pub(crate) fn detach(label: &str) {
    assert!(
        MainThreadMarker::new().is_some(),
        "native navigation teardown requires main thread"
    );
    OWNERS.with(|owners| {
        if let Some(owner) = owners.borrow_mut().remove(label) {
            // SAFETY: Owner holds the view and both delegates on the main thread. Restore
            // only our exact weak slot; never overwrite a later delegate installation.
            unsafe {
                if owner.view.navigationDelegate().is_some_and(|current| {
                    Retained::as_ptr(&current).cast::<()>()
                        == Retained::as_ptr(&owner.delegate).cast::<()>()
                }) {
                    owner
                        .view
                        .setNavigationDelegate(Some(&owner.delegate.ivars().original));
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    fn policy() -> FramePolicy {
        FramePolicy::new(&Bootstrap {
            container_url: "http://127.0.0.1:32101/container.html".to_owned(),
            #[cfg(feature = "native-apps-test")]
            probe_url: "http://127.0.0.1:32102/probe/container.html".to_owned(),
        })
        .unwrap()
    }

    #[test]
    fn target_frame_not_source_controls_main_navigation_and_popups() {
        let policy = policy();
        assert_eq!(
            policy.decide("tauri://localhost/index.html", Some(true), false, None),
            Decision::Forward
        );
        for target in [Some(true), None] {
            assert_eq!(
                policy.decide(
                    "http://127.0.0.1:32101/container.html",
                    target,
                    false,
                    Some("tauri://localhost/index.html")
                ),
                Decision::Cancel
            );
        }
        assert_eq!(
            policy.decide("tauri://localhost/index.html", Some(false), false, None),
            Decision::Cancel
        );
    }

    #[test]
    fn subframes_are_generation_bound_path_restricted_and_download_free() {
        let policy = policy();
        for value in [
            "http://127.0.0.1:32101/container.html",
            "http://127.0.0.1:32101/views/opaque-id",
            #[cfg(feature = "native-apps-test")]
            "http://127.0.0.1:32102/probe/container.html",
        ] {
            assert_eq!(
                policy.decide(value, Some(false), false, None),
                Decision::Allow,
                "{value}"
            );
            assert_eq!(
                policy.decide(value, Some(false), true, None),
                Decision::Cancel
            );
        }
        for value in [
            #[cfg(not(feature = "native-apps-test"))]
            "http://127.0.0.1:32102/probe/container.html",
            "http://127.0.0.1:32103/container.html",
            "http://localhost:32101/container.html",
            "http://user@127.0.0.1:32101/container.html",
            "http://127.0.0.1:32101/views/../container.js",
            "http://127.0.0.1:32101/views/a%2Fb",
            "http://127.0.0.1:32101/mcp",
            "http://127.0.0.1:32102/probe/probe.js",
            "http://127.0.0.1:32101/container.html#x",
            "https://example.com",
            "not a URL",
            "data:text/html,pwned",
            "http://127.0.0.1:32101\\container.html",
        ] {
            assert_eq!(
                policy.decide(value, Some(false), false, None),
                Decision::Cancel,
                "{value}"
            );
        }
    }

    #[test]
    fn empty_and_srcdoc_frames_need_exact_owned_outer_source() {
        let policy = policy();
        for value in ["about:blank", "about:srcdoc"] {
            assert_eq!(
                policy.decide(
                    value,
                    Some(false),
                    false,
                    Some("http://127.0.0.1:32101/container.html")
                ),
                Decision::Allow
            );
            assert_eq!(
                policy.decide(
                    value,
                    Some(true),
                    false,
                    Some("http://127.0.0.1:32101/container.html")
                ),
                Decision::Cancel
            );
            assert_eq!(
                policy.decide(
                    value,
                    Some(false),
                    false,
                    Some("http://127.0.0.1:32103/probe/container.html")
                ),
                Decision::Cancel
            );
            assert_eq!(
                policy.decide(value, Some(false), false, None),
                Decision::Cancel
            );
        }
    }

    #[test]
    fn container_generation_fragment_is_the_only_permitted_asset_fragment() {
        let policy = policy();
        assert_eq!(policy.decide("http://127.0.0.1:32101/container.html#hostOrigin=tauri%3A%2F%2Flocalhost&generation=01234567-89ab-4def-8123-456789abcdef", Some(false), false, None), Decision::Allow);
        for value in [
            "http://127.0.0.1:32101/container.html#hostOrigin=https%3A%2F%2Fevil.test&generation=01234567-89ab-4def-8123-456789abcdef",
            "http://127.0.0.1:32101/container.html#hostOrigin=tauri%3A%2F%2Flocalhost&generation=not-a-uuid",
            "http://127.0.0.1:32101/container.html?endpoint=private",
            "http://127.0.0.1:32101/views/id#unexpected",
        ] {
            assert_eq!(
                policy.decide(value, Some(false), false, None),
                Decision::Cancel
            );
        }
    }

    #[test]
    fn resolved_responses_cannot_redirect_to_api_or_downloads() {
        let policy = policy();
        assert_eq!(
            policy.response("http://127.0.0.1:32101/views/id", false, false),
            Decision::Allow
        );
        assert_eq!(
            policy.response("tauri://localhost/index.html", true, false),
            Decision::Forward
        );
        for main in [true, false] {
            for value in [
                "http://127.0.0.1:32103/container.html",
                "http://127.0.0.1:32101/mcp",
                "http://user:pass@127.0.0.1:32101/container.html",
                "tauri://remote/index.html",
                "tauri://localhost:123/index.html",
                "not a URL",
            ] {
                assert_eq!(policy.response(value, main, false), Decision::Cancel);
            }
            assert_eq!(
                policy.response("http://127.0.0.1:32101/container.html", main, true),
                Decision::Cancel
            );
        }
        assert_eq!(
            policy.response("about:srcdoc", false, false),
            Decision::Allow
        );
        assert_eq!(
            policy.response("about:srcdoc", true, false),
            Decision::Cancel
        );
    }
}
