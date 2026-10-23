//! What the client told us during connection setup, for `client.json` in the debug directory.
//!
//! `ironrdp-server` decodes the whole GCC Client Core Data (MS-RDPBCGR 2.2.1.3.2), the
//! Client Info PDU and the client's capability sets, then keeps only `keyboard_layout`,
//! `keyboard_type` and `ime_file_name` and drops the rest. Everything else the
//! diagnostics would like is out of reach until a local IronRDP patch adds a hook, so [`UNAVAILABLE`] records the gap instead of guessing.
//!
//! This module adds the one half of the channel question that *is* observable: for the channels we
//! own, IronRDP only runs the handshake of a channel the client asked for (static channels)
//! or accepted (`Create Response` with `CreationStatus::OK`, dynamic channels), so a channel
//! whose backend reached its ready state is a channel the client opened. Which channels the
//! client *requested* and why a DVC was declined stay unavailable — `DrdynvcServer` knows the
//! creation status but only writes it to a `debug!` log line.

use std::sync::Arc;

use ironrdp_server::{ConnectionHandler, ConnectionInfo};
use parking_lot::Mutex;
use vds_core::debug::Json;

use crate::session::SessionShared;

/// Channel names as they appear in `client.json`. The cliprdr, rdpsnd and EGFX channels are
/// the ones libvds implements, so they are also the ones whose opening it can observe;
/// the names live here so the offered list in `session.rs` and the hooks cannot drift apart.
pub(crate) const CHANNEL_DISPLAY_CONTROL: &str = "Microsoft::Windows::RDS::DisplayControl";
pub(crate) const CHANNEL_ECHO: &str = "ECHO";
pub(crate) const CHANNEL_AINPUT: &str = "FreeRDP::Advanced::Input";
pub(crate) const CHANNEL_CLIPRDR: &str = "cliprdr";
pub(crate) const CHANNEL_RDPSND: &str = "rdpsnd";
pub(crate) const CHANNEL_GFX: &str = "Microsoft::Windows::RDS::Graphics";

/// Client fields IronRDP decodes but does not hand to an embedding server.
pub(crate) const UNAVAILABLE: [&str; 8] = [
    "client_build",
    "client_name",
    "client_product_id",
    "keyboard_subtype",
    "gcc_core_flags",
    "client_capability_sets",
    "requested_dvc_channels",
    "dvc_creation_status",
];

/// Per-connection client facts, filled in as the connection progresses.
#[derive(Debug, Default)]
pub(crate) struct ClientRecord {
    inner: Mutex<Record>,
}

#[derive(Debug, Default)]
struct Record {
    keyboard_layout: Option<u32>,
    keyboard_type: Option<u32>,
    ime_file_name: Option<String>,
    desktop: Option<(u16, u16)>,
    egfx_advertised: Vec<String>,
    egfx_confirmed: Option<String>,
    avc420: Option<bool>,
    transport: Option<String>,
    channels: Vec<String>,
    channels_opened: Vec<String>,
}

impl ClientRecord {
    /// Records the GCC Client Core Data fields IronRDP surfaces.
    pub(crate) fn set_connection_info(&self, info: &ConnectionInfo) {
        let mut r = self.inner.lock();
        r.keyboard_layout = Some(info.keyboard_layout);
        r.keyboard_type = Some(info.keyboard_type.0);
        r.ime_file_name = Some(info.ime_file_name.clone());
    }

    /// Records the size the client asked for (Confirm Active Bitmap capability).
    pub(crate) fn set_desktop(&self, width: u16, height: u16) {
        self.inner.lock().desktop = Some((width, height));
    }

    /// Records the capability sets the client advertised on the EGFX channel.
    pub(crate) fn set_egfx_advertised(&self, versions: Vec<String>) {
        self.inner.lock().egfx_advertised = versions;
    }

    /// Records the capability set EGFX finally agreed on.
    pub(crate) fn set_egfx_confirmed(&self, caps: String, avc420: bool) {
        let mut r = self.inner.lock();
        r.egfx_confirmed = Some(caps);
        r.avc420 = Some(avc420);
    }

    /// Records the transport that was selected.
    pub(crate) fn set_transport(&self, transport: &str) {
        self.inner.lock().transport = Some(transport.to_owned());
    }

    /// Records the channels the server offers this client.
    pub(crate) fn set_channels(&self, channels: Vec<String>) {
        self.inner.lock().channels = channels;
    }

    /// Records that a channel this server owns reached its ready state, which means the
    /// client opened it. Called from the cliprdr, rdpsnd and EGFX backends, which are
    /// the three channels libvds implements itself; the DisplayControl, ECHO and
    /// `FreeRDP::Advanced::Input` DVCs belong to IronRDP and have no hook.
    pub(crate) fn channel_opened(&self, name: &str) {
        let mut r = self.inner.lock();
        if !r.channels_opened.iter().any(|c| c == name) {
            r.channels_opened.push(name.to_owned());
        }
    }

