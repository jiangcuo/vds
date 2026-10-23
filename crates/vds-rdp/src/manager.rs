//! Accepts connections and owns the sessions. Runs on the RDP thread's `LocalSet`
//! (IronRDP connection futures are not `Send`).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use ironrdp_server::{Credentials, RdpServerSecurity};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};
use vds_core::Core;
use vds_core::auth::{AccessDecision, AuthMode};

use crate::error::{Error, Result};
use crate::session::{self, Access, ManagerEvent, SessionShared};
use crate::tls::TlsIdentity;

/// Connections (authenticated or not) handled at the same time.
const MAX_SESSIONS: usize = 8;
/// How long shutdown waits for sessions to close.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// Requests from the host to the RDP thread.
pub(crate) enum Command {
    Listen(SocketAddr, oneshot::Sender<Result<SocketAddr>>),
    AddClient(std::net::TcpStream, bool),
    SetTls(TlsIdentity),
    DisconnectAll(String),
    Shutdown,
}

struct Manager {
    core: Core,
    tls: TlsIdentity,
    sessions: HashMap<u64, Arc<SessionShared>>,
    listeners: Vec<JoinHandle<()>>,
    conn_tx: mpsc::UnboundedSender<(TcpStream, Option<SocketAddr>)>,
    mgr_tx: mpsc::UnboundedSender<ManagerEvent>,
}

pub(crate) async fn run(core: Core, tls: TlsIdentity, mut commands: mpsc::UnboundedReceiver<Command>) {
    let (conn_tx, mut conn_rx) = mpsc::unbounded_channel();
    let (mgr_tx, mut mgr_rx) = mpsc::unbounded_channel();
    let mut m = Manager {
        core,
        tls,
        sessions: HashMap::new(),
        listeners: Vec::new(),
        conn_tx,
        mgr_tx,
    };
    loop {
        tokio::select! {
            cmd = commands.recv() => match cmd {
                None | Some(Command::Shutdown) => break,
                Some(Command::Listen(addr, reply)) => {
                    let _ = reply.send(m.listen(addr).await);
                }
                Some(Command::AddClient(stream, skip_auth)) => m.add_client(stream, skip_auth),
                Some(Command::SetTls(tls)) => {
                    info!("TLS certificate replaced");
                    m.tls = tls;
                }
                Some(Command::DisconnectAll(reason)) => {
                    for s in m.sessions.values() {
                        s.request_disconnect(&reason);
                    }
                }
            },
            Some((stream, peer)) = conn_rx.recv() => m.start_session(stream, peer, false),
            Some(ev) = mgr_rx.recv() => m.on_session_event(ev),
        }
    }
    m.shutdown(&mut mgr_rx).await;
}

impl Manager {
    async fn listen(&mut self, addr: SocketAddr) -> Result<SocketAddr> {
        let listener = TcpListener::bind(addr)
            .await
            .map_err(|source| Error::Listen { addr, source })?;
        let local = listener.local_addr()?;
        let tx = self.conn_tx.clone();
        self.listeners.push(tokio::task::spawn_local(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, peer)) => {
                        if tx.send((stream, Some(peer))).is_err() {
                            return;
                        }
                    }
                    Err(e) => {
                        vds_core::warn_ratelimited!(error = %e, "accept failed");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
            }
        }));
        info!(addr = %local, "listening for RDP connections");
        Ok(local)
    }

    fn add_client(&mut self, stream: std::net::TcpStream, skip_auth: bool) {
        let stream = match stream.set_nonblocking(true).and_then(|()| TcpStream::from_std(stream)) {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "unusable client socket");
                return;
            }
        };
        let peer = stream.peer_addr().ok();
        self.start_session(stream, peer, skip_auth);
    }

    fn start_session(&mut self, stream: TcpStream, peer: Option<SocketAddr>, skip_auth: bool) {
        if self.sessions.len() >= MAX_SESSIONS {
            info!(peer = ?peer, reason = "too many connections", "connection refused");
            return;
        }
        if let Err(e) = stream.set_nodelay(true) {
            debug!(error = %e, "cannot disable Nagle's algorithm");
        }
        let decision = if skip_auth {
            AccessDecision::Open
        } else {
            self.core.credentials().decision(SystemTime::now())
        };
        let mode = self.core.credentials().mode();
        let acceptor = self.tls.acceptor.clone();
        let (security, credentials, user) = match decision {
            AccessDecision::Open => (RdpServerSecurity::Tls(acceptor), None, None),
            AccessDecision::Check(c) => {
                let security = if mode == AuthMode::Nla {
                    RdpServerSecurity::Hybrid((acceptor, self.tls.public_key.clone()))
                } else {
                    RdpServerSecurity::Tls(acceptor)
                };
                let creds = Credentials {
                    username: c.username.clone(),
                    password: c.password.expose().clone(),
                    domain: c.domain.clone(),
                };
                (security, Some(creds), Some(c.username))
            }
            AccessDecision::Denied(reason) => {
                info!(peer = ?peer, reason, "connection refused");
                return;
            }
        };
        let sid = self.core.next_session_id();
        let shared = SessionShared::new(sid, peer, self.core.clone(), user, self.mgr_tx.clone());
        self.sessions.insert(sid, Arc::clone(&shared));
        tokio::task::spawn_local(session::run(shared, stream, Access { security, credentials }));
    }

    fn on_session_event(&mut self, ev: ManagerEvent) {
        match ev {
            ManagerEvent::Activated(sid) => {
                // One active client at a time: a newly authenticated client replaces the others.
                for (other, s) in &self.sessions {
                    if *other != sid && s.is_activated() {
                        s.request_disconnect("replaced by a new connection");
                    }
                }
            }
            ManagerEvent::Ended(sid) => {
                self.sessions.remove(&sid);
            }
        }
    }

    async fn shutdown(&mut self, mgr_rx: &mut mpsc::UnboundedReceiver<ManagerEvent>) {
        for l in self.listeners.drain(..) {
            l.abort();
        }
        for s in self.sessions.values() {
            s.request_disconnect("server shutting down");
        }
        let deadline = tokio::time::Instant::now() + SHUTDOWN_GRACE;
        while !self.sessions.is_empty() {
            match tokio::time::timeout_at(deadline, mgr_rx.recv()).await {
                Ok(Some(ManagerEvent::Ended(sid))) => {
                    self.sessions.remove(&sid);
                }
                Ok(Some(ManagerEvent::Activated(_))) => {}
                Ok(None) | Err(_) => break,
            }
        }
        debug!("RDP manager stopped");
    }
}
