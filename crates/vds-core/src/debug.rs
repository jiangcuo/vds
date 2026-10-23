//! `debug-dir`: per-session diagnostic files (`docs/spec/logging.md`, section 5).
//!
//! When a debug directory is configured (`-rdp debug-dir=DIR`, or `vds_server_set_debug()`),
//! every connection gets its own directory under `<dir>/<vm>-<pid>/session-<sid>-<time>/`
//! holding a session log, a client description and CSV traces of frames, pointer updates,
//! input events and audio.
//!
//! Nothing here runs while no directory is configured: [`enabled`] is a single relaxed atomic
//! load and the callers keep their session handle in an `Option`, so the hot paths neither
//! allocate nor format anything with debugging off.

use std::collections::{BTreeMap, VecDeque};
use std::fmt::Write as _;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write as IoWrite};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;

use crate::error::{Error, Result};

/// Log filter of `session.log`, independent of the stderr filter.
pub const LOG_FILTER: &str = "vds_core=debug,vds_rdp=debug,ironrdp_server=debug,ironrdp_egfx=debug";

/// Rotate a `video-head<N>.h264` file once it reaches this size.
const VIDEO_ROTATE_BYTES: u64 = 200 * 1024 * 1024;

/// A frame left unacknowledged for this long is written to `frames.csv` without an ack delay.
const ACK_STALE: Duration = Duration::from_secs(2);

/// Value of `VdsConfig::debug_flags` (and `vds_server_set_debug()`) enabling `input.csv`.
pub const FLAG_INPUT: u32 = 1;
/// Value of `debug_flags` enabling `video-head<N>.h264`.
pub const FLAG_VIDEO: u32 = 2;
/// Value of `debug_flags` enabling `client-pdus.bin`.
pub const FLAG_PDU: u32 = 4;

/// Optional traces of the debug directory.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DebugFlags {
    /// Write `input.csv`: client keyboard and mouse events, keys redacted.
    pub input: bool,
    /// Write `video-head<N>.h264`: the raw Annex-B video stream.
    pub video: bool,
    /// Write `client-pdus.bin`: decrypted client PDUs (not implemented yet).
    pub pdu: bool,
}

impl DebugFlags {
    /// Reads the `VDS_DEBUG_*` bit set.
    pub const fn from_bits(bits: u32) -> Self {
        Self {
            input: bits & FLAG_INPUT != 0,
            video: bits & FLAG_VIDEO != 0,
            pdu: bits & FLAG_PDU != 0,
        }
    }

    /// The `VDS_DEBUG_*` bit set.
    pub const fn bits(self) -> u32 {
        (if self.input { FLAG_INPUT } else { 0 })
            | (if self.video { FLAG_VIDEO } else { 0 })
            | (if self.pdu { FLAG_PDU } else { 0 })
    }
}

/// Directory of the debug files (`None` until configured).
static DIR: OnceLock<PathBuf> = OnceLock::new();
/// `<vm>-<pid>`, the per-VM directory holding the session directories.
static VM_PREFIX: OnceLock<String> = OnceLock::new();
static INPUT: AtomicBool = AtomicBool::new(false);
static VIDEO: AtomicBool = AtomicBool::new(false);
static PDU: AtomicBool = AtomicBool::new(false);
/// Open sessions, looked up by the `sid` field of the current span.
///
/// The map holds [`Weak`] references on purpose: the session owns the files, and keeping it
/// alive from here would mean its `Drop` (footer, flush) never runs.
static SESSIONS: Mutex<BTreeMap<u64, Weak<SessionDebug>>> = parking_lot::const_mutex(BTreeMap::new());

/// Configures the debug directory and the initial flags.
///
/// The first call with a directory wins: a process runs a single RDP server, and the families
/// of files (`client.json` and friends) are keyed by it. Later calls only update the flags, so
/// the QMP command can toggle them at runtime.
pub fn configure(dir: Option<&str>, flags: DebugFlags, vm_name: Option<&str>) -> Result<()> {
    if let Some(dir) = dir.filter(|d| !d.is_empty()) {
        if DIR.get().is_none() {
            let path = PathBuf::from(dir);
            std::fs::create_dir_all(&path)
                .map_err(|e| Error::InvalidConfig(format!("cannot create debug directory {}: {e}", path.display())))?;
            // SAFETY of the ordering: only the first caller reaches this, and the value is
            // published before any session can look it up.
            let _ = DIR.set(path);
            let _ = VM_PREFIX.set(process_dir_name(vm_name));
        }
        set_flags(flags);
    }
    Ok(())
}

