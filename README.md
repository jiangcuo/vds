# libvds

Virtual Desktop Server library: native RDP (and future protocols) for our QEMU fork.
QEMU links `libvds` in-process through the C ABI in `include/vds.h` and exposes it as `-rdp`.

- Rules for contributors and AI agents, locked decisions: [AGENTS.md](AGENTS.md)
- Specifications: [docs/spec/](docs/spec/) (C ABI, QEMU style, Rust style, logging)

License: MIT OR Apache-2.0.

## Features

- Graphics: EGFX AVC420 with openh264 (quality rate control) or optional VA-API (`vaapi`
  feature, Linux), RemoteFX and plain bitmaps as fallbacks; optional lossless ClearCodec tiles
  beside the video (`egfx-mixed`). Shareable QEMU surfaces are read in place
  (`vds_head_attach_shared`), not copied on the main thread.
- Cursor: hardware cursor shapes are sent to the client, which draws the pointer locally.
- Input: keyboard (scancodes), absolute/relative mouse, wheel (fast-path and ainput).
- Audio playback over rdpsnd, from `-audiodev rdp` or captured from any other audiodev.
- Clipboard: text and PNG images (RDP side: registered "PNG"/"image/png" and `CF_DIB`).
- Resolution follows the client (DisplayControl, `resize-on-connect`).
- Web clients: IronRDP web through `tools/vds-rdcleanpath` (RDCleanPath bridge).

## Architecture

```
 QEMU fork (C, GPL)                         libvds (Rust, MIT OR Apache-2.0)
┌──────────────────────────────┐  vds.h  ┌──────────────────────────────────────────────┐
│ ui/rdp-core.c    options, QMP│ ──────► │ vds-capi   C ABI, ffi_guard, config parsing   │
│ ui/rdp-display.c DCL, cursor │         │    │                                          │
│ ui/rdp-input.c   input inject│ ◄────── │ vds-core   Core: heads, cursor, events,       │
│ ui/rdp-clipboard.c  clipboard│ callbacks│           auth, config, debug-dir, logging  │
│ ui/rdp-audio.c / rdpaudio.c  │         │    │                                          │
└──────────────────────────────┘         │ vds-rdp    IronRDP server: sessions, EGFX,   │
        main thread + BQL                │            legacy display, input, sound,     │
                                         │            clipboard, TLS                    │
                                         │    │                                          │
                                         │ vds-encode openh264 / VA-API, BGRX→I420      │
                                         └──────────────────────────────────────────────┘
```

**Boundary.** QEMU only includes `vds.h`. It hands libvds CPU pixels (BGRX), cursor shapes,
PCM and clipboard data, and receives client input, layout requests and clipboard events
through callbacks. Callbacks run only inside `vds_server_dispatch()`, which QEMU calls from a
bottom half on its main thread; libvds asks for that with the `[any-thread]` `wakeup`
callback. Everything else in libvds runs on its own threads.

**Crates** (each has an `AGENTS.md` with its design and constraints):

| crate | role |
|---|---|
| `crates/vds-capi` | the C ABI: `extern "C"` functions, plain C types, panic guard; generates `include/vds.h` |
| `crates/vds-core` | protocol-independent state shared by host and protocol: display heads with damage, cursor, client/host events, credentials, configuration, debug directory, logging |
| `crates/vds-rdp` | the RDP backend on IronRDP: listener and sessions, codec selection, EGFX (AVC420 + ClearCodec), legacy RemoteFX/bitmap, input, rdpsnd, cliprdr, DisplayControl, TLS |
| `crates/vds-encode` | video encoding: openh264 (default), FFmpeg `h264_vaapi` (optional), BGRX→I420 conversion of damaged areas |
| `crates/vds-test-host` | a synthetic host (animated pattern) standing in for QEMU in tests and tools |
| `tools/vds-demo` | standalone RDP server on the synthetic host |
| `tools/vds-bench` | encoder and colour-conversion benchmark |
| `tools/vds-rdcleanpath` | RDCleanPath bridge for the IronRDP web client |
| `xtask` | header generation/check, install, C smoke test |

