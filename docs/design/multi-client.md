# 多人同时连接（多客户端共享）设计（草案）

状态：已实现（libvds + QEMU `rdp` / `rdp-11`），尚未提交。2026-11-14 人类决定：**默认多人共享**，对齐 QEMU VNC 的 `share=` / `connections=`。实现与计划的差别和验证结果见第 11 节。

## 1. 现状

同一时间只有一个客户端在用：

- `vds-rdp::manager`：最多 `MAX_SESSIONS = 8` 个连接（握手中的也算），但一个客户端激活后，其他已激活的会话都会被断开（`manager.rs` 的 `ManagerEvent::Activated`，"replaced by a new connection"）。
- 很多地方默认只有一个消费者：
  - `vds-core::display`：每个 head 只有一份脏区，`take_frame` 会把它取走；
  - `set_consumer` / `set_busy` 是全局开关；
  - EGFX 和 legacy 在重新配置时调用 `invalidate_all`；
  - 剪贴板请求发给所有会话；
  - QEMU 前端的 `rdp-core.c` 只记一个 `active_client_sid`。

## 2. 目标与非目标

目标：
- 多个 RDP 客户端同时连接同一台 VM，都能看到完整、正确的画面，都能用键盘和鼠标操作（和 VNC 共享模式一样，谁动都算数）；
- 每个客户端各自协商编解码方式（AVC420 / RemoteFX / bitmap）、各自控制码率和背压，慢的客户端不拖累快的；
- 保留现在的独占模式，作为可选项。

非目标（第一版不做）：
- 多个客户端共用同一路编码（N 个客户端就编 N 路，见第 6 节）；
- 只能看、不能操作的旁观模式；
- 按用户区分权限。

## 3. 选项（对齐 VNC）

QEMU VNC：`share=allow-exclusive|force-shared|ignore`（默认 `allow-exclusive`，由客户端在 ClientInit 里声明共享或独占），`connections=N`（默认 32）。

RDP 协议里**没有**客户端声明共享或独占的标志，所以 `allow-exclusive` 和 `ignore` 没有对应的含义。`-rdp` 提供：

| 选项 | 取值 | 默认 | 含义 |
|---|---|---|---|
| `share` | `force-shared` | **是** | 所有客户端共享，互不断开 |
| | `exclusive` | | 现在的行为：新客户端激活后断开其他客户端 |
| `connections` | 1–64 | 32（同 VNC） | 同时存在的连接数上限（握手中的也算）；超过的直接拒绝并记日志 |

- 写了其他取值就报错，错误信息列出可用的值。
- `connections=1` 时，第二个连接直接被拒，原有客户端不受影响，这一点和 `exclusive` 不同。
- **默认值的变化要在 README 里明确写出来**：升级后，第二个客户端不会再把第一个顶掉。

## 4. 各部分的行为

| 方面 | `force-shared` 下的行为 |
|---|---|
| 画面 | 每个客户端都收到完整画面；新加入的客户端只给**它自己**发一个全屏 IDR（或全量位图），不影响其他人 |
| 背压 | 每个客户端各自背压；只要还有一个客户端能收，宿主就继续提交 |
| 编码 | 每个会话各自有视频线程和编码器（现在就是这样），各自协商编解码方式、码率、帧率 |
| 键盘、鼠标 | 所有客户端的输入都直接注入 guest；客户端断开时，把它还按着的键和鼠标键全部释放 |
| 锁定键同步 | 客户端获得焦点时发的 LockSync 照样生效，以最后一次为准 |
| 光标 | 每个客户端各自在本地画；形状广播给所有人（现在就是这样） |
| 分辨率 | `resize-on-connect` 只在**当前没有其他激活客户端**时生效；客户端主动调整大小（DisplayControl）时，最后一次请求生效，其他客户端跟着 guest 的新分辨率走 |
| 剪贴板 guest → 客户端 | 广播给所有客户端（现在就是这样） |
| 剪贴板 客户端 → guest | 最后一次复制的客户端是剪贴板所有者；guest 要数据时只向它请求；所有者断开后释放 guest 剪贴板 |
| 音频 | 广播给所有客户端（现在就是这样） |
| dmabuf | 只要有一个客户端需要 CPU 像素，这个 head 就改成回读（`CpuDemand` 本来就是计数的）；其他 VA-API 客户端改走上传路径，照常工作 |
| 一次性密码 | 只放进一个客户端（激活时消耗，现在就是这样）；之后的客户端需要新密码 |
| `set_password connected=disconnect` | 断开所有客户端（现在就是这样） |

## 5. libvds 改动

### 5.1 vds-core：每个消费者独立的脏区和背压（核心）

- 新增 `display::ConsumerId`，以及 `Display::add_consumer() -> Consumer` 守卫（drop 时注销），由每个会话持有。
- `HeadState` 的 `damage` / `damage_at` 改成 `BTreeMap<ConsumerId, (Region, Option<Instant>)>`：
  - `damage()` / `submit()` / `resize()` / `attach_*()` 把脏区写进**所有**消费者；
  - `take_frame(consumer, ..)` 只取这个消费者自己的那份，和它的年龄；
  - `invalidate(consumer)` 只把这个消费者那份标成全屏，取代各会话里的 `invalidate_all()`；
  - `has_damage(consumer)`。
