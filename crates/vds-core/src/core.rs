//! [`Core`]: the shared state between the host and protocol backends.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::{Mutex, MutexGuard};
use tracing::{debug, info};

use crate::auth::{ConnectedAction, CredentialStore, Credentials};
use crate::config::ServerConfig;
use crate::cursor::Cursor;
use crate::display::Display;
use crate::error::{Error, Result};
use crate::event::{ClientEvent, ClientInfo, ClipboardType, HostClipboardEvent};
use crate::session::SessionSink;

/// Maximum number of undelivered client events; beyond this the oldest are dropped.
const MAX_PENDING_EVENTS: usize = 16 * 1024;

type Waker = Box<dyn Fn() + Send + Sync>;

struct Inner {
    config: ServerConfig,
    display: Display,
    cursor: Cursor,
    credentials: Mutex<CredentialStore>,
    events: Mutex<VecDeque<ClientEvent>>,
    waker: Mutex<Option<Waker>>,
    sessions: Mutex<BTreeMap<u64, Arc<dyn SessionSink>>>,
    clients: Mutex<BTreeMap<u64, ClientInfo>>,
    next_sid: AtomicU64,
}

/// Shared state between the host and the protocol backends. Cheap to clone.
#[derive(Clone)]
pub struct Core {
    inner: Arc<Inner>,
}

impl core::fmt::Debug for Core {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Core")
            .field("config", &self.inner.config)
            .finish_non_exhaustive()
    }
}

impl Core {
    /// Creates the core after validating `config`.
    pub fn new(config: ServerConfig) -> Result<Self> {
        config.validate()?;
        crate::debug::configure(
            config.debug_dir.as_deref(),
            config.debug_flags,
            config.vm_name.as_deref(),
        )?;
        let credentials = CredentialStore::new(config.auth);
        Ok(Self {
            inner: Arc::new(Inner {
                config,
                display: Display::new(),
                cursor: Cursor::new(),
                credentials: Mutex::new(credentials),
                events: Mutex::new(VecDeque::new()),
                waker: Mutex::new(None),
                sessions: Mutex::new(BTreeMap::new()),
                clients: Mutex::new(BTreeMap::new()),
                next_sid: AtomicU64::new(1),
            }),
        })
    }

    /// Server configuration.
    pub fn config(&self) -> &ServerConfig {
        &self.inner.config
    }

    /// Display heads.
    pub fn display(&self) -> &Display {
        &self.inner.display
    }

    /// Guest cursor.
    pub fn cursor(&self) -> &Cursor {
        &self.inner.cursor
    }

