//! The macOS side: `NSUserNotification` through `objc2`, the click delegate, bundle-identity
//! adoption and the `osascript` fallback. Ported from zeron's `crates/notify` (see
//! docs/internal/UPSTREAM.md). The only unsafe code in the crate; `lib.rs` keeps the API safe.

#![allow(deprecated)]

use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Imp, Sel};
use objc2::{AnyThread, ClassType, MainThreadMarker, define_class, msg_send, sel};
use objc2_app_kit::{NSApplication, NSWorkspace};
use objc2_foundation::{
    NSBundle, NSDictionary, NSObject, NSString, NSUserNotification, NSUserNotificationCenter,
};

use crate::{Banner, applescript_escape, clicked};

/// `userInfo` key carrying the banner's target back to the click handler.
const TARGET_KEY: &str = "target";

/// The notification center for this process. `None` when the class is gone from Foundation (the
/// API is deprecated) or the process has no bundle identifier (the center answers nil).
fn center() -> Option<Retained<NSUserNotificationCenter>> {
    // Looked up by name: the typed class lookup panics if the class is ever removed.
    let class = AnyClass::get(c"NSUserNotificationCenter")?;
    unsafe { msg_send![class, defaultUserNotificationCenter] }
}

pub(crate) fn has_center() -> bool {
    center().is_some()
}

pub(crate) fn post(banner: &Banner) {
    let Some(center) = center() else {
        post_script(banner);
        return;
    };
    set_delegate(&center);
    let note = NSUserNotification::new();
    note.setTitle(Some(&NSString::from_str(&banner.title)));
    note.setInformativeText(Some(&NSString::from_str(&banner.body)));
    note.setIdentifier(Some(&NSString::from_str(&banner.id)));
    if let Some(target) = &banner.target {
        tag_target(&note, target);
    }
    center.deliverNotification(&note);
}

pub(crate) fn withdraw(id: &str) {
    let Some(center) = center() else { return };
    let delivered = center.deliveredNotifications();
    let matching: Vec<_> = (0..delivered.count())
        .map(|index| delivered.objectAtIndex(index))
        .filter(|note| note.identifier().is_some_and(|got| got.to_string() == id))
        .collect();
    for note in matching {
        center.removeDeliveredNotification(&note);
    }
}

pub(crate) fn set_badge(label: Option<&str>) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let label = label.map(NSString::from_str);
    NSApplication::sharedApplication(mtm)
        .dockTile()
        .setBadgeLabel(label.as_deref());
}

/// Attach the delegate to the center, so clicks reach the handler.
pub(crate) fn install_delegate() {
    if let Some(center) = center() {
        set_delegate(&center);
    }
}

fn set_delegate(center: &NSUserNotificationCenter) {
    let delegate = delegate();
    let _: () = unsafe { msg_send![center, setDelegate: delegate] };
}

/// No notification center: post through `osascript` on a background thread (shown as Script
/// Editor; clicks report nothing).
fn post_script(banner: &Banner) {
    let script = format!(
        "display notification \"{}\" with title \"{}\"",
        applescript_escape(&banner.body),
        applescript_escape(&banner.title),
    );
    std::thread::spawn(move || {
        let _ = Command::new("osascript")
            .args(["-e", &script])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    });
}

/// Stamp `note` with the target [`target_of`] reads back on click.
fn tag_target(note: &NSUserNotification, target: &str) {
    let key = NSString::from_str(TARGET_KEY);
    // SAFETY: an NSString is an NSObject; the cast only forgets the static type.
    let value: Retained<AnyObject> =
        unsafe { Retained::cast_unchecked(NSString::from_str(target)) };
    let info: Retained<NSDictionary<NSString, AnyObject>> =
        NSDictionary::from_retained_objects(&[&*key], &[value]);
    // SAFETY: the dictionary holds a string, a property-list type, as `userInfo` requires.
    unsafe { note.setUserInfo(Some(&info)) };
}

/// The target a clicked banner carries, if it carries one.
fn target_of(note: &NSUserNotification) -> Option<String> {
    let info = note.userInfo()?;
    let value = info.objectForKey(&NSString::from_str(TARGET_KEY))?;
    let text = value.downcast::<NSString>().ok()?;
    Some(text.to_string())
}

define_class!(
    /// Answers "present" always (overriding the center's suppress-while-frontmost policy: whether
    /// to ring is the app's decision) and reports clicks.
    #[unsafe(super(NSObject))]
    #[name = "RenoNotifyDelegate"]
    pub(crate) struct Delegate;

    impl Delegate {
        #[unsafe(method(userNotificationCenter:shouldPresentNotification:))]
        fn should_present(&self, _center: Option<&AnyObject>, _note: Option<&AnyObject>) -> bool {
            true
        }

        // AppKit calls this on the main thread when a banner is clicked.
        #[unsafe(method(userNotificationCenter:didActivateNotification:))]
        fn did_activate(&self, _center: Option<&AnyObject>, note: Option<&NSUserNotification>) {
            if let Some(target) = note.and_then(target_of) {
                clicked(target);
            }
        }
    }
);