/// Changes the optional traces at runtime (QMP `rdp-set-debug`).
pub fn set_flags(flags: DebugFlags) {
    INPUT.store(flags.input, Ordering::Release);
    VIDEO.store(flags.video, Ordering::Release);
    PDU.store(flags.pdu, Ordering::Release);
}

/// The configured flags.
pub fn flags() -> DebugFlags {
    DebugFlags {
        input: INPUT.load(Ordering::Acquire),
        video: VIDEO.load(Ordering::Acquire),
        pdu: PDU.load(Ordering::Acquire),
    }
}

/// Whether a debug directory is configured. One relaxed load, safe on hot paths.
pub fn enabled() -> bool {
    DIR.get().is_some()
}

/// Whether `input.csv` is being written.
pub fn input_enabled() -> bool {
    enabled() && INPUT.load(Ordering::Acquire)
}

/// Whether `video-head<N>.h264` is being written.
pub fn video_enabled() -> bool {
    enabled() && VIDEO.load(Ordering::Acquire)
}

/// The debug directory, if configured.
pub fn directory() -> Option<&'static Path> {
    DIR.get().map(PathBuf::as_path)
}

/// Opens the debug files of session `sid`; `None` when debugging is off.
///
/// The returned handle unregisters itself when it is dropped.
pub fn session(sid: u64) -> Option<Arc<SessionDebug>> {
    let dir = DIR.get()?;
    match SessionDebug::create(dir, VM_PREFIX.get().map_or("vm", String::as_str), sid) {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::error!(error = %e, "cannot create the session debug directory");
            None
        }
    }
}

/// `<vm>-<pid>`, with anything that is not a plain file name character replaced.
fn process_dir_name(vm: Option<&str>) -> String {
    let name = vm.unwrap_or("vm");
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    format!("{cleaned}-{}", std::process::id())
}

/// The diagnostic files of one connection.
///
/// Every method is a no-op if the file could not be created (a full disk must not break the
/// session), and none of them return errors.
#[derive(Debug)]
pub struct SessionDebug {
    sid: u64,
    dir: PathBuf,
    started: Instant,
    started_wall: SystemTime,
    log: Mutex<BufWriter<File>>,
    frames: Mutex<BufWriter<File>>,
    pointer: Mutex<BufWriter<File>>,
    input: Mutex<BufWriter<File>>,
    audio: Mutex<BufWriter<File>>,
    pending_frames: Mutex<VecDeque<PendingFrame>>,
    video: Mutex<Vec<VideoFile>>,
    finished: AtomicBool,
}

#[derive(Debug)]
struct PendingFrame {
    sent_at: Instant,
    row: FrameRow,
    ack_us: Option<u64>,
}

#[derive(Debug)]
struct VideoFile {
    head: u32,
    file: BufWriter<File>,
    bytes: u64,
}

/// One encoded video frame (`frames.csv`).
#[derive(Debug, Clone, Copy)]
pub struct FrameRow {
    /// Display head.
    pub head: u32,
    /// EGFX frame id.
    pub frame_id: u32,
    /// `idr`, `p`, `refine` or `clear` (lossless tiles alone, no H.264).
    ///
    /// A mixed frame keeps the `idr`/`p`/`refine` of its H.264 half; `clear` means the frame
    /// carried tiles only. Either way `still_tiles` says how many lossless tiles went with it.
    pub kind: &'static str,
    /// Encoded bytes handed to the client: H.264 and lossless tiles together.
    pub bytes: usize,
    /// Average quantizer of the H.264 half; 0 when the frame carried none.
    pub qp: u8,
    /// Number of rectangles the client is told to copy into its copy of the H.264 picture.
    pub regions: usize,
    /// Total area of those rectangles, in pixels.
    pub region_area: u64,
    /// Age of the picture when encoding started, microseconds: the host had submitted this
    /// damage that long before, through the refresh interval and the video thread's wake-up.
    pub age_us: u64,
    /// Colour conversion time, microseconds.
    pub convert_us: u64,
    /// Encoding time, microseconds: the H.264 encoder and the lossless tiles together.
    pub encode_us: u64,
    /// Hand-off time, microseconds: from the end of the encode to the moment the EGFX PDUs
    /// entered the session's event loop (which owns the socket).
    pub handoff_us: u64,
    /// Frames in flight once this one was queued.
    pub in_flight: u32,
    /// Lossless tiles the client received with this frame.
    pub still_tiles: usize,
    /// Bytes those tiles took.
    pub still_bytes: usize,
}

