# vds-core — 设计与约束

改本 crate 时先读根目录 `AGENTS.md`；改完在同一次改动里更新本文件。

## 职责

与协议无关的共享状态。宿主（QEMU 经 `vds-capi`）往 [`Core`] 里放画面、光标、音频、剪贴板；
协议后端（`vds-rdp`）从里面取，并把客户端事件推回来，由宿主在主线程取走。
本 crate 不依赖 IronRDP、tokio 或任何编码器。

## 模块

| 模块 | 内容 |
|---|---|
| `core` | `Core`：上述状态的总入口；事件队列（`push_event`/`drain_events`，上限 `MAX_PENDING_EVENTS`）、`set_waker`、会话登记（`SessionSink`）、客户端信息、剪贴板/音频转发给各会话；`register_session`/`unregister_session` 同时登记/注销帧消费者（消费者 id = 会话 id）；`client_clipboard(sid, types)` 记录剪贴板所有者（最后复制的客户端），`clipboard_request` 只发给所有者，所有者注销或清空时推送 `ClipboardRelease`；`active_sessions()`；`demand_cpu(head, reason)` 返回 `CpuDemand` 守卫，每个 head 的第一个需求和最后一个释放各发一次 `ClientEvent::HeadInput` |
| `display` | 显示头（head）：尺寸、累积脏区、三种像素来源——影子缓冲（`submit` 拷贝）、宿主缓冲原地读取（`attach_shared` + `damage`）或 GPU 缓冲（`attach_gpu`，`GpuBuffer` 持有 dmabuf；`detach` 回到影子缓冲）；**每个消费者（会话）各有一份脏区和年龄**（`add_consumer`/`remove_consumer`，新消费者从全屏开始；`damage`/`submit`/`resize`/`attach_*` 写进所有消费者）；`take_frame(consumer, ..)` 只取自己的脏区和像素，`has_damage(consumer)`、`invalidate(consumer)` 只影响自己，GPU 缓冲时 `FrameRef::gpu` 是 `GpuFrame`（`pixels` 为空），宿主报过脏区的那一帧带票据，最后一个克隆释放时调用 `ConsumedNotify::consumed`；`invalidate`；`signal` 唤醒消费者；`set_busy(consumer, ..)` 每个消费者各自背压，`wants_frame` = 至少一个消费者不忙 |
| `platform` | 平台相关类型：`BufferFd`（Unix 上包一个 `OwnedFd`；Windows 上是无法构造的空枚举） |
| `cursor` | 光标形状（ARGB，上限 `MAX_CURSOR_DIMENSION`）和可见性，`generation` 每次变化加一 |
| `event` | 宿主⇄会话的事件：`ClientEvent`（输入、布局、剪贴板、客户端状态）、`HostClipboardEvent`、`ClipboardType`（Text=1、Png=2，与 `VDS_CLIPBOARD_*` 一致） |
| `session` | `SessionSink` trait：会话接收宿主数据（剪贴板、音频、音量、断开） |
| `auth` | 凭据、过期、一次性密码（对应 QMP `set_password` / `expire_password`） |
| `config` | `ServerConfig`：`-rdp` 中与监听/TLS 无关的选项，含编码器种类 `VideoEncoderKind`、`SasHotkey`、`SharePolicy`（`ForceShared` 默认 / `Exclusive`）、`max_connections`（默认 32，1–64）、qp 上限等 |
| `codec` | 编码标识与协商：`select(allowed, egfx)`，顺序固定 AVC420 → RemoteFX → bitmap |
| `debug` | `debug-dir`：每会话一个目录，`session.log`、`client.json`、`frames.csv`、`pointer.csv`、`input.csv`、`audio.csv`、可选 `video-head<N>.h264` |
| `log` | 日志初始化（text/json、过滤器） |
| `secret` | `Secret<T>`：敏感值，打印时脱敏，析构时清零 |
| `signal` | 线程和 async 任务都能等待的唤醒信号 |
| `geom` | `Rect`、`Region`（脏区） |

## 线程与数据流

- 宿主线程调用 `Head::submit`/`damage`/`resize`、`Cursor::define`、`Core::audio_out` 等，都是短时间持锁、不阻塞网络。
- `attach_shared` 的缓冲由宿主保证在释放回调前有效；`take_frame` 读它时**不持锁**，宿主不会等待。
- GPU 缓冲同样不持锁交给消费方；`consumed` 回调（宿主代码）一律在锁外调用。替换、`detach`、`submit`、`resize` 放掉 GPU 缓冲时，宿主还在等就立即通知一次。
- 客户端事件进 `Core` 的队列后调用 waker，宿主在主线程 `drain_events`。

## 约束

- 不得引入协议或编码相关依赖；平台相关代码只放在 `platform`。GPU 缓冲的测试也在 `platform/unix.rs` 里。
- 敏感数据只用 `Secret<T>`；按键内容、剪贴板内容不得进日志，`input.csv` 的脱敏规则见 `docs/spec/logging.md`。
- `ClipboardType` 的数值是 C ABI 的一部分，改动要同步 `vds-capi` 和 `include/vds.h`。
- 修改 `display` 的背压或脏区语义时，要同时检查 `vds-rdp` 的 EGFX 与 legacy 两条消费路径。