**Threads.**

- QEMU main thread: submits damage (`vds_head_damage` for shared surfaces, `vds_head_submit`
  otherwise), cursor, audio, clipboard; runs `vds_server_dispatch()` from a bottom half.
- RDP thread: a tokio runtime with a `LocalSet` (IronRDP connection futures are not `Send`);
  accepts connections and runs one IronRDP server per session.
- Per session, a video thread converts damaged macroblocks and drives EGFX; per head, an
  encoder thread runs openh264/VA-API with two picture buffers, so conversion of frame N+1
  overlaps encoding of frame N. Frame acknowledgements pace the pipeline.
- Short-lived threads convert clipboard images (PNG ⇄ DIB).

**Frame path.** Guest scanout → QEMU surface → shared in place (`vds_head_attach_shared`) or
copied into the head's shadow buffer → damage accumulates in `vds-core` → the session's video
thread takes it → EGFX: BGRX→I420 of damaged macroblocks, H.264, and ClearCodec tiles for
areas that stopped changing (`egfx-mixed`); legacy: RemoteFX/bitmap updates by IronRDP →
client. Without an EGFX client the legacy path is used; codec order is fixed: AVC420,
RemoteFX, bitmap.

**Other paths.** Cursor shapes go to the client as pointer updates and are drawn locally.
Audio: QEMU's `rdp` audiodev or a capture of any other audiodev → 10 ms waves over rdpsnd.
Clipboard: QEMU clipboard (via `qemu-vdagent`) ⇄ cliprdr, text and PNG. Resolution:
DisplayControl and `resize-on-connect` → `dpy_set_ui_info` → the guest driver.

**Web clients.** Browser (IronRDP web) ⇄ WebSocket ⇄ terminator (pveproxy, websockify) ⇄
`vds-rdcleanpath` (RDCleanPath handshake, TLS) ⇄ QEMU `-rdp`.

## Build

Requirements: Rust (version pinned in `rust-toolchain.toml`), a C/C++ compiler and nasm (openh264 is built from source), pkg-config,
`cbindgen` (`cargo install cbindgen`) for header generation.

```bash
cargo build --release -p vds-capi --features vaapi   # drop --features vaapi without VA-API
cargo run -p xtask -- install --prefix "$HOME/vds-prefix"
```
Windows hosts build the same way from an MSYS2 UCRT64 shell (toolchain
`1.94.1-x86_64-pc-windows-gnu`, no `vaapi` feature).

Debian packages (`libvds0`, `libvds-dev`; SONAME `libvds.so.0`), built inside the target
distribution's builder image:

```bash
cargo deb -p vds-capi
cargo deb -p vds-capi --variant dev --no-build
```
The Proxmox VE QEMU package with `-rdp` is built from `jiangcuo/pve-qemu` with the standard
`apt update && mk-build-deps --install --remove && make deb` once `libvds-dev` is installed:

| PVE | pve-qemu branch | QEMU | builder image | QEMU fork branch the patches come from |
|---|---|---|---|---|
| 8 (bookworm) | `vds-pve8` (on `ramfb`) | 10.0.12 | `pxvirt-builder-bookworm` | `rdp` (v10.0.10) |
| 9 (trixie) | `vds-pve9` (on `master`) | 11.1.1 | `pxvirt-builder-trixie` | `rdp-11` (v11.1.1) |

The patches sit in `debian/patches/vds/` of each branch. QEMU 11 changed the audio backends
(QOM types) and the console API, so the two patch sets differ in those places.

## Try it without QEMU

```bash
cargo run --release -p vds-demo -- --listen 127.0.0.1:3389 --user vds --password secret
```
Then connect with `mstsc /v:127.0.0.1:3389` or `xfreerdp /v:127.0.0.1:3389 /u:vds /p:secret /gfx`.

