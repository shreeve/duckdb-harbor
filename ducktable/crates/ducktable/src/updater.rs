//! In-app updates. macOS delegates to the Sparkle framework that
//! `scripts/macos-app.sh` embeds in DuckTable.app; the feed it reads is the
//! `ducktable-updates` GitHub release (docs/UPDATES.md).
//!
//! Every user-facing moment — the one-time "check automatically?" prompt,
//! the checking window, the update sheet, download, install and relaunch —
//! is Sparkle's own standard UI. DuckTable only starts the updater and
//! forwards the Check for Updates menu item, so this file is glue, not a
//! user driver of its own.
//!
//! Debug builds stay dormant so the dev bundle never offers to replace
//! itself with a release; `DUCKTABLE_FORCE_UPDATER=1` exercises the real
//! flow from one. A bare `cargo run` binary has no embedded framework and
//! stays dormant too, in which case the menu item is omitted.

use gpui::Global;

/// App-wide handle to the updater, if this build can update itself.
pub struct UpdaterState(pub Option<Updater>);

impl Global for UpdaterState {}

#[cfg(target_os = "macos")]
pub use macos::Updater;

/// Other platforms have no updater; the menu item is omitted with it.
#[cfg(not(target_os = "macos"))]
pub struct Updater;

#[cfg(not(target_os = "macos"))]
impl Updater {
    pub fn init() -> Option<Self> {
        None
    }

    pub fn check_for_updates(&self) {}
}

#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::{CStr, CString, c_char};
    use std::os::unix::ffi::OsStrExt as _;
    use std::path::PathBuf;
    use std::ptr;

    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, AnyObject};
    use objc2::{MainThreadMarker, msg_send};

    pub struct Updater {
        updater: Retained<AnyObject>,
        /// Sparkle's standard user driver, kept alive for the updater's
        /// lifetime alongside it.
        _user_driver: Retained<AnyObject>,
    }

    impl Updater {
        /// Load Sparkle and start its updater. `None` when this build cannot
        /// update itself: debug builds unless forced, and binaries running
        /// outside a bundle with an embedded framework.
        pub fn init() -> Option<Self> {
            let forced =
                std::env::var_os("DUCKTABLE_FORCE_UPDATER").is_some_and(|value| value == "1");
            if cfg!(debug_assertions) && !forced {
                return None;
            }

            // Sparkle is AppKit code and must be started on the main thread,
            // which is where GPUI runs `Application::run`.
            let _mtm = MainThreadMarker::new()?;
            let library = sparkle_library_path()?;
            let library_c = CString::new(library.as_os_str().as_bytes()).ok()?;
            let handle = unsafe { libc::dlopen(library_c.as_ptr(), libc::RTLD_NOW) };
            if handle.is_null() {
                let reason = unsafe { libc::dlerror() };
                let reason = if reason.is_null() {
                    "unknown dlopen failure".to_owned()
                } else {
                    unsafe { CStr::from_ptr(reason) }
                        .to_string_lossy()
                        .into_owned()
                };
                eprintln!("DuckTable updater: failed to load Sparkle: {reason}");
                return None;
            }

            let bundle_class = AnyClass::get(c"NSBundle")?;
            let updater_class = AnyClass::get(c"SPUUpdater")?;
            let driver_class = AnyClass::get(c"SPUStandardUserDriver")?;
            let main_bundle: *mut AnyObject = unsafe { msg_send![bundle_class, mainBundle] };
            if main_bundle.is_null() {
                return None;
            }

            let user_driver = unsafe {
                let allocated: *mut AnyObject = msg_send![driver_class, alloc];
                let initialized: *mut AnyObject = msg_send![
                    allocated,
                    initWithHostBundle: main_bundle,
                    delegate: ptr::null_mut::<AnyObject>()
                ];
                Retained::from_raw(initialized)?
            };
            let updater = unsafe {
                let allocated: *mut AnyObject = msg_send![updater_class, alloc];
                let initialized: *mut AnyObject = msg_send![
                    allocated,
                    initWithHostBundle: main_bundle,
                    applicationBundle: main_bundle,
                    userDriver: &*user_driver,
                    delegate: ptr::null_mut::<AnyObject>()
                ];
                Retained::from_raw(initialized)?
            };

            // `startUpdater:` validates the feed URL and the public key from
            // Info.plist; a bad key here is the one misconfiguration that
            // would otherwise fail silently at update time.
            let mut error: *mut AnyObject = ptr::null_mut();
            let started: bool = unsafe { msg_send![&*updater, startUpdater: &mut error] };
            if !started {
                eprintln!(
                    "DuckTable updater: Sparkle refused to start: {}",
                    error_description(error)
                );
                return None;
            }

            Some(Self {
                updater,
                _user_driver: user_driver,
            })
        }

        /// The menu item: a user-initiated check through Sparkle's standard
        /// windows. Sparkle's own scheduled checks run silently beside it.
        pub fn check_for_updates(&self) {
            let _: () = unsafe { msg_send![&*self.updater, checkForUpdates] };
        }
    }

    fn error_description(error: *mut AnyObject) -> String {
        if error.is_null() {
            return "unknown error".to_owned();
        }
        let description: *mut AnyObject = unsafe { msg_send![error, localizedDescription] };
        if description.is_null() {
            return "unknown error".to_owned();
        }
        let utf8: *const c_char = unsafe { msg_send![description, UTF8String] };
        if utf8.is_null() {
            return "unknown error".to_owned();
        }
        unsafe { CStr::from_ptr(utf8) }
            .to_string_lossy()
            .into_owned()
    }

    /// The embedded framework's dylib relative to the running executable
    /// (Contents/MacOS/ducktable → Contents/Frameworks/Sparkle.framework).
    fn sparkle_library_path() -> Option<PathBuf> {
        let executable = std::env::current_exe().ok()?;
        let contents = executable.parent()?.parent()?;
        let library = contents.join("Frameworks/Sparkle.framework/Sparkle");
        library.exists().then_some(library)
    }
}
