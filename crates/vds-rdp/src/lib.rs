//! RDP protocol backend of libvds, built on IronRDP.
//!
//! [`RdpServer`] runs on its own thread (IronRDP connection futures are not `Send`) and talks
//! to the host only through [`vds_core::Core`].

#![forbid(unsafe_code)]

mod blocks;
mod clearcodec;
mod client_info;
mod clipboard;
mod clipboard_image;
mod display;
mod egfx;
pub mod error;
mod input;
mod manager;
mod session;
mod sound;
pub mod tls;

use std::net::SocketAddr;
use std::thread::JoinHandle;

use tokio::sync::{mpsc, oneshot};
use tracing::{error, warn};
use vds_core::Core;

pub use crate::error::{Error, Result};
use crate::manager::Command;
pub use crate::tls::TlsIdentity;

/// Handle of the RDP server thread. Dropping it shuts the server down.
#[derive(Debug)]
pub struct RdpServer {
    commands: mpsc::UnboundedSender<Command>,
    thread: Option<JoinHandle<()>>,
}

impl RdpServer {
    /// Starts the RDP thread. No socket is opened until [`listen`](Self::listen) or
    /// [`add_client`](Self::add_client) is called.
    pub fn start(core: Core, tls: TlsIdentity) -> Result<Self> {
        let (tx, rx) = mpsc::unbounded_channel();
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
        let thread = std::thread::Builder::new()
            .name(String::from("vds-rdp"))
            .spawn(move || {
                let local = tokio::task::LocalSet::new();
                local.block_on(&runtime, manager::run(core, tls, rx));
            })?;
        Ok(Self {
            commands: tx,
            thread: Some(thread),
        })
    }

    fn send(&self, cmd: Command) -> Result<()> {
        self.commands.send(cmd).map_err(|_| Error::Stopped)
    }

    /// Listens on `addr` (port 0 picks a free port). Returns the bound address.
    ///
    /// Blocks until the socket is bound; must not be called from inside a Tokio runtime.
    pub fn listen(&self, addr: SocketAddr) -> Result<SocketAddr> {
        let (tx, rx) = oneshot::channel();
        self.send(Command::Listen(addr, tx))?;
        rx.blocking_recv().map_err(|_| Error::Stopped)?
    }

    /// Serves an already connected socket (QMP `add_client`). With `skip_auth` no password
    /// is required.
    pub fn add_client(&self, stream: std::net::TcpStream, skip_auth: bool) -> Result<()> {
        self.send(Command::AddClient(stream, skip_auth))
    }

    /// Replaces the TLS identity for new connections.
    pub fn set_tls(&self, tls: TlsIdentity) -> Result<()> {
        self.send(Command::SetTls(tls))
    }

    /// Disconnects every client.
    pub fn disconnect_all(&self, reason: &str) -> Result<()> {
        self.send(Command::DisconnectAll(reason.to_owned()))
    }

    /// Stops the server and waits for its thread.
    pub fn shutdown(mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        let _ = self.commands.send(Command::Shutdown);
        if let Some(t) = self.thread.take()
            && t.join().is_err()
        {
            error!("RDP thread panicked");
        }
    }
}

impl Drop for RdpServer {
    fn drop(&mut self) {
        if self.thread.is_some() {
            warn!("RDP server dropped without shutdown");
            self.stop();
        }
    }
}