/// One cursor update (`pointer.csv`).
#[derive(Debug, Clone, Copy)]
pub struct PointerRow {
    /// `shape` (a real cursor) or `transparent` (none: the guest draws it).
    pub kind: &'static str,
    /// Cursor width in pixels.
    pub width: u16,
    /// Cursor height in pixels.
    pub height: u16,
    /// Hot spot X.
    pub hot_x: u16,
    /// Hot spot Y.
    pub hot_y: u16,
    /// PDU sent: `color` or `rgba`.
    pub pdu: &'static str,
    /// Length of the AND mask.
    pub and_mask_len: usize,
    /// Whether the client's declared pointer limits discard this update; `None` if unknown.
    pub dropped: Option<bool>,
}

/// One client input event (`input.csv`).
///
/// Character keys carry no scancode: `kind` is `char` and `scancode` is `None`.
#[derive(Debug, Clone, Copy)]
pub struct InputRow {
    /// Event kind: a key name, `char`, `move`, `rel`, `button`, `wheel` or `lock-sync`.
    pub kind: &'static str,
    /// `fast-path` or `ainput` for mouse events, empty for keyboard events.
    pub source: &'static str,
    /// Scancode of a non-character key.
    pub scancode: Option<u8>,
    /// Scancode had the 0xE0 prefix.
    pub extended: Option<bool>,
    /// Key or button pressed.
    pub down: Option<bool>,
    /// Mouse button index, wheel notches, or the lock-key bit set.
    pub value: Option<i64>,
}

impl SessionDebug {
    fn create(root: &Path, vm: &str, sid: u64) -> Result<Arc<Self>> {
        let vm_dir = root.join(vm);
        std::fs::create_dir_all(&vm_dir)?;
        let dir = unique_dir(&vm_dir, sid)?;
        let file = |name: &str| -> std::io::Result<BufWriter<File>> {
            Ok(BufWriter::new(
                OpenOptions::new().create(true).append(true).open(dir.join(name))?,
            ))
        };
        let session = Arc::new(Self {
            sid,
            dir: dir.clone(),
            started: Instant::now(),
            started_wall: SystemTime::now(),
            log: Mutex::new(file("session.log")?),
            frames: Mutex::new(file("frames.csv")?),
            pointer: Mutex::new(file("pointer.csv")?),
            input: Mutex::new(file("input.csv")?),
            audio: Mutex::new(file("audio.csv")?),
            pending_frames: Mutex::new(VecDeque::new()),
            video: Mutex::new(Vec::new()),
            finished: AtomicBool::new(false),
        });
        session.write_line(&format!(
            "# session {sid} started {}",
            utc_timestamp(session.started_wall)
        ));
        session.record(
            &session.frames,
            format_args!(
                "time_ms,head,frame_id,kind,bytes,qp,regions,region_area,age_us,convert_us,encode_us,handoff_us,in_flight,ack_us,still_tiles,still_bytes"
            ),
        );
        session.record(
            &session.pointer,
            format_args!("time_ms,kind,width,height,hot_x,hot_y,pdu,and_mask_len,dropped"),
        );
        session.record(
            &session.input,
            format_args!("time_ms,kind,source,scancode,extended,down,value"),
        );
        session.record(
            &session.audio,
            format_args!("time_ms,kind,block,samples,bytes,left,right,mute,client_ts,wave_ts"),
        );
        SESSIONS.lock().insert(sid, Arc::downgrade(&session));
        tracing::debug!(sid, dir = %dir.display(), "session debug directory created");
        Ok(session)
    }

