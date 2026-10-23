//! Clipboard sharing (MS-RDPECLIP) between the client and the host clipboard: text and PNG
//! images.
//!
//! Based on qemu-display (qemu-rdp/src/server/clipboard.rs), MIT License,
//! Copyright (c) Marc-André Lureau and contributors.
//!
//! Images travel as PNG on the host side. Towards the client they are offered as the registered
//! formats "PNG" and "image/png" and as `CF_DIB`; from the client a registered PNG format is
//! preferred and `CF_DIB` is converted. Conversions run on a short-lived thread so neither the
//! host main thread nor the session task blocks on a large image.
//!
//! Clipboard content is sensitive: only formats and sizes are logged.

use std::sync::Arc;

use ironrdp_cliprdr::backend::{ClipboardMessage, CliprdrBackend, CliprdrBackendFactory};
use ironrdp_cliprdr::pdu::{
    ClipboardFormat, ClipboardFormatId, ClipboardFormatName, ClipboardGeneralCapabilityFlags, FileContentsRequest,
    FileContentsResponse, FormatDataRequest, FormatDataResponse, LockDataId, OwnedFormatDataResponse,
};
use ironrdp_core::impl_as_any;
use ironrdp_server::{CliprdrServerFactory, ServerEvent, ServerEventSender};
use tokio::sync::mpsc;
use tracing::{debug, warn};
use vds_core::event::{ClientEvent, ClipboardType, HostClipboardEvent};

use crate::client_info::CHANNEL_CLIPRDR;
use crate::clipboard_image::{dib_to_png, png_to_dib};
use crate::session::SessionShared;

/// Server-side id of the registered "PNG" format (ids are local to the sender, MS-RDPECLIP 2.2.3.1).
const SERVER_PNG: ClipboardFormatId = ClipboardFormatId(0xC100);
/// Server-side id of the registered "image/png" format.
const SERVER_IMAGE_PNG: ClipboardFormatId = ClipboardFormatId(0xC101);
/// Registered format names that carry a PNG file.
const PNG_NAMES: [&str; 2] = ["PNG", "image/png"];

/// How clipboard data is encoded on the RDP side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wire {
    /// `CF_UNICODETEXT`.
    Text,
    /// A registered PNG format.
    Png,
    /// `CF_DIB`, converted from/to PNG.
    Dib,
}

impl Wire {
    fn ty(self) -> ClipboardType {
        match self {
            Wire::Text => ClipboardType::Text,
            Wire::Png | Wire::Dib => ClipboardType::Png,
        }
    }
}

/// Pending requests in either direction and the formats of the client clipboard.
#[derive(Debug, Default)]
pub(crate) struct ClipboardState {
    /// The client asked for host data in this encoding; the host has not answered yet.
    client_request: Option<Wire>,
    /// The host asked for client data in this encoding; the client has not answered yet.
    host_request: Option<Wire>,
    /// Client-side id of its registered PNG format, if it offers one.
    client_png: Option<ClipboardFormatId>,
    /// The client offers `CF_DIB`.
    client_dib: bool,
}

/// Decodes a `CF_UNICODETEXT` payload (UTF-16LE, NUL-terminated) into UTF-8.
fn utf16le_to_utf8(data: &[u8]) -> Vec<u8> {
    let units: Vec<u16> = data
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|u| *u != 0)
        .collect();
    String::from_utf16_lossy(&units).into_bytes()
}

/// Runs `f` on its own thread; image conversions can take tens of milliseconds.
fn spawn_conversion(f: impl FnOnce() + Send + 'static) {
    if let Err(e) = std::thread::Builder::new().name("vds-clipboard".into()).spawn(f) {
        warn!(error = %e, "failed to start the clipboard conversion thread");
    }
}

