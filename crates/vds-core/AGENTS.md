# vds-core — 设计与约束

改本 crate 时先读根目录 `AGENTS.md`；改完在同一次改动里更新本文件。

## 职责

与协议无关的共享状态。宿主（QEMU 经 `vds-capi`）往 [`Core`] 里放画面、光标、音频、剪贴板；
协议后端（`vds-rdp`）从里面取，并把客户端事件推回来，由宿主在主线程取走。
本 crate 不依赖 IronRDP、tokio 或任何编码器。

## 模块

| 模块 | 内容 |
|---|---|
| `core` | `Core`：上述状态的总入口；事件队列（`push_event`/`drain_events`，上限 `MAX_PENDING_EVENTS`）、`set_waker`、会话登记（`SessionSink`）、客户端信息、剪贴板/音频转发给各会话 |
| `display` | 显示头（head）：尺寸、累积脏区、两种像素来源——影子缓冲（`submit` 拷贝）或宿主缓冲原地读取（`attach_shared` + `damage`）；`take_frame` 取脏区和像素；`invalidate`；`signal` 唤醒消费者；`set_consumer`/`set_busy`/`wants_frame` 做背压 |
| `cursor` | 光标形状（ARGB，上限 `MAX_CURSOR_DIMENSION`）和可见性，`generation` 每次变化加一 |
| `event` | 宿主⇄会话的事件：`ClientEvent`（输入、布局、剪贴板、客户端状态）、`HostClipboardEvent`、`ClipboardType`（Text=1、Png=2，与 `VDS_CLIPBOARD_*` 一致） |
| `session` | `SessionSink` trait：会话接收宿主数据（剪贴板、音频、音量、断开） |
| `auth` | 凭据、过期、一次性密码（对应 QMP `set_password` / `expire_password`） |
| `config` | `ServerConfig`：`-rdp` 中与监听/TLS 无关的选项，含编码器种类 `VideoEncoderKind`、`SasHotkey`、qp 上限等 |
| `codec` | 编码标识与协商：`select(allowed, egfx)`，顺序固定 AVC420 → RemoteFX → bitmap |
| `debug` | `debug-dir`：每会话一个目录，`session.log`、`client.json`、`frames.csv`、`pointer.csv`、`input.csv`、`audio.csv`、可选 `video-head<N>.h264` |
| `log` | 日志初始化（text/json、过滤器） |
| `secret` | `Secret<T>`：敏感值，打印时脱敏，析构时清零 |
| `signal` | 线程和 async 任务都能等待的唤醒信号 |
| `geom` | `Rect`、`Region`（脏区） |

## 线程与数据流

- 宿主线程调用 `Head::submit`/`damage`/`resize`、`Cursor::define`、`Core::audio_out` 等，都是短时间持锁、不阻塞网络。
- `attach_shared` 的缓冲由宿主保证在释放回调前有效；`take_frame` 读它时**不持锁**，宿主不会等待。
- 客户端事件进 `Core` 的队列后调用 waker，宿主在主线程 `drain_events`。

## 约束

- 不得引入协议或编码相关依赖；平台相关代码只放在 `platform`（目前没有）。
- 敏感数据只用 `Secret<T>`；按键内容、剪贴板内容不得进日志，`input.csv` 的脱敏规则见 `docs/spec/logging.md`。
- `ClipboardType` 的数值是 C ABI 的一部分，改动要同步 `vds-capi` 和 `include/vds.h`。
- 修改 `display` 的背压或脏区语义时，要同时检查 `vds-rdp` 的 EGFX 与 legacy 两条消费路径。