/// The one delegate, leaked on purpose: the center holds its delegate weakly.
pub(crate) fn delegate() -> &'static Delegate {
    static DELEGATE: OnceLock<usize> = OnceLock::new();
    let address = *DELEGATE.get_or_init(|| {
        let this = Delegate::alloc().set_ivars(());
        let instance: Retained<Delegate> = unsafe { msg_send![super(this), init] };
        Retained::into_raw(instance) as usize
    });
    // SAFETY: the address came from `Retained::into_raw` and is never released.
    unsafe { &*(address as *const Delegate) }
}

/// The identity banners are attributed to once adopted.
static ADOPTED_ID: OnceLock<String> = OnceLock::new();

/// The pre-override `bundleIdentifier` IMP, forwarded to for every bundle but the main one.
static ORIGINAL: AtomicUsize = AtomicUsize::new(0);

type BundleIdentifierFn = unsafe extern "C-unwind" fn(&AnyObject, Sel) -> *mut AnyObject;

/// `-[NSBundle bundleIdentifier]` answering the adopted id for the main bundle only.
unsafe extern "C-unwind" fn bundle_identifier_override(
    this: &AnyObject,
    cmd: Sel,
) -> *mut AnyObject {
    if let Some(id) = ADOPTED_ID.get() {
        let main = NSBundle::mainBundle();
        if std::ptr::eq(this, Retained::as_ptr(&main).cast::<AnyObject>()) {
            return Retained::autorelease_ptr(NSString::from_str(id)).cast();
        }
    }
    match ORIGINAL.load(Ordering::Relaxed) {
        0 => std::ptr::null_mut(),
        imp => {
            // SAFETY: stored from the method's own implementation, with this signature.
            let original: BundleIdentifierFn = unsafe { std::mem::transmute(imp) };
            unsafe { original(this, cmd) }
        }
    }
}

/// Bundle-identity adoption (the terminal-notifier technique). The notification center refuses a
/// process whose main bundle has no identifier and resolves each banner's name and icon from the
/// identifier, so answering the app's id for the main bundle makes a `cargo run` build's banners
/// look like the installed app's. Only when this process has no bundle id of its own and the
/// system can resolve `id` to an installed app. Once per process.
pub(crate) fn adopt_bundle_id(id: &str) {
    static ATTEMPTED: OnceLock<()> = OnceLock::new();
    if ATTEMPTED.set(()).is_err() || id.is_empty() {
        return;
    }
    if NSBundle::mainBundle().bundleIdentifier().is_some() {
        return;
    }
    let installed = NSWorkspace::sharedWorkspace()
        .URLForApplicationWithBundleIdentifier(&NSString::from_str(id))
        .is_some();
    if !installed || ADOPTED_ID.set(id.to_owned()).is_err() {
        return;
    }
    let Some(method) = NSBundle::class().instance_method(sel!(bundleIdentifier)) else {
        return;
    };
    let override_fn: BundleIdentifierFn = bundle_identifier_override;
    // SAFETY: the replacement has the replaced method's signature (object, selector -> object).
    let replacement: Imp = unsafe { std::mem::transmute(override_fn) };
    let original = unsafe { method.set_implementation(replacement) };
    ORIGINAL.store(original as usize, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// The delegate hands a banner's target to the click handler, and ignores a banner with none.
    /// Calls the delegate method directly: nothing is delivered.
    #[test]
    fn clicked_banner_reports_its_target() {
        let clicked = Arc::new(Mutex::new(Vec::new()));
        let sink = clicked.clone();
        crate::set_click_handler(move |target| sink.lock().unwrap().push(target));

        let tagged = NSUserNotification::new();
        tag_target(&tagged, "run:r-42");
        let bare = NSUserNotification::new();
        let delegate = delegate();
        for note in [&tagged, &bare] {
            let _: () = unsafe {
                msg_send![
                    delegate,
                    userNotificationCenter: None::<&AnyObject>,
                    didActivateNotification: &**note
                ]
            };
        }
        assert_eq!(*clicked.lock().unwrap(), vec!["run:r-42".to_string()]);
    }

    #[test]
    fn the_delegate_always_presents() {
        let present: bool = unsafe {
            msg_send![
                delegate(),
                userNotificationCenter: None::<&AnyObject>,
                shouldPresentNotification: None::<&AnyObject>
            ]
        };
        assert!(present);
    }
}
