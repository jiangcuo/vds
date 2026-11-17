//! macOS: Finder does not put a double-clicked file on the command line, it starts the app and
//! sends the file in an Apple Event, which `NSApplication` hands to its delegate as
//! `application:openURLs:`.
//!
//! Each URL starts a connection on its own thread while the event loop keeps running, so files
//! opened while a session is up (LaunchServices sends them to the running app) start sessions
//! too. The process exits with the FreeRDP code of the last session to end. Files opened at
//! launch arrive before `applicationDidFinishLaunching:`; if none did, the app was started
//! without one and exits.

use std::fs::File;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{declare_class, msg_send_id, mutability, ClassType, DeclaredClass};
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate};
use objc2_foundation::{MainThreadMarker, NSArray, NSNotification, NSObject, NSObjectProtocol, NSURL};

/// Sessions still running.
static SESSIONS: AtomicUsize = AtomicUsize::new(0);
/// Whether any file was opened.
static OPENED: AtomicBool = AtomicBool::new(false);

struct Ivars {
    log: Option<File>,
    console: bool,
}

declare_class!(
    struct Delegate;

    // SAFETY:
    // - NSObject has no subclassing requirements.
    // - MainThreadOnly is right for an application delegate.
    // - Delegate does not implement Drop.
    unsafe impl ClassType for Delegate {
        type Super = NSObject;
        type Mutability = mutability::MainThreadOnly;
        const NAME: &'static str = "VdsConnectDelegate";
    }

    impl DeclaredClass for Delegate {
        type Ivars = Ivars;
    }

    unsafe impl NSObjectProtocol for Delegate {}

    unsafe impl NSApplicationDelegate for Delegate {
        #[method(application:openURLs:)]
        fn open_urls(&self, _application: &NSApplication, urls: &NSArray<NSURL>) {
            for url in urls.iter() {
                // SAFETY: plain property getters of a valid NSURL.
                let path = unsafe { url.isFileURL().then(|| url.path()).flatten() };
                match path {
                    Some(path) => self.start(PathBuf::from(path.to_string())),
                    None => tracing::warn!("ignoring a URL that is not a file"),
                }
            }
        }

        #[method(applicationDidFinishLaunching:)]
        fn did_finish_launching(&self, _notification: &NSNotification) {
            if !OPENED.load(Ordering::SeqCst) {
                tracing::error!("no .vds file given: open one from the Finder or the browser");
                std::process::exit(1);
            }
        }
    }
);

impl Delegate {
    fn new(log: Option<File>, console: bool, mtm: MainThreadMarker) -> Retained<Self> {
        let this = mtm.alloc().set_ivars(Ivars { log, console });
        // SAFETY: NSObject's init on a freshly allocated object.
        unsafe { msg_send_id![super(this), init] }
    }

    /// Runs a session for `path` on a new thread; the last session to end exits the process.
    fn start(&self, path: PathBuf) {
        OPENED.store(true, Ordering::SeqCst);
        SESSIONS.fetch_add(1, Ordering::SeqCst);

        let log = self.ivars().log.as_ref().and_then(|log| log.try_clone().ok());
        let console = self.ivars().console;
        std::thread::spawn(move || {
            let code = match crate::connect(&path, log, console) {
                Ok(code) => code,
                Err(error) => {
                    tracing::error!("{error:#}");
                    1
                }
            };
            if SESSIONS.fetch_sub(1, Ordering::SeqCst) == 1 {
                std::process::exit(code);
            }
        });
    }
}

/// Runs the event loop, starting a session for each file opened; never returns.
pub fn run(log: Option<File>, console: bool) -> ! {
    let Some(mtm) = MainThreadMarker::new() else {
        tracing::error!("not on the main thread");
        std::process::exit(1);
    };

    let app = NSApplication::sharedApplication(mtm);
    // No Dock icon or menu bar: the FreeRDP window is the user interface.
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    let delegate = Delegate::new(log, console, mtm);
    app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));

    // SAFETY: called once, on the main thread, with the delegate kept alive above.
    unsafe { app.run() };
    std::process::exit(0);
}