    /// Milliseconds since this session started (the time base of every CSV).
    pub fn elapsed_ms(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// UTC time of the first log line, `YYYY-MM-DDTHH:MM:SS.ffffffZ`.
    pub fn started_utc(&self) -> String {
        utc_timestamp(self.started_wall)
    }

    /// Directory holding the files of this session.
    pub fn directory(&self) -> &Path {
        &self.dir
    }

    fn write_line(&self, line: &str) {
        let mut log = self.log.lock();
        let _ = log.write_all(line.as_bytes());
        let _ = log.write_all(b"\n");
        let _ = log.flush();
    }

    /// Appends one CSV record and pushes it out of the buffer.
    ///
    /// Nothing is left buffered: the files must be usable while the session is still running,
    /// and a session killed without a clean shutdown must not lose what it already recorded.
    fn record(&self, file: &Mutex<BufWriter<File>>, line: std::fmt::Arguments<'_>) {
        let mut file = file.lock();
        let _ = file.write_fmt(line);
        let _ = file.write_all(b"\n");
        let _ = file.flush();
    }

    /// Records a sent frame; the row is written once its acknowledgement arrives (or after
    /// [`ACK_STALE`]).
    pub fn frame_sent(&self, row: FrameRow) {
        let now = Instant::now();
        let mut pending = self.pending_frames.lock();
        pending.push_back(PendingFrame {
            sent_at: now,
            row,
            ack_us: None,
        });
        let stale = pending
            .iter()
            .take_while(|p| now.duration_since(p.sent_at) > ACK_STALE)
            .count();
        self.flush_frames(&mut pending, stale);
    }

    /// Records a frame acknowledgement: every row up to that frame is written with the delay.
    pub fn frame_acked(&self, frame_id: u32) {
        let now = Instant::now();
        let mut pending = self.pending_frames.lock();
        let Some(position) = pending.iter().position(|p| p.row.frame_id == frame_id) else {
            return;
        };
        if let Some(p) = pending.get_mut(position) {
            p.ack_us = Some(u64::try_from(now.duration_since(p.sent_at).as_micros()).unwrap_or(u64::MAX));
        }
        self.flush_frames(&mut pending, position + 1);
    }

    fn flush_frames(&self, pending: &mut VecDeque<PendingFrame>, count: usize) {
        if count == 0 {
            return;
        }
        let now = self.elapsed_ms();
        for _ in 0..count {
            let Some(p) = pending.pop_front() else { break };
            self.record(
                &self.frames,
                format_args!(
                    "{now},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
                    p.row.head,
                    p.row.frame_id,
                    p.row.kind,
                    p.row.bytes,
                    p.row.qp,
                    p.row.regions,
                    p.row.region_area,
                    p.row.age_us,
                    p.row.convert_us,
                    p.row.encode_us,
                    p.row.handoff_us,
                    p.row.in_flight,
                    p.ack_us.map_or_else(String::new, |us| us.to_string()),
                    p.row.still_tiles,
                    p.row.still_bytes,
                ),
            );
        }
    }

    /// Records a cursor update.
    pub fn pointer(&self, row: PointerRow) {
        self.record(
            &self.pointer,
            format_args!(
                "{},{},{},{},{},{},{},{},{}",
                self.elapsed_ms(),
                row.kind,
                row.width,
                row.height,
                row.hot_x,
                row.hot_y,
                row.pdu,
                row.and_mask_len,
                row.dropped.map_or_else(String::new, |d| u8::from(d).to_string()),
            ),
        );
    }

    /// Records a client input event.
    pub fn input(&self, row: InputRow) {
        self.record(
            &self.input,
            format_args!(
                "{},{},{},{},{},{},{}",
                self.elapsed_ms(),
                row.kind,
                row.source,
                row.scancode.map_or_else(String::new, |s| s.to_string()),
                row.extended.map_or_else(String::new, |b| u8::from(b).to_string()),
                row.down.map_or_else(String::new, |b| u8::from(b).to_string()),
                row.value.map_or_else(String::new, |v| v.to_string()),
            ),
        );
    }

    /// Records a wave handed to the client.
    ///
    /// `ts` is the wave's own media timestamp (the moment it was captured, counted from the
    /// stream's samples). Read together with the `confirm` row carrying nearly the same
    /// `client_ts`, it gives the hold time the client added and, from the two `time_ms`
    /// values, how long the confirm took to come back.
    pub fn audio_wave(&self, samples: usize, bytes: usize, ts: u32) {
        self.record(
            &self.audio,
            format_args!("{},wave,,{samples},{bytes},,,,,{ts}", self.elapsed_ms()),
        );
    }

    /// Records a volume change.
    pub fn audio_volume(&self, left: u16, right: u16, mute: bool) {
        self.record(
            &self.audio,
            format_args!("{},volume,,,{left},{right},{},,,", self.elapsed_ms(), u8::from(mute)),
        );
    }

    /// Records a wave confirmation.
    ///
    /// `client_ts` is the timestamp the client echoed, i.e. the moment it finished playing
    /// the block plus the hold time it added, not a round-trip time (MS-RDPEA 2.2.3.4).
    pub fn audio_confirm(&self, block: u8, client_ts: u16) {
        self.record(
            &self.audio,
            format_args!("{},confirm,{block},,,,,,{client_ts},", self.elapsed_ms()),
        );
    }

    /// Appends the original Annex-B stream of `head` to its video file.
    pub fn video(&self, head: u32, data: &[u8]) {
        let mut files = self.video.lock();
        let index = match files.iter().position(|v| v.head == head) {
            Some(i) => i,
            None => {
                let name = format!("video-head{head}.h264");
                match OpenOptions::new().create(true).append(true).open(self.dir.join(&name)) {
                    Ok(f) => {
                        files.push(VideoFile {
                            head,
                            file: BufWriter::new(f),
                            bytes: 0,
                        });
                        files.len() - 1
                    }
                    Err(_) => return,
                }
            }
        };
        let Some(v) = files.get_mut(index) else { return };
        if v.bytes > VIDEO_ROTATE_BYTES {
            // Rotation drops the oldest frames: one bounded file per head.
            let name = format!("video-head{head}.h264");
            let Ok(f) = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(self.dir.join(name))
            else {
                return;
            };
            *v = VideoFile {
                head,
                file: BufWriter::new(f),
                bytes: 0,
            };
        }
        if v.file.write_all(data).is_ok() {
            v.bytes += data.len() as u64;
            // See `record`: the stream must be readable while the session runs.
            let _ = v.file.flush();
        }
    }

    /// (Re)writes `client.json`.
    pub fn client(&self, client: &Json) {
        let mut out = String::new();
        client.write(&mut out, 0);
        out.push('\n');
        let _ = std::fs::write(self.dir.join("client.json"), out);
    }

    /// Marks the session over: writes the footer and flushes what is left.
    ///
    /// Called when the connection ends, not when this object is dropped: the object is handed
    /// to IronRDP's task tree and can outlive the session by a long time. The footer is the
    /// marker saying "these files are complete", so it must not wait for a `Drop` that may only
    /// come when the server stops. [`Drop`] calls this too, as a backstop for sessions that
    /// never got this far (a failed handshake, a process that ends first).
    pub fn finish(&self) {
        if self.finished.swap(true, Ordering::AcqRel) {
            return;
        }
        self.write_line(&format!(
            "# session {} finished after {} ms",
            self.sid,
            self.elapsed_ms()
        ));
        let mut pending = self.pending_frames.lock();
        let count = pending.len();
        self.flush_frames(&mut pending, count);
    }
}

impl Drop for SessionDebug {
    fn drop(&mut self) {
        {
            // Our own strong count is already zero here; a live entry for the same `sid` would
            // be a newer session, which must not be unregistered by us.
            let mut sessions = SESSIONS.lock();
            if sessions.get(&self.sid).is_some_and(|w| w.strong_count() == 0) {
                sessions.remove(&self.sid);
            }
        }
        self.finish();
    }
}

/// Creates `<parent>/session-<sid>-<utc>`; a second `create` within the same second gets a
/// numeric suffix.
fn unique_dir(parent: &Path, sid: u64) -> Result<PathBuf> {
    let stamp = utc_compact(SystemTime::now());
    for attempt in 0..1000u32 {
        let name = if attempt == 0 {
            format!("session-{sid}-{stamp}")
        } else {
            format!("session-{sid}-{stamp}-{attempt}")
        };
        let path = parent.join(name);
        match std::fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(Error::Io(e)),
        }
    }
    Err(Error::InvalidConfig(String::from(
        "cannot create a session debug directory",
    )))
}