    /// Credential store.
    pub fn credentials(&self) -> MutexGuard<'_, CredentialStore> {
        self.inner.credentials.lock()
    }

    /// Changes the password (QMP `set_password`).
    pub fn set_password(&self, creds: Option<Credentials>, one_time: bool, action: ConnectedAction) -> Result<()> {
        let connected = !self.inner.sessions.lock().is_empty();
        if action == ConnectedAction::Fail && connected {
            return Err(Error::InvalidArgument(String::from("a client is connected")));
        }
        let user = creds.as_ref().map(|c| c.username.clone());
        self.credentials().set(creds, one_time);
        info!(user = user.as_deref().unwrap_or(""), one_time, "credentials updated");
        if action == ConnectedAction::Disconnect && connected {
            self.disconnect_all("password changed");
        }
        Ok(())
    }

    /// Installs the function called (from any thread) when events are queued for the host.
    pub fn set_waker(&self, waker: impl Fn() + Send + Sync + 'static) {
        *self.inner.waker.lock() = Some(Box::new(waker));
    }

    /// Queues an event for the host and wakes it.
    pub fn push_event(&self, event: ClientEvent) {
        {
            let mut q = self.inner.events.lock();
            if q.len() >= MAX_PENDING_EVENTS {
                q.pop_front();
                crate::warn_ratelimited!("host event queue full, dropping oldest event");
            }
            q.push_back(event);
        }
        if let Some(w) = self.inner.waker.lock().as_ref() {
            w();
        }
    }

    /// Delivers every queued event to `f`. Called by the host on its main thread.
    pub fn drain_events(&self, mut f: impl FnMut(ClientEvent)) {
        // Take the queue first so that `f` may call back into the core without deadlocking.
        let events = core::mem::take(&mut *self.inner.events.lock());
        for ev in events {
            f(ev);
        }
    }

    /// Allocates a session id.
    pub fn next_session_id(&self) -> u64 {
        self.inner.next_sid.fetch_add(1, Ordering::Relaxed)
    }

    /// Registers an authenticated session as receiver of host data.
    pub fn register_session(&self, sid: u64, sink: Arc<dyn SessionSink>) {
        self.inner.sessions.lock().insert(sid, sink);
        self.inner.display.set_consumer(true);
    }

    /// Unregisters a session.
    pub fn unregister_session(&self, sid: u64) {
        let mut sessions = self.inner.sessions.lock();
        sessions.remove(&sid);
        if sessions.is_empty() {
            self.inner.display.set_consumer(false);
        }
    }

    /// Calls `f` for every registered session (outside of the registry lock).
    fn for_each_session(&self, mut f: impl FnMut(&dyn SessionSink)) {
        let sessions: Vec<_> = self.inner.sessions.lock().values().cloned().collect();
        for s in sessions {
            f(s.as_ref());
        }
    }

    /// Disconnects every session.
    pub fn disconnect_all(&self, reason: &str) {
        debug!(reason, "disconnecting all sessions");
        self.for_each_session(|s| s.disconnect(reason));
    }

    /// Records or updates client information.
    pub fn client_update(&self, info: ClientInfo) {
        self.inner.clients.lock().insert(info.sid, info);
    }

    /// Modifies the stored information of a client.
    pub fn client_modify(&self, sid: u64, f: impl FnOnce(&mut ClientInfo)) {
        if let Some(info) = self.inner.clients.lock().get_mut(&sid) {
            f(info);
        }
    }

    /// Removes a client and returns its last information.
    pub fn client_remove(&self, sid: u64) -> Option<ClientInfo> {
        self.inner.clients.lock().remove(&sid)
    }

    /// Information about every connected client.
    pub fn clients(&self) -> Vec<ClientInfo> {
        self.inner.clients.lock().values().cloned().collect()
    }

    /// The guest copied data (host -> client).
    pub fn clipboard_grab(&self, types: Vec<ClipboardType>) {
        self.for_each_session(|s| s.clipboard(HostClipboardEvent::Grab(types.clone())));
    }

    /// The guest clipboard was emptied.
    pub fn clipboard_release(&self) {
        self.for_each_session(|s| s.clipboard(HostClipboardEvent::Release));
    }

    /// The guest wants the client's clipboard content.
    pub fn clipboard_request(&self, ty: ClipboardType) {
        self.for_each_session(|s| s.clipboard(HostClipboardEvent::Request(ty)));
    }

    /// Guest clipboard content requested by the client.
    pub fn clipboard_data(&self, ty: ClipboardType, data: Vec<u8>) {
        self.for_each_session(|s| s.clipboard(HostClipboardEvent::Data(ty, data.clone())));
    }

    /// Guest audio samples (48 kHz, s16, stereo interleaved).
    pub fn audio_out(&self, pcm: &[i16]) {
        self.for_each_session(|s| s.audio_out(pcm));
    }

    /// Guest playback volume.
    pub fn audio_volume(&self, left: u16, right: u16, mute: bool) {
        self.for_each_session(|s| s.audio_volume(left, right, mute));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use parking_lot::Mutex;

    use super::Core;
    use crate::auth::{ConnectedAction, Credentials};
    use crate::config::ServerConfig;
    use crate::event::{ClientEvent, HostClipboardEvent};
    use crate::secret::Secret;
    use crate::session::SessionSink;

    #[derive(Default)]
    struct Sink {
        disconnects: Mutex<Vec<String>>,
    }

    impl SessionSink for Sink {
        fn clipboard(&self, _event: HostClipboardEvent) {}
        fn audio_out(&self, _pcm: &[i16]) {}
        fn audio_volume(&self, _left: u16, _right: u16, _mute: bool) {}
        fn disconnect(&self, reason: &str) {
            self.disconnects.lock().push(reason.to_owned());
        }
    }

    fn creds() -> Credentials {
        Credentials {
            username: String::from("u"),
            password: Secret::new(String::from("p")),
            domain: None,
        }
    }

    #[test]
    fn events_wake_host_and_drain_in_order() {
        let core = Core::new(ServerConfig::default()).unwrap();
        let wakes = Arc::new(AtomicUsize::new(0));
        let w = Arc::clone(&wakes);
        core.set_waker(move || {
            w.fetch_add(1, Ordering::SeqCst);
        });
        core.push_event(ClientEvent::Wheel { dx: 0, dy: 1 });
        core.push_event(ClientEvent::Wheel { dx: 0, dy: 2 });
        assert_eq!(wakes.load(Ordering::SeqCst), 2);
        let mut got = Vec::new();
        core.drain_events(|e| got.push(e));
        assert_eq!(
            got,
            [ClientEvent::Wheel { dx: 0, dy: 1 }, ClientEvent::Wheel { dx: 0, dy: 2 }]
        );
    }

    #[test]
    fn set_password_actions() {
        let core = Core::new(ServerConfig::default()).unwrap();
        let sink = Arc::new(Sink::default());
        core.register_session(1, sink.clone());
        assert!(core.set_password(Some(creds()), false, ConnectedAction::Fail).is_err());
        core.set_password(Some(creds()), false, ConnectedAction::Keep).unwrap();
        assert!(sink.disconnects.lock().is_empty());
        core.set_password(Some(creds()), false, ConnectedAction::Disconnect)
            .unwrap();
        assert_eq!(sink.disconnects.lock().len(), 1);
        core.unregister_session(1);
        core.set_password(Some(creds()), false, ConnectedAction::Fail).unwrap();
    }

    #[test]
    fn consumer_follows_sessions() {
        let core = Core::new(ServerConfig::default()).unwrap();
        assert!(!core.display().wants_frame());
        core.register_session(7, Arc::new(Sink::default()));
        assert!(core.display().wants_frame());
        core.unregister_session(7);
        assert!(!core.display().wants_frame());
    }
}
