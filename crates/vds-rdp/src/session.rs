//! One client connection.
//!
//! A fresh `ironrdp_server::RdpServer` is built for every connection, so no event queued for
//! a previous client (EGFX frames in particular) can leak into a new one.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use ironrdp_server::{ConnectionHandler, Credentials, RdpServer, RdpServerSecurity, ServerEvent};
use parking_lot::Mutex;
use tokio::net::TcpStream;
use tokio::sync::{Notify, mpsc};
use tracing::{Instrument as _, debug, info, info_span, warn};
use vds_core::Core;
use vds_core::codec::{Codec, EgfxStatus, Transport};
use vds_core::debug::SessionDebug;
use vds_core::event::{ClientEvent, ClientInfo, ClientState, HostClipboardEvent};
use vds_core::session::SessionSink;
use vds_core::signal::Signal;

use crate::client_info::{
    CHANNEL_AINPUT, CHANNEL_CLIPRDR, CHANNEL_DISPLAY_CONTROL, CHANNEL_ECHO, CHANNEL_GFX, CHANNEL_RDPSND, ClientHandler,
    ClientRecord,
};
use crate::clipboard::{self, ClipboardFactory, ClipboardState};
use crate::display::DisplayHandler;
use crate::egfx::{GfxFactory, VideoWorker};
use crate::input::InputHandler;
use crate::sound::{self, AudioState, SoundFactory};

/// How long a disconnect request waits for the graceful shutdown before the socket is dropped.
const DISCONNECT_GRACE: Duration = Duration::from_millis(500);

/// Notifications from sessions to the connection manager.
#[derive(Debug)]
pub(crate) enum ManagerEvent {
    /// The session finished authentication and capability exchange.
    Activated(u64),
    /// The session ended.
    Ended(u64),
}

/// Security parameters decided by the manager for one connection.
pub(crate) struct Access {
    pub(crate) security: RdpServerSecurity,
    pub(crate) credentials: Option<Credentials>,
}

/// State shared by the IronRDP handlers of one connection.
pub(crate) struct SessionShared {
    pub(crate) sid: u64,
    pub(crate) peer: Option<SocketAddr>,
    pub(crate) core: Core,
    pub(crate) started: Instant,
    user: Option<String>,
    ev_tx: Mutex<Option<mpsc::UnboundedSender<ServerEvent>>>,
    pub(crate) gfx: Mutex<Option<ironrdp_server::GfxServerHandle>>,
    egfx: Mutex<EgfxStatus>,
    /// Raised on EGFX state changes and disconnect requests (wakes the display stream).
    pub(crate) mode_signal: Signal,
    transport: Mutex<Option<Transport>>,
    /// Desktop size the client currently believes in (legacy transport).
    pub(crate) client_desktop: Mutex<Option<(u32, u32)>>,
    activated: AtomicBool,
    stopping: AtomicBool,
    cancel: Notify,
    cancel_reason: Mutex<Option<String>>,
    manager_tx: mpsc::UnboundedSender<ManagerEvent>,
    video: Mutex<Option<VideoWorker>>,
    pub(crate) clipboard: Mutex<ClipboardState>,
    pub(crate) audio: Mutex<AudioState>,
    /// Diagnostic files, `None` unless `-rdp debug-dir` is set.
    pub(crate) debug: Option<Arc<SessionDebug>>,
    /// What the client told us during connection setup (`client.json`).
    pub(crate) client: ClientRecord,
    /// Frames handed to the client but not acknowledged yet (`frames.csv` column).
    frames_in_flight: AtomicU32,
}

impl SessionShared {
    pub(crate) fn new(
        sid: u64,
        peer: Option<SocketAddr>,
        core: Core,
        user: Option<String>,
        manager_tx: mpsc::UnboundedSender<ManagerEvent>,
    ) -> Arc<Self> {
        Arc::new(Self {
            sid,
            peer,
            core,
            started: Instant::now(),
            user,
            ev_tx: Mutex::new(None),
            gfx: Mutex::new(None),
            egfx: Mutex::new(EgfxStatus::Pending),
            mode_signal: Signal::new(),
            transport: Mutex::new(None),
            client_desktop: Mutex::new(None),
            activated: AtomicBool::new(false),
            stopping: AtomicBool::new(false),
            cancel: Notify::new(),
            cancel_reason: Mutex::new(None),
            manager_tx,
            video: Mutex::new(None),
            clipboard: Mutex::new(ClipboardState::default()),
            audio: Mutex::new(AudioState::default()),
            debug: vds_core::debug::session(sid),
            client: ClientRecord::default(),
            frames_in_flight: AtomicU32::new(0),
        })
    }

    /// Name of the authenticated user, if authentication succeeded.
    pub(crate) fn user_name(&self) -> Option<&str> {
        self.user.as_deref()
    }

    /// Frames sent to the client but not acknowledged yet.
    pub(crate) fn frames_in_flight(&self) -> u32 {
        self.frames_in_flight.load(Ordering::Relaxed)
    }

    /// Accounts for a frame just handed to the client.
    pub(crate) fn frame_sent(&self) {
        self.frames_in_flight.fetch_add(1, Ordering::Relaxed);
    }

