//! In-app updates. macOS delegates to the Sparkle framework that
//! `scripts/macos-app.sh` embeds in DuckTable.app; the feed it reads is the
//! `ducktable-updates` GitHub release (docs/UPDATES.md).
//!
//! Every user-facing moment — the one-time "check automatically?" prompt,
//! the checking window, the update sheet, download, install and relaunch —
//! is Sparkle's own standard UI. DuckTable starts the updater, forwards the
//! Check for Updates menu item, and has one say of its own: Install and
//! Relaunch quits the app, so it is held until the quit dialog has asked
//! whatever ⌘Q would ask (docs/EDITING.md, "Dialogs"). This file is glue,
//! not a user driver of its own.
//!
//! Debug builds stay dormant so the dev bundle never offers to replace
//! itself with a release; `DUCKTABLE_FORCE_UPDATER=1` exercises the real
//! flow from one. A bare `cargo run` binary has no embedded framework and
//! stays dormant too, in which case the menu item is omitted.

use gpui_kit::Global;

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

    pub fn relaunch_requests(&self) -> async_channel::Receiver<()> {
        async_channel::unbounded().1
    }

    pub fn install_waiting(&self) -> bool {
        false
    }

    pub fn install(&self) {}
}

#[cfg(target_os = "macos")]
mod macos {
    use std::cell::RefCell;
    use std::ffi::{CStr, CString, c_char};
    use std::os::unix::ffi::OsStrExt as _;
    use std::path::PathBuf;
    use std::ptr;

    use block2::{Block, RcBlock};
    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, AnyObject, Bool, NSObject};
    use objc2::{ClassType as _, MainThreadMarker, define_class, msg_send};

    thread_local! {
        /// Sparkle's go-ahead for a relaunch it was asked to hold: calling
        /// it installs the update and relaunches. Sparkle calls the delegate
        /// on the main thread, and the app answers there too.
        static INSTALL: RefCell<Option<RcBlock<dyn Fn()>>> = const { RefCell::new(None) };
        /// Where the delegate says a relaunch is waiting.
        static REQUESTS: RefCell<Option<async_channel::Sender<()>>> = const { RefCell::new(None) };
    }

    define_class!(
        // SAFETY: NSObject has no subclassing requirements, and this class
        // has no ivars and no Drop.
        #[unsafe(super(NSObject))]
        #[name = "DuckTableUpdaterDelegate"]
        struct UpdaterDelegate;

        impl UpdaterDelegate {
            /// Install and Relaunch was chosen and the update is staged.
            /// Sparkle asks this once per update: the relaunch is held, and
            /// the app decides when to go on (`Updater::install`). Held but
            /// never released, the update installs when DuckTable quits,
            /// without relaunching it, since Sparkle's installer is already
            /// waiting for the app to end.
            #[unsafe(method(updater:shouldPostponeRelaunchForUpdate:untilInvokingBlock:))]
            fn hold_relaunch(
                &self,
                _updater: *mut AnyObject,
                _item: *mut AnyObject,
                install: &Block<dyn Fn()>,
            ) -> Bool {
                INSTALL.with(|slot| *slot.borrow_mut() = Some(install.copy()));
                let told = REQUESTS.with(|requests| {
                    requests.borrow().as_ref().is_some_and(|tx| tx.try_send(()).is_ok())
                });
                // With nobody to ask, Sparkle goes on as it would have.
                if !told {
                    INSTALL.with(|slot| slot.borrow_mut().take());
                }
                Bool::new(told)
            }
        }
    );

    pub struct Updater {
        updater: Retained<AnyObject>,
        /// Sparkle's standard user driver, kept alive for the updater's
        /// lifetime alongside it.
        _user_driver: Retained<AnyObject>,
        /// The updater's delegate. Sparkle holds it weakly, so it is kept
        /// here.
        _delegate: Retained<UpdaterDelegate>,
        requests: async_channel::Receiver<()>,
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
            let delegate: Retained<UpdaterDelegate> =
                unsafe { msg_send![UpdaterDelegate::class(), new] };
            let updater = unsafe {
                let allocated: *mut AnyObject = msg_send![updater_class, alloc];
                let initialized: *mut AnyObject = msg_send![
                    allocated,
                    initWithHostBundle: main_bundle,
                    applicationBundle: main_bundle,
                    userDriver: &*user_driver,
                    delegate: &*delegate
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

            let (tx, requests) = async_channel::unbounded();
            REQUESTS.with(|slot| *slot.borrow_mut() = Some(tx));
            Some(Self {
                updater,
                _user_driver: user_driver,
                _delegate: delegate,
                requests,
            })
        }

        /// The menu item: a user-initiated check through Sparkle's standard
        /// windows. Sparkle's own scheduled checks run silently beside it.
        pub fn check_for_updates(&self) {
            let _: () = unsafe { msg_send![&*self.updater, checkForUpdates] };
        }

        /// One message each time Sparkle holds a relaunch for the app to
        /// release (`install`).
        pub fn relaunch_requests(&self) -> async_channel::Receiver<()> {
            self.requests.clone()
        }

        /// A staged update is waiting for the app's go-ahead. Sparkle asks
        /// only once per update, so while this holds, Check for Updates
        /// asks again here rather than through Sparkle, whose second
        /// Install and Relaunch would not wait.
        pub fn install_waiting(&self) -> bool {
            INSTALL.with(|slot| slot.borrow().is_some())
        }

        /// Go ahead: Sparkle installs the waiting update, ends the app and
        /// opens the new one.
        pub fn install(&self) {
            let install = INSTALL.with(|slot| slot.borrow_mut().take());
            if let Some(install) = install {
                install.call(());
            }
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
