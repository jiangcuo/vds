# Rust 代码规范

适用于 `vds/` 下全部 Rust 代码。机器可检查的部分由 `rustfmt.toml`、`Cargo.toml` 的 `[workspace.lints]`
和 CI 强制；本文件规定机器查不出来的部分。

## 1. 工具链与格式

- 工具链：`rust-toolchain.toml` 锁定（当前 1.94.1，与上游 IronRDP 一致）。不得在代码里依赖 nightly 特性。
- Edition：2024。
- 格式：`cargo fmt --all`，配置见 `rustfmt.toml`。**只使用 stable rustfmt 选项**，保证所有人格式化结果一致。
- 行宽 120（与 IronRDP 一致）。
- 换行符 LF（`.gitattributes` 强制，Windows 上也一样）。

## 2. Workspace 结构

```
crates/vds-core       # 无 unsafe；trait、帧管线、会话、事件、Secret、platform
crates/vds-encode     # openh264、可选 VA-API C shim、颜色转换；unsafe 仅限 src/vaapi/
crates/vds-rdp        # IronRDP 后端；无 unsafe
crates/vds-usbredir   # 无 unsafe
crates/vds-capi       # extern "C"；unsafe 集中在此
crates/vds-test-host  # 合成画面源
tools/vds-demo        # 二进制
tools/vds-bench       # 二进制
xtask                 # 构建辅助（check-abi 等）
```

- crate 之间的依赖方向：`vds-capi → vds-rdp → vds-core ← vds-encode`；`vds-core` 不依赖任何其它 vds crate。
- 协议后端 crate 不得直接依赖 `vds-capi`；`vds-core` 不得依赖 IronRDP。

## 3. 命名

| 对象 | 规则 | 示例 |
|---|---|---|
| crate | `vds-<area>` | `vds-rdp` |
| 模块/函数/变量 | snake_case | `submit_damage` |
| 类型/trait | UpperCamelCase，缩写按单词处理 | `RdpSession`、`Avc420Encoder`（不是 `AVC420Encoder`） |
| 常量 | SCREAMING_SNAKE_CASE | `MAX_FRAMES_IN_FLIGHT` |
| 单位 | 写进名字 | `timeout_ms`、`bitrate_kbps`、`pts_us` |

- 不用含糊的缩写：`cfg`、`ctx`、`buf`、`len` 可以；`hdl`、`mgr2`、`tmp` 不行。

## 4. 错误处理

- 每个库 crate 有自己的 `Error` 枚举（`thiserror`），放在 `src/error.rs`；公开 `pub type Result<T> = std::result::Result<T, Error>;`
- `anyhow` 只允许在 `tools/` 和 `xtask` 中使用。
- 错误信息：英文、小写开头、不加句号，描述"什么失败了"：`#[error("failed to open the encoder ({width}x{height})")]`。
- 包装底层错误时用 `#[source]` / `#[from]` 保留错误链，不要把底层错误 `to_string()` 进消息。
- 禁止 `unwrap()`、`expect()`、`panic!`、`unreachable!`、`todo!`、`unimplemented!`、下标越界可能的 `[]` 索引
  （lint 已拒绝前几项）。确实不可能失败的地方：
  ```rust
  #[expect(clippy::expect_used, reason = "width is validated to be non-zero in HeadConfig::new")]
  let w = NonZeroU32::new(width).expect("non-zero width");
  ```
  `reason` 必须写出保证该不变量的具体位置。
- 测试代码（`#[cfg(test)]`、`tests/`）可以使用 `unwrap`/`expect`。

## 5. unsafe

- `unsafe` 只允许出现在：`vds-capi`、`vds-encode/src/vaapi/`。其它 crate 设置 `#![forbid(unsafe_code)]`。
- 每个 `unsafe` 块前必须有 `// SAFETY:` 注释，说明满足了哪些前提条件（lint `undocumented_unsafe_blocks` 强制）。
- 每个 `unsafe fn` 必须有 `# Safety` 文档节。
- FFI 指针在入口处立即转换为安全类型（`&[u8]`、`&str` 等），内部逻辑不传递裸指针。

## 6. 并发与异步

- 异步运行时只用 `tokio`，由 `vds-core` 统一创建（多线程 runtime，线程数可配置），其它 crate 不得自行创建 runtime。
- H.264 编码、颜色转换等 CPU 密集工作放在**专用编码线程**（每个 head 一个），通过有界 channel 通信；
  不得在 async 任务中直接执行，也不要用 `spawn_blocking` 做长期编码循环。
- channel 一律**有界**；满了时的策略必须显式写出（丢弃旧帧、合并脏区、背压），不允许无界增长。
- 锁：优先 `parking_lot` 或 `std::sync`；不得在持锁期间 `.await`；不得在持锁期间调用回调（包括 C 回调）。
- 跨 C ABI 的回调只能在 `vds_server_dispatch()` 中调用（见 `c-abi.md`）。

## 7. 平台代码

- 平台相关代码集中在 `vds-core/src/platform/{unix,windows}.rs`，对外暴露统一接口。
- 其它位置不出现 `#[cfg(unix)]` / `#[cfg(windows)]`，除非是 `vds-capi` 中 socket/fd 类型的转换。
- 不使用只在一个平台存在的 crate 而不加替代实现。

## 8. 性能热路径规则

热路径 = 每帧或每个包都会执行的代码（脏区合并、颜色转换、编码、封包、发送）。

- 不做按帧堆分配：缓冲区预分配并复用（`BytesMut`、对象池）。
- 不在热路径上 `clone()` 像素数据；用 `Bytes` / `Arc` 共享。
- 热路径上日志只能是 `trace!`（见 `logging.md`）。
- 修改热路径时，需要附 `vds-bench` 前后对比数据（P0 完成后生效）。

## 9. 注释与文档

- 所有 `pub` 项必须有文档注释（lint `missing_docs` 在库 crate 中为 warn→CI 拒绝）。
- 注释解释"为什么"，不复述代码"做了什么"。
- 协议相关代码注明规范章节：`// MS-RDPEGFX 3.3.5.2: server must wait for CapsAdvertise`。
- 不留下被注释掉的代码；`TODO` 必须带负责人或任务标识：`// TODO(P3): adaptive bitrate`。
- 从其它项目移植的文件，文件头注明：
  ```rust
  // Ported from <project> (<path>), <license>,
  // Copyright (c) <holder>.
  ```

## 10. 敏感数据

- 密码、票据、私钥、会话 Cookie 用 `vds_core::Secret<T>` 包装，其 `Debug`/`Display` 输出 `<redacted>`，
  `Drop` 时清零内存。
- 不得为 `Secret<T>` 实现 `Serialize` 或把内部值传给日志。
- 按键数据和剪贴板内容视为敏感数据，不得以任何级别记录内容（见 `logging.md`）。

## 11. 测试

修 bug 时尽量补一个能复现它的测试，做不到时在报告中说明原因。不主动跑 `cargo test` 或整套测试，除非人类要求（`AGENTS.md` 第 6 节）。