/// Handles a host clipboard event for this session.
pub(crate) fn on_host_event(shared: &SessionShared, event: HostClipboardEvent) {
    let msg = match event {
        HostClipboardEvent::Grab(types) => {
            let mut formats = Vec::new();
            for t in types {
                match t {
                    ClipboardType::Text => formats.push(ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT)),
                    ClipboardType::Png => {
                        formats
                            .push(ClipboardFormat::new(SERVER_PNG).with_name(ClipboardFormatName::new_static("PNG")));
                        formats.push(
                            ClipboardFormat::new(SERVER_IMAGE_PNG)
                                .with_name(ClipboardFormatName::new_static("image/png")),
                        );
                        formats.push(ClipboardFormat::new(ClipboardFormatId::CF_DIB));
                    }
                }
            }
            ClipboardMessage::SendInitiateCopy(formats)
        }
        HostClipboardEvent::Release => ClipboardMessage::SendInitiateCopy(Vec::new()),
        HostClipboardEvent::Request(ty) => {
            let mut state = shared.clipboard.lock();
            let (wire, format) = match ty {
                ClipboardType::Text => (Wire::Text, ClipboardFormatId::CF_UNICODETEXT),
                ClipboardType::Png => match (state.client_png, state.client_dib) {
                    (Some(id), _) => (Wire::Png, id),
                    (None, true) => (Wire::Dib, ClipboardFormatId::CF_DIB),
                    (None, false) => {
                        debug!("host asked for an image the client does not offer");
                        return;
                    }
                },
            };
            state.host_request = Some(wire);
            ClipboardMessage::SendInitiatePaste(format)
        }
        HostClipboardEvent::Data(ty, data) => {
            let mut state = shared.clipboard.lock();
            let wire = match state.client_request {
                Some(w) if w.ty() == ty => w,
                _ => {
                    debug!("dropping unrequested host clipboard data");
                    return;
                }
            };
            state.client_request = None;
            drop(state);
            debug!(bytes = data.len(), ?wire, "sending clipboard data to the client");
            match wire {
                Wire::Text => {
                    let text = String::from_utf8_lossy(&data);
                    ClipboardMessage::SendFormatData(OwnedFormatDataResponse::new_unicode_string(&text))
                }
                Wire::Png => ClipboardMessage::SendFormatData(OwnedFormatDataResponse::new_data(data)),
                Wire::Dib => {
                    let Some(tx) = shared.event_sender() else {
                        return;
                    };
                    spawn_conversion(move || {
                        let response = match png_to_dib(&data) {
                            Ok(dib) => OwnedFormatDataResponse::new_data(dib),
                            Err(e) => {
                                warn!(error = %e, "cannot convert the host image for the client");
                                OwnedFormatDataResponse::new_error()
                            }
                        };
                        let _ = tx.send(ServerEvent::Clipboard(ClipboardMessage::SendFormatData(response)));
                    });
                    return;
                }
            }
        }
    };
    let _ = shared.send_event(ServerEvent::Clipboard(msg));
}

pub(crate) struct ClipboardFactory {
    shared: Arc<SessionShared>,
}

impl ClipboardFactory {
    pub(crate) fn new(shared: Arc<SessionShared>) -> Self {
        Self { shared }
    }
}

impl ServerEventSender for ClipboardFactory {
    fn set_sender(&mut self, sender: mpsc::UnboundedSender<ServerEvent>) {
        self.shared.set_event_sender(sender);
    }
}

impl CliprdrBackendFactory for ClipboardFactory {
    fn build_cliprdr_backend(&self) -> Box<dyn CliprdrBackend> {
        Box::new(Backend {
            shared: Arc::clone(&self.shared),
        })
    }
}

impl CliprdrServerFactory for ClipboardFactory {}

struct Backend {
    shared: Arc<SessionShared>,
}

impl core::fmt::Debug for Backend {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Backend").field("sid", &self.shared.sid).finish()
    }
}

impl_as_any!(Backend);

impl CliprdrBackend for Backend {
    fn temporary_directory(&self) -> &str {
        ".cliprdr"
    }

    fn client_capabilities(&self) -> ClipboardGeneralCapabilityFlags {
        ClipboardGeneralCapabilityFlags::empty()
    }

    fn on_ready(&mut self) {
        // The cliprdr handshake only runs for a client that asked for the channel, so getting
        // here is proof it opened one.
        self.shared.client.channel_opened(CHANNEL_CLIPRDR);
        self.shared.client.write(&self.shared);
    }