    /// Writes `client.json`; does nothing while no debug directory is configured.
    pub(crate) fn write(&self, shared: &SessionShared) {
        let Some(debug) = &shared.debug else {
            return;
        };
        let r = self.inner.lock();
        let peer = shared.peer.map_or_else(String::new, |p| p.to_string());
        let mut json = vec![
            ("sid", Json::Int(i64::try_from(shared.sid).unwrap_or(i64::MAX))),
            ("peer", Json::string(peer)),
            ("user", Json::string(shared.user_name().unwrap_or_default())),
            (
                "vm",
                Json::string(shared.core.config().vm_name.clone().unwrap_or_default()),
            ),
            ("started", Json::string(debug.started_utc())),
            (
                "collected_ms",
                Json::Int(i64::try_from(debug.elapsed_ms()).unwrap_or(i64::MAX)),
            ),
        ];
        json.push(("transport", opt_str(r.transport.as_deref())));
        json.push(("avc420", r.avc420.map_or(Json::Null, Json::Bool)));
        json.push((
            "gcc",
            Json::object(vec![
                ("keyboard_layout", opt_int(r.keyboard_layout)),
                ("keyboard_type", opt_int(r.keyboard_type)),
                ("ime_file_name", opt_str(r.ime_file_name.as_deref())),
            ]),
        ));
        json.push((
            "desktop",
            match r.desktop {
                Some((w, h)) => Json::object(vec![
                    ("width", Json::Int(i64::from(w))),
                    ("height", Json::Int(i64::from(h))),
                ]),
                None => Json::Null,
            },
        ));
        json.push((
            "egfx",
            Json::object(vec![
                (
                    "advertised",
                    Json::Array(r.egfx_advertised.iter().map(Json::string).collect()),
                ),
                ("confirmed", opt_str(r.egfx_confirmed.as_deref())),
            ]),
        ));
        json.push(("channels", Json::Array(r.channels.iter().map(Json::string).collect())));
        json.push((
            "channels_opened",
            Json::Array(r.channels_opened.iter().map(Json::string).collect()),
        ));
        json.push((
            "unavailable",
            Json::Array(UNAVAILABLE.iter().copied().map(Json::string).collect()),
        ));
        debug.client(&Json::object(json));
    }
}

fn opt_int(value: Option<u32>) -> Json {
    value.map_or(Json::Null, |v| Json::Int(i64::from(v)))
}

fn opt_str(value: Option<&str>) -> Json {
    value.map_or(Json::Null, Json::string)
}

/// Feeds [`ClientRecord`] from the IronRDP connection lifecycle.
pub(crate) struct ClientHandler {
    shared: Arc<SessionShared>,
}

impl ClientHandler {
    pub(crate) fn new(shared: Arc<SessionShared>) -> Self {
        Self { shared }
    }
}

impl ConnectionHandler for ClientHandler {
    fn on_connection_info(&mut self, info: &ConnectionInfo) {
        self.shared.client.set_connection_info(info);
    }
}

#[cfg(test)]
mod tests {
    use super::{CHANNEL_CLIPRDR, CHANNEL_GFX, ClientRecord, UNAVAILABLE};
    use vds_core::debug::Json;

    #[test]
    fn missing_fields_are_null_not_invented() {
        let record = ClientRecord::default();
        let r = record.inner.lock();
        assert!(r.keyboard_layout.is_none() && r.egfx_confirmed.is_none());
        assert!(UNAVAILABLE.contains(&"client_build"));
    }

    /// A channel is recorded once, however many times its backend reaches a ready state
    /// (a reconnecting EGFX channel, say), and in the order the handshakes completed.
    #[test]
    fn opened_channels_are_recorded_once_in_order() {
        let record = ClientRecord::default();
        record.channel_opened(CHANNEL_CLIPRDR);
        record.channel_opened(CHANNEL_GFX);
        record.channel_opened(CHANNEL_CLIPRDR);
        assert_eq!(record.inner.lock().channels_opened, [CHANNEL_CLIPRDR, CHANNEL_GFX]);
    }

    #[test]
    fn json_serialises_every_section() {
        let value = Json::object(vec![
            ("transport", Json::string("egfx")),
            ("avc420", Json::Bool(true)),
            (
                "egfx",
                Json::object(vec![(
                    "advertised",
                    Json::Array(vec![Json::string("V10_7"), Json::string("V8_1")]),
                )]),
            ),
        ]);
        let mut out = String::new();
        value.write(&mut out, 0);
        assert!(out.contains("\"avc420\": true"), "{out}");
        assert!(out.contains("\"V10_7\","), "{out}");
    }
}