## QEMU

Build the fork with `--enable-rdp` (see `docs/spec/c-qemu-style.md`), then:

```bash
qemu-system-x86_64 ... \
  -object secret,id=rdppw,data=secret \
  -rdp addr=127.0.0.1,port=3389,username=vdi,password-secret=rdppw \
  -audiodev rdp,id=snd0 -device intel-hda -device hda-duplex,audiodev=snd0 \
  -device virtio-vga \
  -chardev qemu-vdagent,id=vda,clipboard=on \
  -device virtio-serial -device virtserialport,chardev=vda,name=com.redhat.spice.0 \
  -device usb-tablet
```

One-time credentials from management software (QMP):

```json
{"execute": "set_password", "arguments": {"protocol": "rdp", "password": "<random>",
  "username": "u-7f3a", "one-time": true, "connected": "disconnect"}}
{"execute": "expire_password", "arguments": {"protocol": "rdp", "time": "+60"}}
{"execute": "query-rdp"}
```

GL guests (virgl, vfio display): add `-display egl-headless`.

- **Audio**: any audiodev works. `-audiodev rdp` is forwarded directly; with another driver
  (`none`, `pa`, `spice`...) the mixed output is captured. With several audiodevs pick one with
  `-rdp ...,audiodev=<id>`.
- **Clipboard** needs the `qemu-vdagent` chardev with `clipboard=on` and spice-vdagent in the
  guest. Add `mouse=off` to it when the VM has a USB tablet, otherwise every pointer motion is
  also sent through the agent.
- **Display**: use `virtio-vga` (or `virtio-gpu-pci`) for a hardware cursor and resolution
  changes. Windows guests need the virtio-win `viogpudo` driver with `HWCursor=1` (our fork
  `jiangcuo/kvm-guest-drivers-windows`, branch `viogpu-cursor-fixes`, enables it by default and
  fixes stale/doubled pointer shapes) and the `vgpusrv` resolution service.

## Web client

The IronRDP web client speaks RDCleanPath over a WebSocket. `vds-rdcleanpath` answers it and
connects to the RDP server; something else terminates the WebSocket and forwards its payload
as a byte stream:

```bash
# test setup: browser -> websockify -> vds-rdcleanpath -> QEMU -rdp
cargo build -p vds-rdcleanpath
target/debug/vds-rdcleanpath --listen 127.0.0.1:9100 --connect 127.0.0.1:3389
websockify 127.0.0.1:9443 127.0.0.1:9100
```
In the web client set the gateway to `ws://127.0.0.1:9443` and any non-empty auth token; the
bridge always connects to its `--connect` target, whatever host the page names. Without
`--listen` it serves stdin/stdout, the form meant for Proxmox VE (an `rdpproxy` API spawning it
like `qm vncproxy`, pveproxy terminating the WebSocket).

## Client requirements and known limits

- **Keyboard**: clients must send scancodes. Unicode key events
  (`KeyboardEvent::UnicodePressed`) cannot be mapped to a guest scancode and are dropped —
  QEMU has no guest keymap to reverse them with — so FreeRDP's `/kbd:unicode` mode does not
  type into the guest. `mstsc` and the default FreeRDP mode are fine.
- **Ctrl+Alt+Del**: clients send Ctrl+Alt+End (their own OS eats Ctrl+Alt+Del). libvds
  injects Ctrl+Alt+Del for it, like mstsc; `-rdp sas-hotkey=off` injects the keys as sent.
- **Pause does not work**: clients send it as EXTENDED1 + 0x45, and `ironrdp-server` keeps
  only the E0 flag, so it arrives as NumLock. Needs a patch to IronRDP.
- **Web client**: IronRDP web negotiates no EGFX, so it gets the legacy bitmap path (more
  bandwidth than H.264).