    fn on_request_format_list(&mut self) {}

    fn on_process_negotiated_capabilities(&mut self, capabilities: ClipboardGeneralCapabilityFlags) {
        debug!(?capabilities, "clipboard capabilities negotiated");
    }

    fn on_remote_copy(&mut self, available_formats: &[ClipboardFormat]) {
        let text = available_formats
            .iter()
            .any(|f| f.id() == ClipboardFormatId::CF_UNICODETEXT);
        let png = available_formats
            .iter()
            .find(|f| f.name().is_some_and(|n| PNG_NAMES.contains(&n.value())))
            .map(ClipboardFormat::id);
        let dib = available_formats.iter().any(|f| f.id() == ClipboardFormatId::CF_DIB);
        debug!(
            formats = available_formats.len(),
            text,
            png = png.is_some(),
            dib,
            "client clipboard changed"
        );
        {
            let mut state = self.shared.clipboard.lock();
            state.client_png = png;
            state.client_dib = dib;
        }
        let mut types = Vec::new();
        if text {
            types.push(ClipboardType::Text);
        }
        if png.is_some() || dib {
            types.push(ClipboardType::Png);
        }
        let ev = if types.is_empty() {
            ClientEvent::ClipboardRelease
        } else {
            ClientEvent::ClipboardGrab(types)
        };
        self.shared.core.push_event(ev);
    }

    fn on_format_data_request(&mut self, request: FormatDataRequest) {
        let wire = match request.format {
            ClipboardFormatId::CF_UNICODETEXT => Some(Wire::Text),
            SERVER_PNG | SERVER_IMAGE_PNG => Some(Wire::Png),
            ClipboardFormatId::CF_DIB => Some(Wire::Dib),
            _ => None,
        };
        if let Some(wire) = wire {
            self.shared.clipboard.lock().client_request = Some(wire);
            self.shared.core.push_event(ClientEvent::ClipboardRequest(wire.ty()));
        } else {
            debug!(
                format = request.format.value(),
                "unsupported clipboard format requested"
            );
            let _ = self
                .shared
                .send_event(ServerEvent::Clipboard(ClipboardMessage::SendFormatData(
                    OwnedFormatDataResponse::new_error(),
                )));
        }
    }

    fn on_format_data_response(&mut self, response: FormatDataResponse<'_>) {
        let Some(wire) = self.shared.clipboard.lock().host_request.take() else {
            return;
        };
        if response.is_error() {
            debug!("client refused the clipboard request");
            return;
        }
        debug!(
            bytes = response.data().len(),
            ?wire,
            "received clipboard data from the client"
        );
        match wire {
            Wire::Text => {
                let data = utf16le_to_utf8(response.data());
                self.shared
                    .core
                    .push_event(ClientEvent::ClipboardData(ClipboardType::Text, data));
            }
            Wire::Png => {
                let data = response.data().to_vec();
                self.shared
                    .core
                    .push_event(ClientEvent::ClipboardData(ClipboardType::Png, data));
            }
            Wire::Dib => {
                let dib = response.data().to_vec();
                let shared = Arc::clone(&self.shared);
                spawn_conversion(move || match dib_to_png(&dib) {
                    Ok(png) => shared
                        .core
                        .push_event(ClientEvent::ClipboardData(ClipboardType::Png, png)),
                    Err(e) => warn!(error = %e, "cannot convert the client image for the host"),
                });
            }
        }
    }

    fn on_file_contents_request(&mut self, _request: FileContentsRequest) {}

    fn on_file_contents_response(&mut self, _response: FileContentsResponse<'_>) {}

    fn on_lock(&mut self, _data_id: LockDataId) {}

    fn on_unlock(&mut self, _data_id: LockDataId) {}
}

#[cfg(test)]
mod tests {
    use super::utf16le_to_utf8;

    #[test]
    fn decodes_nul_terminated_utf16() {
        let mut data: Vec<u8> = "héllo".encode_utf16().flat_map(u16::to_le_bytes).collect();
        data.extend_from_slice(&[0, 0, 0x41, 0]);
        assert_eq!(utf16le_to_utf8(&data), "héllo".as_bytes());
    }
}
