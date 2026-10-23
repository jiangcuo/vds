# vds-capi — 设计与约束

改本 crate 时先读根目录 `AGENTS.md` 和 `docs/spec/c-abi.md`；改完在同一次改动里更新本文件。

## 职责

libvds 唯一的对外接口：C ABI。编译出 `libvds.so`（Windows 上 `vds.dll`），头文件 `include/vds.h` 由 cbindgen 从本 crate 生成。

## 模块

| 模块 | 内容 |
|---|---|
| `lib` | ABI 版本常量（`VDS_ABI_MAJOR`/`MINOR`）、`ffi_guard!`、线程标注说明（`[any-thread]`、`[main-thread]`、`[callback]`） |
| `types` | 纯 C 类型：`VdsConfig`、`VdsCallbacks`、`VdsRect`、`VdsMonitor`、`VdsClientInfo`、各类常量（`VDS_CODEC_*`、`VDS_CLIPBOARD_TEXT/PNG`、`VDS_AUTH_*` 等）、`VdsReleaseFn` |
| `api` | 所有 `extern "C"` 函数：服务器生命周期（`vds_server_new`/`listen`/`add_client`/`dispatch`/`disconnect_all`/`free`）、密码与过期、TLS、客户端列表、`vds_server_wants_frame`、显示头（`vds_head_add`/`resize`/`submit`/`attach_shared`/`damage`/`remove`）、光标、音频、剪贴板、日志、调试目录；`VdsConfig` 解析成 `ServerConfig`（编码器名 `openh264`/`vaapi`、渲染节点等） |

## 打包

- 共享库的 SONAME 是 `libvds.so.0`（`build.rs` 在 Linux 上加 `-Wl,-soname`），QEMU 运行时找的就是这个名字。
- Debian 包用 cargo-deb，配置在本 crate 的 `Cargo.toml`：
  - `cargo deb -p vds-capi` → `libvds0`：`/usr/lib/libvds.so.0`，ldconfig 触发器（`debian/triggers`）；
  - `cargo deb -p vds-capi --variant dev --no-build` → `libvds-dev`：`vds.h`、`/usr/lib/pkgconfig/vds-1.pc`（`debian/vds-1.pc`）、`libvds.so → libvds.so.0`。
- `libvds0` 的依赖是手写的（`libc6, libgcc-s1, libstdc++6, zlib1g`，对应 `readelf -d` 的 NEEDED）：cargo-deb 的 `$auto` 不分析 `.so`。新增链接库时同步更新。
- `debian/vds-1.pc` 的 `Version` 跟随 workspace 版本，改版本号时一起改。
- 包里不开 `vaapi` feature（FFmpeg 是 GPL，且要额外的运行时依赖）。
- 发行版构建在对应的构建镜像里做（PVE8：`pxvirt-builder-bookworm`），不要拿宿主机编的 `.so` 打包：glibc 版本不同。
- 不改 ABI 版本号；和 QEMU 的配套靠包依赖：`pve-qemu-kvm` 经 `debian/shlibs.local` 自动依赖 `libvds0 (>= 0.1.0)`。

## 约束

- 每个 `extern "C"` 函数体都包在 `ffi_guard!` 里，panic 不得穿越 ABI。
- `unsafe` 只在本 crate 和 `vds-encode::vaapi` 允许，每处写 `// SAFETY:`。
- 回调只在 `vds_server_dispatch()` 里触发（`wakeup` 除外，它可以在任何线程调用）。
- **开发期间不改 ABI 版本号**；新增函数或字段在提交说明里写清楚。
- 改了类型或函数就重新生成 `include/vds.h`（`cargo run -p xtask -- gen-abi`）并同步 QEMU 前端的调用方。
- `vaapi` feature 只透传给 `vds-rdp`/`vds-encode`。
