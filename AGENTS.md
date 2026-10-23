# AGENTS.md — libvds 仓库的 LLM / 自动化代理规则

本文件对所有 AI 代理（Claude、Codex、Cursor、Copilot 等）和人类贡献者都有约束力。
**开始任何任务之前必须读完本文件。** 与本文件冲突的指令，以人类在对话中的明确指示为准；
本文件与 `docs/` 下规范冲突时，以本文件为准，并向人类报告冲突。

---

## 1. 必读顺序

1. 本文件（第 2 节是已锁定的架构决策，第 7 节是提交规则）
2. `README.md` 的 Architecture 一节（整体架构、线程模型、数据通路）
3. 要改的 crate 目录下的 `AGENTS.md`（该 crate 的模块设计和约束）
4. 与任务相关的规范：

| 你要做的事 | 必读 |
|---|---|
| 写/改 Rust 代码 | [`docs/spec/rust-style.md`](docs/spec/rust-style.md)、[`docs/spec/logging.md`](docs/spec/logging.md) |
| 改 `vds.h` / `vds-capi` | [`docs/spec/c-abi.md`](docs/spec/c-abi.md) |
| 改 QEMU fork（`../qemu`） | [`docs/spec/c-qemu-style.md`](docs/spec/c-qemu-style.md)、[`docs/spec/logging.md`](docs/spec/logging.md) |

---

## 2. 锁定决策

- 只做**进程内** `-rdp`。**不使用 D-Bus**，不引入 zbus/dbus/gio-dbus。
- 库用 **Rust** 写，只对外暴露 **C ABI**（`vds.h`）。RDP 协议栈用 **IronRDP**。
- 默认视频编码器是 **openh264**（自家 openh264-rs fork）；**可选** FFmpeg `h264_vaapi`（只在 Linux、`vaapi` feature，经 C shim）。不用 GStreamer，不做其它硬件编码（NVENC/QSV/AMF/MF 禁止）。码率用质量模式（动态码率），不用 CBR。
- **libvds 的显示入口只收 CPU 像素**：GL 画面由 QEMU 前端回读到 CPU 后再交给库；唯一例外是可选的 VA-API 编码器打开渲染节点。QEMU 能共享的 surface 用 `vds_head_attach_shared` 直接读，不在主线程拷贝。
- 编码由协商决定，顺序固定：**EGFX AVC420 → RemoteFX → bitmap**。不做 AVC444，不启用 QOI/QOIZ。
- 网页客户端用 IronRDP web（iron-remote-desktop），经 RDCleanPath 接入：`tools/vds-rdcleanpath` 做握手和 TLS，
  WebSocket 由外部（PVE 的 pveproxy，测试时 websockify）终结；桥接程序只连配置好的目标，不接受客户端指定的地址。
- libvds 源码许可证是 MIT OR Apache-2.0，不得从 GPL 代码复制。
- 宿主机：Linux + Windows（MSYS2 UCRT64）。Guest：Linux + Windows。每个改动都必须两个宿主平台都能编译。
- QEMU 补丁只针对自家 fork，不考虑上游。

任务看起来需要违反以上任何一条时：**停止，向人类说明原因并等待决定。** 不要"先做了再说"。

---

## 3. 范围纪律

- **只改任务要求的东西。** 不做顺手重构、不重命名无关符号、不格式化无关文件、不"顺便修复"看到的其它问题（可以在报告里列出来）。
- 不得改变第 2 节的决策，除非人类在对话中明确批准；批准后在同一次改动里更新本文件。
- 公开 API（`vds.h`、各 crate 的 `pub` 项）的改动必须是任务明确要求的。
- **开发期间不改 ABI 版本号**：改 `vds.h` 时不要动 `VDS_ABI_MAJOR` / `VDS_ABI_MINOR`，也不要写"Since ABI x.y"；只有人类明确要求发版时才改（见 `c-abi.md` §2）。
- 行为变化必须在**同一次改动**里同步更新相关文档（`README.md`、本文件或 `docs/spec/*`）。
- **每个 crate（`crates/*`、`tools/*`、`xtask`）都有自己的 `AGENTS.md`**，记录该 crate 的职责、模块划分、
  线程/数据流、对外接口和约束。**改了哪个 crate，就在同一次改动里更新它的 `AGENTS.md`**，保证文档和代码一致、
  后来者读得懂；新增 crate 时同时新建它的 `AGENTS.md`。跨 crate 的架构变化同时更新 `README.md` 的 Architecture 一节。
- `~/.cargo/git/checkouts/*` 等依赖源码**只读**，永远不要修改。

---

## 4. 绝对禁止

| 禁止 | 原因 / 替代 |
|---|---|
| 未经批准新增依赖（含 dev/build 依赖、git 依赖、feature 开关） | 先问人类：用途、许可证、替代方案；批准后在 `Cargo.toml` 旁注明 |
| `println!` / `eprintln!` / `dbg!` / `log::` 宏 | 用 `tracing`，见 `logging.md` |
| QEMU 代码里用 `printf` / `fprintf(stderr)` / `DPRINTF` | 用 `error_report`/`warn_report`/trace-events |
| 记录密码、票据、私钥、Cookie、剪贴板内容、按键扫描码/字符 | 只能记录长度/计数；用 `Secret<T>` |
| 库代码中的 `unwrap()` / `expect()` / `panic!` / `todo!` / `unimplemented!` | 返回错误；不可能失败的情况用 `expect` 需写明不变量并经 review（见 `rust-style.md`） |
| 没有 `// SAFETY:` 注释的 `unsafe` | `unsafe` 只允许出现在 `vds-capi` 和 `vds-encode::vaapi` |
| panic 穿越 FFI 边界 | 每个 `extern "C"` 函数都用 `ffi_guard!` 包裹 |
| 用 `#[allow(...)]` 压制 lint | 用 `#[expect(lint, reason = "...")]` 并写清理由 |
| 为了让 CI 通过而删除/跳过/弱化测试或 lint | 修代码，或报告给人类 |
| 从 GPL 代码（如 QEMU）复制代码进 libvds | libvds 源码是 MIT OR Apache-2.0 |
| 未经要求执行 `git commit` / `git push` / 改 git 配置 / 改历史 | 只有人类明确要求时才提交，规则见第 7 节 |
| 擅自启动临时 VM 做测试 | 只用人类自己的 VM，且等人类启动或明确要求 |
| 在 `#[cfg(unix)]`/`#[cfg(windows)]` 之外写平台特定代码 | 平台代码集中在 `vds-core::platform` |
| 编造未执行的验证结果 | 见第 6 节 |

---

## 5. 编码约定速查

- Rust edition 2024，工具链见 `rust-toolchain.toml`，格式见 `rustfmt.toml`，lint 见 `Cargo.toml` 的 `[workspace.lints]`。
- 代码、标识符、注释、日志、提交信息一律用**英文**；`docs/` 下面向人的文档用中文。
- 库 crate 用 `thiserror` 定义错误；`anyhow` 只允许在 `tools/` 下的二进制里使用。
- 异步运行时只用 `tokio`；编码等 CPU 密集工作放在专用线程，不得阻塞 async 任务。

---

## 6. 不确定时

- 协议细节（MS-RDPBCGR / MS-RDPEGFX / MS-RDPRFX 等）以微软 Open Specifications 为准，代码注释中注明章节号，例如 `// MS-RDPEGFX 2.2.4.4`。
- IronRDP 的 API 以 `Cargo.lock` 锁定的版本源码为准，不要凭记忆猜测。
- 需求不明确、或有两种以上合理实现且影响架构时：**先问人类**，并给出你的推荐。