- 没有任何消费者时，脏区不累积（现在也是这样：没有客户端时宿主不提交）。消费者加入时自己的那份初始化为全屏。
- 背压：`busy` 改成每个消费者一个标志；`wants_frame()` = 至少有一个消费者，并且至少一个不忙。`set_consumer(bool)` 去掉，消费者的有无由 `Consumer` 守卫决定。
- GPU 帧的读完通知按 `take_frame` 发，多个消费者各自取帧不受影响。QEMU 现在不挡 guest，`consumed` 传 NULL，不涉及。
- 单元测试：两个消费者交替取帧，脏区互不影响；新消费者只拿到全屏；一个消费者忙时 `wants_frame` 仍为真；守卫 drop 后脏区不再为它累积。

### 5.2 vds-core：共享策略和剪贴板所有者

- `ServerConfig` 新增 `share: SharePolicy { ForceShared, Exclusive }`（默认 `ForceShared`）和 `max_connections: u32`（默认 32，范围 1–64）。
- `Core` 新增 `clipboard_owner: Mutex<Option<u64>>`：
  - 客户端发来剪贴板 Grab 时，把所有者设为这个 sid；
  - `clipboard_request` 只发给所有者的 `SessionSink`；
  - 所有者注销时清空，并推送一次 `ClientEvent::ClipboardRelease`，让 guest 剪贴板不再指向一个已经断开的客户端。
- `Core` 新增 `active_sessions() -> usize`，供分辨率策略使用。

### 5.3 vds-rdp

- `manager`：`MAX_SESSIONS` 改成 `config.max_connections`；只有 `share == Exclusive` 时才在 `Activated` 里断开其他会话。
- `egfx`：视频线程持有自己的 `Consumer`。
  - `take_frame` / `has_damage` / `invalidate` 都带上这个 `Consumer`；
  - `reconfigure` 里的 `invalidate_all()` 改成 `invalidate(consumer)`；
  - `set_busy(busy)` 改成设置自己的消费者。
- `display`（legacy）：同上，`DisplayUpdates` 持有自己的 `Consumer`。
- `input`：每个会话记录按下未松开的扫描码和鼠标键；会话结束（`session::run` 收尾时）为它们补发松开事件。
- `display::DisplayHandler`：
  - `resize_on_connect` 只在 `core.active_sessions() == 1` 时发送布局请求；
  - DisplayControl 请求照常发送。
- `clipboard`：客户端 Grab 时调用 `core.set_clipboard_owner(sid)`。

### 5.4 C ABI（`vds.h`）

- `VdsConfig` 末尾追加：
  - `uint32_t share`：`VDS_SHARE_FORCE_SHARED = 0`（默认），`VDS_SHARE_EXCLUSIVE = 1`；
  - `uint32_t max_connections`：0 表示默认 32。
- `vds_config_init()` 填默认值。按规则不改 ABI 版本号。
- `VdsClientInfo` 不变；`vds_server_clients()` 本来就返回所有客户端。

## 6. 编码开销

N 个客户端就编 N 路：
- openh264 是 N 倍 CPU；
- VA-API 是 N 套编码上下文。dmabuf 由每个会话各自导入，不会多出拷贝。

第一版接受这个开销，并在 README 里说明。以后可以优化成"编解码方式、分辨率、码率都相同的客户端共用一路码流"，但每个客户端的 IDR 时机和确认节奏都不一样，需要单独设计，不在这次范围内。

## 7. QEMU 前端（`rdp` 和 `rdp-11` 两条分支同步）

- `rdp-core.c`：
  - 新增 `share=`、`connections=` 选项，解析后填进 `VdsConfig`；取值非法时报错；
  - `active_client_sid` 改成已初始化客户端的计数，`qemu_rdp_has_client()` 改为"计数大于 0"；
  - 客户端状态回调按 sid 增减计数，QMP 事件仍按 sid 发送。
- `qapi/ui.json`：`RdpInfo` 新增 `share`（字符串）和 `connections`（整数），`hmp_info_rdp` 打印出来。
- `qemu-options.hx` 或 `-rdp` 的帮助文本里加上这两个选项的说明。
- `rdp-display.c` 和 `rdp-gl.c`：不用改。刷新节拍和 `wants_frame` 的语义对宿主不变。

## 8. 实施顺序

1. vds-core：消费者、脏区、背压（5.1），以及单元测试；
2. vds-core / vds-rdp：共享策略、连接上限、剪贴板所有者、断开时释放按键、分辨率策略（5.2、5.3）；
3. C ABI 和头文件（5.4），`xtask gen-abi` / `check-abi`；
4. QEMU 两条分支（第 7 节），checkpatch；
5. 文档：`README.md`（选项、默认值的变化、编码开销）、`docs/spec/c-abi.md`、`docs/spec/c-qemu-style.md`（第 97 行"替换客户端"那句）、`crates/vds-core|vds-rdp|vds-capi/AGENTS.md`（`manager` 一行的 `MAX_SESSIONS`）；
6. 端到端测试（第 9 节）。