/// A minimal JSON tree, so the debug files need no serialization dependency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Json {
    /// `null`.
    Null,
    /// `true` or `false`.
    Bool(bool),
    /// An integer.
    Int(i64),
    /// A string (escaped on output).
    Str(String),
    /// An array.
    Array(Vec<Json>),
    /// An object; the key order is preserved.
    Object(Vec<(&'static str, Json)>),
}

impl Json {
    /// An object from a list of fields.
    pub fn object(fields: Vec<(&'static str, Json)>) -> Self {
        Self::Object(fields)
    }

    /// A string field.
    pub fn string(s: impl Into<String>) -> Self {
        Self::Str(s.into())
    }

    /// Appends the value to `out`, indenting nested values by `indent` levels of two spaces.
    pub fn write(&self, out: &mut String, indent: usize) {
        match self {
            Self::Null => out.push_str("null"),
            Self::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Self::Int(i) => {
                let _ = write!(out, "{i}");
            }
            Self::Str(s) => write_escaped(out, s),
            Self::Array(items) => {
                if items.is_empty() {
                    out.push_str("[]");
                    return;
                }
                out.push_str("[\n");
                for (i, item) in items.iter().enumerate() {
                    indent_by(out, indent + 1);
                    item.write(out, indent + 1);
                    if i + 1 < items.len() {
                        out.push(',');
                    }
                    out.push('\n');
                }
                indent_by(out, indent);
                out.push(']');
            }
            Self::Object(fields) => {
                if fields.is_empty() {
                    out.push_str("{}");
                    return;
                }
                out.push_str("{\n");
                for (i, (key, value)) in fields.iter().enumerate() {
                    indent_by(out, indent + 1);
                    write_escaped(out, key);
                    out.push_str(": ");
                    value.write(out, indent + 1);
                    if i + 1 < fields.len() {
                        out.push(',');
                    }
                    out.push('\n');
                }
                indent_by(out, indent);
                out.push('}');
            }
        }
    }
}

fn indent_by(out: &mut String, indent: usize) {
    for _ in 0..indent {
        out.push_str("  ");
    }
}

fn write_escaped(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// `YYYY-MM-DDTHH:MM:SS.ffffffZ`.
fn utc_timestamp(t: SystemTime) -> String {
    let (secs, micros) = match t.duration_since(UNIX_EPOCH) {
        Ok(d) => (d.as_secs() as i64, d.subsec_micros()),
        Err(e) => (-(e.duration().as_secs() as i64), 0),
    };
    let (year, month, day) = civil_from_days(secs.div_euclid(86_400));
    let day_secs = secs.rem_euclid(86_400);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{micros:06}Z",
        day_secs / 3600,
        (day_secs % 3600) / 60,
        day_secs % 60
    )
}

/// `YYYYMMDD-HHMMSS`, used in directory names.
fn utc_compact(t: SystemTime) -> String {
    let secs = match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(_) => 0,
    };
    let (year, month, day) = civil_from_days(secs.div_euclid(86_400));
    let day_secs = secs.rem_euclid(86_400);
    format!(
        "{year:04}{month:02}{day:02}-{:02}{:02}{:02}",
        day_secs / 3600,
        (day_secs % 3600) / 60,
        day_secs % 60
    )
}

/// Days since the Unix epoch to a civil date (Howard Hinnant's `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = u64::try_from(z - era * 146_097).unwrap_or(0);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = i64::try_from(yoe).unwrap_or(0) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = u32::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap_or(1);
    let month = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(1);
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// `sid` of the innermost span carrying one. Session spans are created as `session{sid}` or
/// `encoder{sid}`.
#[derive(Debug, Default)]
struct SidVisitor(Option<u64>);

impl Visit for SidVisitor {
    fn record_u64(&mut self, field: &Field, value: u64) {
        if field.name() == "sid" {
            self.0 = Some(value);
        }
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        if field.name() == "sid" {
            self.0 = u64::try_from(value).ok();
        }
    }

    fn record_debug(&mut self, _field: &Field, _value: &dyn core::fmt::Debug) {}
}

/// The `sid` recorded on a span, kept in its extensions so `on_event` can find it.
#[derive(Debug, Clone, Copy)]
struct SpanSid(u64);

/// `tracing` layer writing the events of an active session into its `session.log`.
///
/// The stderr layer keeps its own filter: enabling the debug directory never changes what is
/// printed, and the session files get `debug` even when the console stays at `info`.
#[derive(Debug, Default)]
pub struct SessionLogLayer;

impl SessionLogLayer {
    /// Creates the layer.
    pub fn new() -> Self {
        Self
    }
}

fn event_sid<S>(event: &Event<'_>, ctx: &Context<'_, S>) -> Option<u64>
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    let mut own = SidVisitor::default();
    event.record(&mut own);
    if own.0.is_some() {
        return own.0;
    }
    let scope = ctx.event_scope(event)?;
    for span in scope.from_root() {
        if let Some(sid) = span.extensions().get::<SpanSid>() {
            return Some(sid.0);
        }
    }
    None
}