    /// Sets the frame count from the client's queue depth.
    pub(crate) fn set_frames_in_flight(&self, queue_depth: u32) {
        self.frames_in_flight.store(queue_depth, Ordering::Relaxed);
    }

    /// Milliseconds since the session started (EGFX and rdpsnd timestamps).
    pub(crate) fn timestamp_ms(&self) -> u32 {
        u32::try_from(self.started.elapsed().as_millis()).unwrap_or(u32::MAX)
    }

    pub(crate) fn set_event_sender(&self, tx: mpsc::UnboundedSender<ServerEvent>) {
        *self.ev_tx.lock() = Some(tx);
    }

    /// A sender for the IronRDP client loop, for work finished off the session task.
    pub(crate) fn event_sender(&self) -> Option<mpsc::UnboundedSender<ServerEvent>> {
        self.ev_tx.lock().clone()
    }

    /// Queues an event for the IronRDP client loop. Returns `false` if the session is gone.
    pub(crate) fn send_event(&self, ev: ServerEvent) -> bool {
        match self.ev_tx.lock().as_ref() {
            Some(tx) => tx.send(ev).is_ok(),
            None => false,
        }
    }

    pub(crate) fn egfx_status(&self) -> EgfxStatus {
        *self.egfx.lock()
    }

    pub(crate) fn set_egfx_status(&self, status: EgfxStatus) {
        *self.egfx.lock() = status;
        self.mode_signal.raise();
        // The video thread waits on the display signal.
        self.core.display().signal().raise();
    }

    pub(crate) fn transport(&self) -> Option<Transport> {
        *self.transport.lock()
    }

    pub(crate) fn set_transport(&self, transport: Transport) {
        *self.transport.lock() = Some(transport);
        self.client.set_transport(transport.codec_name());
        self.core
            .client_modify(self.sid, |i| i.codec = Some(transport.codec_name()));
    }

    pub(crate) fn is_activated(&self) -> bool {
        self.activated.load(Ordering::Acquire)
    }

    pub(crate) fn is_stopping(&self) -> bool {
        self.stopping.load(Ordering::Acquire)
    }

    /// Called when the client finished connecting (first display update request).
    pub(crate) fn activate(self: &Arc<Self>) {
        if self.activated.swap(true, Ordering::AcqRel) {
            return;
        }
        self.core.credentials().consume();
        let sink: Arc<dyn SessionSink> = Arc::clone(self) as Arc<dyn SessionSink>;
        self.core.register_session(self.sid, sink);
        self.core.client_modify(self.sid, |i| i.user.clone_from(&self.user));
        let _ = self.manager_tx.send(ManagerEvent::Activated(self.sid));
        info!(user = self.user.as_deref().unwrap_or(""), "client authenticated");
        let mut info = ClientInfo::new(self.sid, self.peer);
        info.user.clone_from(&self.user);
        self.core
            .push_event(ClientEvent::Client(info, ClientState::Initialized));
    }

    /// Requests the end of the session (from any thread).
    pub(crate) fn request_disconnect(&self, reason: &str) {
        {
            let mut r = self.cancel_reason.lock();
            if r.is_none() {
                *r = Some(reason.to_owned());
            }
        }
        self.stopping.store(true, Ordering::Release);
        let _ = self.send_event(ServerEvent::Quit(reason.to_owned()));
        self.mode_signal.raise();
        self.cancel.notify_one();
    }

    pub(crate) fn start_video(self: &Arc<Self>) {
        let mut video = self.video.lock();
        if video.is_none() {
            match VideoWorker::spawn(Arc::clone(self)) {
                Ok(w) => *video = Some(w),
                Err(e) => warn!(error = %e, "failed to start the video thread"),
            }
        }
    }

    pub(crate) fn stop_video(&self) {
        let worker = self.video.lock().take();
        if let Some(w) = worker {
            w.stop();
        }
    }
}

impl SessionSink for SessionShared {
    fn clipboard(&self, event: HostClipboardEvent) {
        clipboard::on_host_event(self, event);
    }

    fn audio_out(&self, pcm: &[i16]) {
        sound::on_host_audio(self, pcm);
    }

    fn audio_volume(&self, left: u16, right: u16, mute: bool) {
        sound::on_host_volume(self, left, right, mute);
    }

    fn disconnect(&self, reason: &str) {
        self.request_disconnect(reason);
    }
}

