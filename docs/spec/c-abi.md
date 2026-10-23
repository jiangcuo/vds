# C ABI 规范（`vds.h` / `vds-capi`）

`vds.h` 是 libvds 与 QEMU 之间唯一的契约。改它就是改接口，必须遵守本文件。

## 1. 产物

| 平台 | 库 | 头文件 | pkg-config |
|---|---|---|---|
| Linux | `libvds.so` | `include/vds.h` | `vds-1.pc` |
| Windows（UCRT64） | `vds.dll` + `libvds.dll.a` | `include/vds.h` | `vds-1.pc` |

- 由 `cargo run -p xtask -- install --prefix DIR` 构建（release）并安装库、头文件和 `vds-1.pc`
  （Windows 上在 MSYS2 UCRT64 shell 中执行）。
- `include/vds.h` 由 cbindgen 生成并**提交到仓库**；`cargo run -p xtask -- check-abi` 检查生成结果与提交版本一致。
  不得手工编辑生成段落。

## 2. 版本

- `VDS_ABI_MAJOR` / `VDS_ABI_MINOR` 宏 + `uint32_t vds_abi_version(void)`（`major << 16 | minor`）。
- **开发期间不改版本号**（2026-10-22 人类决定）：libvds 与 QEMU 前端一起开发、一起构建，
  增删函数或字段时**不得**改 `VDS_ABI_MAJOR` / `VDS_ABI_MINOR`，也不要在文档里写"Since ABI x.y"。
  只有人类明确要求发版时才改版本号。
- ABI 2.0（2026-10-22，人类批准）：从 `VdsConfig` 删除旧的编码器预设字段。
- QEMU 前端启动时检查 major 相同、minor ≥ 编译时的值，否则报错退出。

## 3. 命名

| 对象 | 规则 | 示例 |
|---|---|---|
| 函数 | `vds_<对象>_<动作>` | `vds_head_submit` |
| 不透明句柄 | `Vds<对象>`，只以指针形式出现 | `VdsServer *` |
| 结构体 | `Vds<名称>` | `VdsConfig` |
| 枚举值 | `VDS_<枚举>_<值>` | `VDS_LOG_LEVEL_INFO` |
| 错误码 | `VDS_E_<名称>`，负数 | `VDS_E_INVALID_ARG` |

## 4. 类型与内存

- 只使用固定宽度整数（`uint32_t` 等）、`bool`、`size_t`、`intptr_t`、指针和本库定义的结构体。不使用 `long`、`int` 作字段。
- 字符串：UTF-8、以 NUL 结尾。库**不保留**调用方传入的指针，需要时自行复制。
- 配置结构体第一个字段是 `uint32_t struct_size`；`VdsConfig` 必须用 `vds_config_init()` 初始化后再修改字段。
- **谁分配谁释放**：库返回的字符串用 `vds_free_string()` 释放；句柄用对应的 `vds_*_free()`。
  不跨边界传递 `FILE *`、CRT 句柄或要求对方 `free()` 的内存（Windows 上不同 CRT 会崩溃）。
- 像素缓冲区：`vds_head_submit()` 在返回前完成拷贝，返回后调用方可以修改缓冲区。
- 共享像素缓冲区（ABI 1.5 起）：`vds_head_attach_shared()` 把宿主的帧缓冲交给库**原地读取**，宿主之后只用
  `vds_head_damage()` 报告脏区，不再拷贝。库在自己的编码线程上读，宿主可以继续写（偶尔出现撕裂，由下一帧的
  脏区修复）。库不再需要这块缓冲时（换了新缓冲、head 改尺寸或删除、`vds_head_submit()` 切回拷贝），在**任意线程**
  调用一次 `release(opaque)`；在那之前宿主必须保证缓冲区可读。调用失败时不会调用 `release`，缓冲区仍归宿主。
  QEMU 前端只对可共享内存的 surface（virtio-gpu 资源，`share_handle` 有效）使用此方式，其余仍走拷贝。
- `VdsConfig.encoder` / `VdsConfig.render_node`（ABI 1.6 起）：`"openh264"`（默认）或 `"vaapi"`，以及 VA-API 用的 DRM 渲染节点；库复制字符串。
- socket：用 `intptr_t` 传递（Linux 为 fd，Windows 为 `SOCKET`）。`vds_server_add_client()` 返回 `VDS_OK`
  时所有权已转移给库；返回错误时调用方仍拥有该 socket。Windows 上 QEMU 先用 `_get_osfhandle()` 取得
  `SOCKET`，成功后用 `qemu_close_socket_osfhandle()` 只释放 CRT 描述符。

## 5. 错误约定

- 返回 `int` 的函数：`0` 成功，负数为 `VDS_E_*`。
- 需要错误描述的函数带 `char **err` 参数：失败时写入新分配的字符串（调用方用 `vds_free_string` 释放），可为 NULL。
- 返回指针的函数：失败返回 NULL，并通过 `err` 说明原因。
- 参数非法（NULL 句柄等）返回 `VDS_E_INVALID_ARG`，不崩溃。

## 6. 线程模型（每个函数都必须在文档注释中标明）

| 标注 | 含义 |
|---|---|
| `[any-thread]` | 任意线程可调用，非阻塞 |
| `[main-thread]` | 只能在宿主主线程调用（QEMU：持有 BQL） |
| `[callback]` | 库的回调，只会在 `vds_server_dispatch()` 内被调用，即宿主主线程 |
| `[any-thread callback]` | 可能在库的任意线程被调用（`wakeup` 和 `vds_head_attach_shared()` 的 `release`） |

规则：
- 库不会在 `vds_server_dispatch()` 之外调用除 `wakeup` 以外的任何回调。
- 回调中允许调用 `[any-thread]` 函数；**不允许**在回调中调用 `vds_server_dispatch()` 或 `vds_server_free()`。
- `vds_server_free()` 是 `[main-thread]`，会等待库内线程退出，之后不会再有任何回调。
- 所有 `[any-thread]` 函数都不得阻塞超过一次 memcpy 的时间（不等网络、不等编码）。

## 7. panic 与 unsafe

- 每个 `extern "C"` 函数体都用 `ffi_guard!` 包裹（内部 `std::panic::catch_unwind`）：
  panic 时记录 ERROR 日志，返回 `VDS_E_INTERNAL` / NULL。
- 入口处立即检查指针非空、长度合理，转换为安全的 Rust 类型；业务逻辑在安全代码中实现。
- 回调函数指针以 `Option<unsafe extern "C" fn(...)>` 表示，NULL 表示不处理该事件。

## 8. 文档

`vds.h` 中每个函数都要有注释，包含：作用、线程标注、参数所有权、返回值。cbindgen 会把 Rust 文档注释带入头文件，
所以注释写在 Rust 源码中。

## 9. 改动流程

1. 修改 `vds-capi` 源码与文档注释；
2. 重新生成 `include/vds.h`，提交生成结果；
3. 开发期间不改版本号（第 2 节）；
4. 同一次改动中更新 QEMU 前端的调用方（或在报告里明确说明 QEMU 侧尚未适配）；
5. 行为有变化时同步更新 `README.md`。