每一步都要：clippy 无警告，新测试通过；Linux 和 Windows 宿主机都能编译（本机没有 mingw C 工具链的，要在报告里说明）；只有人类明确要求时才提交。

## 9. 验证

用 `research/dmabuf-verify/rdp-test.sh` 的方式，同时启动两到三个 `sdl-freerdp`：

| 场景 | 预期 |
|---|---|
| 两个 AVC420 客户端先后连接 | 两边都有完整画面；第二个连入时第一个不重发 IDR（`frames.csv` 里看不到额外的 idr 帧） |
| 一个 AVC420，一个 legacy（`-gfx /rfx`） | 两边都正常；有 dmabuf 时 head 切到回读，legacy 断开后切回 dmabuf |
| 一个客户端人为限速（`tc` 加延迟或限带宽） | 慢客户端背压，快客户端帧率不受影响 |
| 客户端 A 按住 Ctrl 时断开 | guest 里 Ctrl 被释放（用 `rdp_input` 的 trace 或 guest 里的 `xev` 确认） |
| A 复制、B 复制、guest 粘贴 | 粘贴到的是 B 的内容；B 断开后 guest 剪贴板被释放 |
| B 带 `resize-on-connect` 加入 | guest 分辨率不变；A 主动调整大小后 B 跟着变 |
| `share=exclusive` | 行为和现在一样（新客户端顶掉旧的） |
| `connections=2` 时连第三个 | 第三个被拒，日志里写明原因，前两个不受影响 |
| `query-rdp` / `info rdp` | 列出所有客户端，以及 `share` 和 `connections` |

## 10. 风险

| 风险 | 应对 |
|---|---|
| 每个消费者一份脏区，增加了 `submit`/`damage` 持锁时的工作量 | 每份只是 `Region` 合并，客户端数量少；用 `rdp_display_submit` 的 trace 对比单客户端时的主线程开销 |
| 多路 VA-API 编码时 GPU 编码器满载 | 编码线程本来就按帧确认节奏控制；在 README 里写明开销，必要时用 `connections=` 限制 |
| 多人输入互相干扰（同时拖动） | 和 VNC 共享模式行为一致，不额外处理 |
| 默认值变化影响依赖"新连接顶掉旧连接"的使用方 | 提供 `share=exclusive`，并在 README 的变更说明里写明 |

## 11. 实现说明和验证结果（2026-11-14）

与计划的差别：
- 消费者 id 直接用会话 id：`Core::register_session` / `unregister_session` 同时登记和注销消费者，不另设守卫。
- QEMU 前端用已初始化客户端的 **sid 集合**（不是计数）判断 `qemu_rdp_has_client()`，没走完连接就断开的客户端不会算错。
- `qemu-options.hx` 里同时补上了 dmabuf 那次漏写的 `gl=on` 说明。
- 客户端连上时发来的初始剪贴板格式列表也算一次“复制”，所以新加入的客户端会成为剪贴板所有者。

单元测试和 e2e 测试（`cargo test`）：
- vds-core：两个消费者各取各的脏区，新消费者只拿全屏，`invalidate` 只影响一个消费者，一个忙时 `wants_frame` 仍为真，head 和消费者先后加入都能对上，剪贴板所有者；
- vds-rdp：`HeldInput` 只释放仍按着的键和鼠标键；e2e 测试 `shared_clients_both_receive_updates`、`connections_beyond_the_limit_are_refused`、`exclusive_client_replaces_previous_one`；
- 原有失败的 5 个测试（`api::tests::version` 和 4 个 e2e）在改动前的 `HEAD` 上同样失败，与本次无关。

QEMU 实测（`research/dmabuf-verify/multi-test.sh`，pxvdi ISO，`gl=on,encoder=vaapi`，`sdl-freerdp`）：

| 场景 | 结果 |
|---|---|
| A（AVC420，1280×800）后接 B（AVC420，1024×768） | A、B 同时在线，A 没被断开；B 没有改 guest 分辨率（只有 A 的一次布局事件）；A 全程只有 1 个 IDR |
| A（AVC420）+ B（legacy），`connections=2`，再连 C | B 在线时显示切到 `readback`，B 离开后回到 `dmabuf`；C 被拒（`too many connections`），A、B 不受影响 |
| `share=exclusive` | B 激活后 A 被断开（`replaced by a new connection`），B 的 `resize-on-connect` 生效 |
| `query-rdp` | 显示 `share`、`connections` 和全部客户端 |

没有在 QEMU 上实测的：断开时释放按键、剪贴板所有者（需要往客户端窗口里打字和复制，只有单元测试覆盖）；Windows 宿主机编译（本机没有 mingw C 工具链）。