fn build_server(shared: &Arc<SessionShared>, access: Access) -> anyhow::Result<RdpServer> {
    let config = shared.core.config();
    let remotefx = if config.codecs.contains(Codec::RemoteFx) {
        "remotefx:on"
    } else {
        "remotefx:off"
    };
    let codecs = ironrdp_pdu::rdp::capability_sets::server_codecs_capabilities(&[remotefx])
        .map_err(|e| anyhow::anyhow!("codec configuration: {e}"))?;

    let builder = RdpServer::builder().with_addr(([127, 0, 0, 1], 0));
    let builder = match access.security {
        RdpServerSecurity::None => builder.with_no_security(),
        RdpServerSecurity::Tls(acceptor) => builder.with_tls(acceptor),
        RdpServerSecurity::Hybrid((acceptor, key)) => builder.with_hybrid(acceptor, key),
    };
    // openh264 is built from source: AVC420 is always available when allowed.
    let gfx = config
        .codecs
        .contains(Codec::Avc420)
        .then(|| Box::new(GfxFactory::new(Arc::clone(shared))) as Box<dyn ironrdp_server::GfxServerFactory>);
    let cliprdr = config
        .clipboard
        .then(|| Box::new(ClipboardFactory::new(Arc::clone(shared))) as Box<dyn ironrdp_server::CliprdrServerFactory>);
    let sound = config
        .audio
        .then(|| Box::new(SoundFactory::new(Arc::clone(shared))) as Box<dyn ironrdp_server::SoundServerFactory>);

    // Channels IronRDP opens for us. The client's own requested list is not observable
    // (`client_info::UNAVAILABLE`); the three we implement ourselves are also recorded in
    // `channels_opened` once their handshake runs.
    let mut channels = vec![
        CHANNEL_DISPLAY_CONTROL.to_owned(),
        CHANNEL_ECHO.to_owned(),
        CHANNEL_AINPUT.to_owned(),
    ];
    if gfx.is_some() {
        channels.push(CHANNEL_GFX.to_owned());
    }
    if cliprdr.is_some() {
        channels.push(CHANNEL_CLIPRDR.to_owned());
    }
    if sound.is_some() {
        channels.push(CHANNEL_RDPSND.to_owned());
    }
    shared.client.set_channels(channels);

    let mut server = builder
        .with_input_handler(InputHandler::new(Arc::clone(shared)))
        .with_display_handler(DisplayHandler::new(Arc::clone(shared)))
        .with_connection_handler(Some(
            Box::new(ClientHandler::new(Arc::clone(shared))) as Box<dyn ConnectionHandler>
        ))
        .with_bitmap_codecs(codecs)
        .with_gfx_factory(gfx)
        .with_cliprdr_factory(cliprdr)
        .with_sound_factory(sound)
        .build();
    // Factories without a ServerEventSender role (no clipboard, no sound, no EGFX) still need
    // the sender for Quit and pointer-independent events.
    shared.set_event_sender(server.event_sender().clone());
    server.set_credentials(access.credentials);
    Ok(server)
}

/// Runs one connection to completion.
pub(crate) async fn run(shared: Arc<SessionShared>, stream: TcpStream, access: Access) {
    let span = info_span!(
        "session",
        sid = shared.sid,
        peer = %shared.peer.map_or_else(|| String::from("-"), |p| p.to_string())
    );
    run_inner(Arc::clone(&shared), stream, access).instrument(span).await;
}

async fn run_inner(shared: Arc<SessionShared>, stream: TcpStream, access: Access) {
    let core = shared.core.clone();
    let info = ClientInfo::new(shared.sid, shared.peer);
    core.client_update(info.clone());
    core.push_event(ClientEvent::Client(info, ClientState::Connected));
    info!("client connected");

    let result = match build_server(&shared, access) {
        Ok(mut server) => {
            let conn = server.run_connection(stream);
            tokio::pin!(conn);
            let r = tokio::select! {
                r = &mut conn => r,
                () = shared.cancel.notified() => {
                    match tokio::time::timeout(DISCONNECT_GRACE, &mut conn).await {
                        Ok(r) => r,
                        Err(_) => Ok(()),
                    }
                }
            };
            r.map_err(anyhow::Error::from)
        }
        Err(e) => Err(e),
    };

    shared.stopping.store(true, Ordering::Release);
    shared.stop_video();
    core.unregister_session(shared.sid);

    // A client that just drops the TCP connection is a normal disconnection.
    let closed_by_peer = result.as_ref().err().is_some_and(|e| {
        e.chain().any(|c: &(dyn std::error::Error + 'static)| {
            c.downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::UnexpectedEof)
        })
    });
    let reason = match (shared.cancel_reason.lock().clone(), &result) {
        (Some(r), _) => r,
        (None, Err(_)) if closed_by_peer => String::from("client closed the connection"),
        (None, Err(e)) => format!("{e:#}"),
        (None, Ok(())) => String::from("client disconnected"),
    };
    match &result {
        Err(e) if shared.cancel_reason.lock().is_none() && !closed_by_peer => {
            info!(reason = %reason, error = %e, "client disconnected");
        }
        _ => info!(reason = %reason, "client disconnected"),
    }
    let mut info = core
        .client_remove(shared.sid)
        .unwrap_or_else(|| ClientInfo::new(shared.sid, shared.peer));
    info.reason = Some(reason);
    core.push_event(ClientEvent::Client(info, ClientState::Disconnected));
    shared.client.write(&shared);
    // The footer goes in now, while we know the session is over: the object itself may stay
    // alive inside IronRDP's tasks for a long time, and the footer is what says "complete".
    if let Some(session) = &shared.debug {
        session.finish();
    }
    debug!("session finished");
    let _ = shared.manager_tx.send(ManagerEvent::Ended(shared.sid));
}