/// Collects the rendered event: the `message` field plus every other field.
#[derive(Default)]
struct LineVisitor {
    message: String,
    fields: String,
}

impl Visit for LineVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn core::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
        } else {
            let _ = write!(self.fields, " {}={value:?}", field.name());
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = String::from(value);
        } else {
            let _ = write!(self.fields, " {}={value:?}", field.name());
        }
    }
}

impl<S> Layer<S> for SessionLogLayer
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        if !enabled() {
            return;
        }
        let Some(span) = ctx.span(id) else { return };
        let mut visitor = SidVisitor::default();
        attrs.record(&mut visitor);
        if let Some(sid) = visitor.0 {
            span.extensions_mut().insert(SpanSid(sid));
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        if !enabled() {
            return;
        }
        let Some(sid) = event_sid(event, &ctx) else { return };
        let Some(session) = SESSIONS.lock().get(&sid).and_then(Weak::upgrade) else {
            return;
        };
        let metadata = event.metadata();
        let mut visitor = LineVisitor::default();
        event.record(&mut visitor);
        session.write_line(&format!(
            "{} {} {}: {}{}",
            utc_timestamp(SystemTime::now()),
            metadata.level(),
            metadata.target(),
            visitor.message,
            visitor.fields
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_round_trip() {
        let f = DebugFlags::from_bits(FLAG_INPUT | FLAG_VIDEO);
        assert!(f.input && f.video && !f.pdu);
        assert_eq!(f.bits(), FLAG_INPUT | FLAG_VIDEO);
        assert_eq!(DebugFlags::default().bits(), 0);
    }

    #[test]
    fn timestamps_are_utc() {
        let epoch = UNIX_EPOCH + Duration::from_secs(1_760_000_000);
        assert_eq!(utc_timestamp(epoch), "2025-10-09T08:53:20.000000Z");
        assert_eq!(utc_compact(epoch), "20251009-085320");
    }

    #[test]
    fn json_escapes_and_nests() {
        let value = Json::object(vec![
            ("name", Json::string("a\"b\n")),
            ("count", Json::Int(3)),
            ("list", Json::Array(vec![Json::Bool(true), Json::Null])),
        ]);
        let mut out = String::new();
        value.write(&mut out, 0);
        assert!(out.contains("\"a\\\"b\\n\""), "{out}");
        assert!(out.contains("\"count\": 3"), "{out}");
        assert!(out.contains("[\n    true,\n    null\n  ]"), "{out}");
    }

    #[test]
    fn session_files_are_created_and_written() {
        let dir = std::env::temp_dir().join(format!("vds-debug-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let s = SessionDebug::create(&dir, "vm-1", 7).unwrap();
        s.pointer(PointerRow {
            kind: "transparent",
            width: 32,
            height: 32,
            hot_x: 0,
            hot_y: 0,
            pdu: "color",
            and_mask_len: 128,
            dropped: None,
        });
        s.input(InputRow {
            kind: "char",
            source: "",
            scancode: None,
            extended: None,
            down: Some(true),
            value: None,
        });
        s.frame_sent(FrameRow {
            head: 0,
            frame_id: 1,
            kind: "idr",
            bytes: 100,
            qp: 20,
            regions: 1,
            region_area: 16,
            age_us: 3,
            convert_us: 5,
            encode_us: 6,
            handoff_us: 2,
            in_flight: 1,
            still_tiles: 2,
            still_bytes: 40,
        });
        s.frame_acked(1);
        s.video(0, b"\x00\x00\x00\x01");
        s.client(&Json::object(vec![("sid", Json::Int(7))]));
        let path = s.directory().to_path_buf();
        drop(s);

        let pointer = std::fs::read_to_string(path.join("pointer.csv")).unwrap();
        assert!(pointer.contains("transparent,32,32,0,0,color,128,"), "{pointer}");
        let input = std::fs::read_to_string(path.join("input.csv")).unwrap();
        // A character key is recorded as `char` and carries neither a scancode nor the
        // extended flag (the file must not reveal which characters were typed).
        let row: Vec<&str> = input.lines().nth(1).unwrap().split(',').collect();
        assert_eq!(row[1], "char", "{input}");
        assert_eq!(row[3], "", "scancode leaked: {input}");
        assert_eq!(row[4], "", "extended flag leaked: {input}");
        assert_eq!(row[5], "1", "{input}");
        let frames = std::fs::read_to_string(path.join("frames.csv")).unwrap();
        let mut lines = frames.lines();
        assert_eq!(lines.next().unwrap().split(',').count(), 16, "{frames}");
        let row: Vec<&str> = lines.next().expect("one frame row").split(',').collect();
        assert_eq!(&row[1..=5], ["0", "1", "idr", "100", "20"], "{frames}");
        // The path is timed end to end, in the order it happens.
        assert_eq!(&row[8..=11], ["3", "5", "6", "2"], "{frames}");
        // The row is written on the acknowledgement, so that column holds a latency.
        assert!(row[13].parse::<u64>().is_ok(), "ack_us missing: {frames}");
        // The lossless half of the frame, after the H.264 columns.
        assert_eq!(&row[14..], ["2", "40"], "{frames}");
        assert!(lines.next().is_none(), "{frames}");
        assert!(path.join("client.json").exists());
        assert!(path.join("video-head0.h264").exists());
        let log = std::fs::read_to_string(path.join("session.log")).unwrap();
        assert!(log.contains("session 7 finished"), "{log}");
        let _ = std::fs::remove_dir_all(&path);
    }

    #[test]
    fn unmatched_ack_does_not_lose_rows() {
        let dir = std::env::temp_dir().join(format!("vds-debug-ack-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let s = SessionDebug::create(&dir, "vm", 1).unwrap();
        s.frame_sent(FrameRow {
            head: 0,
            frame_id: 4,
            kind: "p",
            bytes: 1,
            qp: 1,
            regions: 1,
            region_area: 1,
            age_us: 0,
            convert_us: 0,
            encode_us: 0,
            handoff_us: 0,
            in_flight: 1,
            still_tiles: 0,
            still_bytes: 0,
        });
        s.frame_acked(9);
        let path = s.directory().to_path_buf();
        drop(s);
        let frames = std::fs::read_to_string(path.join("frames.csv")).unwrap();
        let row = frames.lines().nth(1).expect("the unacknowledged row survives");
        assert_eq!(row.split(',').count(), 16, "{frames}");
        // The acknowledgement column stays empty; the lossless columns follow it.
        assert_eq!(row.split(',').nth(13), Some(""), "ack_us must stay empty: {frames}");
        assert!(row.ends_with(",0,0"), "the columns after it are still written: {frames}");
        let _ = std::fs::remove_dir_all(&path);
    }
}
