# 日志规范

适用于 libvds（Rust）和 QEMU fork 中的 RDP 前端（C）。目标：运维能用日志定位问题，日志不泄露敏感信息，
日志本身不拖慢热路径。

---

## 1. Rust（libvds）

### 1.1 API

- 只使用 `tracing` 宏：`error!`、`warn!`、`info!`、`debug!`、`trace!`，以及 `#[instrument]` / span。
- 禁止 `println!`、`eprintln!`、`dbg!`、`log::*`（lint 拒绝 `print_stdout`/`print_stderr`/`dbg_macro`）。
- 库**不得**自行安装全局 subscriber。subscriber 只在以下位置初始化：
  - `vds_log_init()`（C ABI，由 QEMU 前端调用一次）；
  - `tools/` 下二进制的 `main()`。

### 1.2 级别语义

| 级别 | 含义 | 举例 | 频率上限（单会话，稳态） |
|---|---|---|---|
| `ERROR` | 功能丢失，需要运维介入 | 监听失败、H.264 编码器打开失败、会话因内部错误中断 | 偶发 |
| `WARN` | 功能降级但仍在运行，或对端行为异常 | 编码回退到 RemoteFX、客户端发来非法 PDU、持续背压丢帧 | ≤ 1 条/10 秒（用限频宏） |
| `INFO` | 生命周期事件 | 开始监听、客户端连接/认证成功/认证失败/断开（含原因）、选定编码、分辨率变化、凭据更新/过期 | ≤ 1 条/秒 |
| `DEBUG` | 协商与配置细节 | 能力集、通道打开/关闭、码率调整 | 不限，但不得每帧 |
| `TRACE` | 每帧/每包 | 脏区、帧大小、编码耗时、ack | 不限 |

**规则：每帧、每包、每个输入事件执行的代码只能用 `trace!`。**
重复出现的 `warn!` 必须用 `vds_core::warn_ratelimited!(...)`（参数同 `warn!`），同一调用点 10 秒内只输出一次，
并附带 `suppressed` 字段（被抑制的次数）。

### 1.3 消息与字段

- 消息：**英文、小写开头、不加句号**，描述发生的事件，不拼接变量：
  ```rust
  // 正确
  info!(peer = %addr, user = %user, codec = %codec, "client authenticated");
  warn!(error = %e, "H.264 encode failed, requesting idr");
  // 错误
  info!("Client {} authenticated with codec {}.", addr, codec);
  ```
- 变量数据一律放在**结构化字段**里。字段名 snake_case，带单位后缀。
- 错误字段：`error = %e`（Display）；需要完整错误链时用 `error = ?e`。

标准字段名（新字段先查这张表，没有再新增并补充到这里）：

| 字段 | 类型 | 含义 |
|---|---|---|
| `sid` | u64 | 会话 ID（服务器进程内递增） |
| `peer` | SocketAddr | 客户端地址 |
| `user` | str | 认证用户名（不是密码） |
| `vm` | str | VM 名称/UUID（由 QEMU 传入） |
| `head` | u32 | 显示器编号 |
| `codec` | str | `avc420` / `remotefx` / `bitmap` |
| `w`, `h` | u32 | 宽、高（像素） |
| `fps` | u32 | 帧率 |
| `bitrate_kbps` | u32 | 码率 |
| `rtt_ms` | u32 | 往返时延 |
| `frame_id` | u32 | EGFX 帧 ID |
| `bytes` | usize | 数据大小 |
| `elapsed_us` | u64 | 耗时（微秒） |
| `in_flight` | u32 | 未确认帧数 |
| `channel` | str | 虚拟通道名 |
| `reason` | str | 断开/失败原因 |
| `error` | — | 错误 |

### 1.4 Span

- 每个客户端连接一个 span：`session{sid, peer}`；认证后记录 `user`。
- 每个 head 的编码线程：`encoder{sid, head}`。
- span 名称用 snake_case 名词，不用动词。

### 1.5 输出格式

由 `vds_log_init()` 配置（QEMU 侧通过 `-rdp log=...,log-format=...`，或环境变量）。

- 过滤：环境变量 `VDS_LOG`（`tracing_subscriber::EnvFilter` 语法），默认 `info`。`-rdp log=` 优先于环境变量。
- 输出：stderr（QEMU 的 stderr 由 libvirt / PVE 收集）。
- 格式：`VDS_LOG_FORMAT=text|json`，默认 `text`。

text 格式（固定，不得自定义）：
```
2026-10-09T08:45:00.123456Z  INFO session{sid=3 peer=10.0.0.5:51234}: vds_rdp::session: client authenticated user="u-7f3a" codec="avc420" w=1920 h=1080
2026-10-09T08:45:12.004211Z  WARN session{sid=3 peer=10.0.0.5:51234}:encoder{sid=3 head=0}: vds_rdp::egfx: frames dropped due to backpressure dropped=37 suppressed=4
```
- 时间：UTC，RFC 3339，微秒精度。
- 每行一条事件，不输出 ANSI 颜色（`with_ansi(false)`）。

json 格式：每行一个 JSON 对象，字段为 `timestamp`、`level`、`target`、`span`、`spans`、`fields`（tracing-subscriber JSON 格式默认结构）。

## 2. 敏感数据（Rust 和 C 都适用）

**任何级别都不得记录：**
- 密码、一次性票据、Pre-Connection Blob 内容、NTLM/CredSSP 消息内容
- TLS 私钥、证书私钥路径之外的内容
- 剪贴板内容（只能记录格式和字节数）
- 按键扫描码、Unicode 字符（只能记录事件数量）
- 鼠标坐标序列（`trace` 中可记录单个事件的坐标用于调试，但不得在 INFO 及以上记录）

例外只有一个：**默认关闭**的调试目录（第 5 节）在显式打开后，可以按 Q-3 的规则把输入事件写进
`input.csv`——字符键仍然只记成 `char`，不记具体键码。除此之外没有例外。

可以记录：用户名、客户端地址、VM 名称、编码参数、统计数字。

`Secret<T>` 打印为 `<redacted>`。代码审查时发现泄露视为严重缺陷。

## 3. C（QEMU fork 中的 RDP 前端）

- 用户可见的错误（配置错误、启动失败）：`error_setg(errp, ...)` / `error_report()` / `warn_report()`。
  QEMU 风格：**小写开头、不加句号、不加换行**：`error_report("rdp: failed to listen on %s:%d", addr, port);`
- 消息统一以 `rdp: ` 为前缀。
- 调试与热路径：trace events，定义在 `ui/trace-events`、`audio/trace-events`、`chardev/trace-events`：
  ```
  # rdp-display.c
  rdp_display_damage(int head, int x, int y, int w, int h) "head=%d x=%d y=%d w=%d h=%d"
  rdp_display_submit(int head, int rects) "head=%d rects=%d"
  ```
  - 事件名：`rdp_<文件主题>_<事件>`，全小写。
  - 格式串：`key=value` 空格分隔，键名与上面的标准字段表一致。
- 禁止：`printf`、`fprintf(stderr, ...)`、`DPRINTF`、`qemu_log` 用于调试输出。
- libvds 的日志直接写 stderr，QEMU 前端不转发、不重复记录同一事件。

## 4. 指标

统计数据（fps、码率、RTT、丢帧）通过 QMP `query-rdp` 提供，不要靠周期性 INFO 日志输出。
需要周期性观察时用 `DEBUG` 且间隔 ≥ 10 秒。

## 5. 调试目录（`debug-dir`）

stderr 上的日志混在 QEMU 的输出里，排查"客户端鼠标不对""组合键没反应""画面糊"这类问题时不够用。
打开调试目录后，libvds 为**每个连接**在磁盘上留一份机器可读的诊断记录。默认关闭。

### 5.1 打开与关闭

| 方式 | 位置 | 说明 |
|---|---|---|
| `-rdp debug-dir=<path>` | QEMU 命令行 | 目录必须显式给出。`debug-dir=on` 表示默认目录：Linux/macOS `${TMPDIR}/vds`，Windows `%TEMP%\vds` |
| `-rdp debug-input=on` | QEMU 命令行 | 对应 `VDS_DEBUG_INPUT`，写 `input.csv` |
| `-rdp debug-video=on` | QEMU 命令行 | 对应 `VDS_DEBUG_VIDEO`，写 `video-head<N>.h264` |
| QMP `rdp-set-debug` | 运行时 | 参数 `input` / `video` / `pdu`，只改标志位，不改目录 |
| `vds_server_set_debug()` | C ABI | 同上，`[any-thread]`；没配目录时返回 `VDS_E_STATE` |
| `VdsConfig::debug_dir` / `debug_flags` | C ABI | 服务器创建时的初值 |

- **目录只在服务器创建时确定**（`vds_server_new()` / `Core::new()`），运行中不能换；
  **标志位可以随时改**，改完立即生效，不需要重连。
- 目录不存在时创建。**创建失败（权限、路径非法）会让服务器创建失败**，错误信息为
  `cannot create debug directory <path>: <原因>`——宁可启动失败，也不要静默地没有调试数据。
- 同一进程内只有**第一次** `configure()` 能设定目录，之后调用只更新标志位。
- `debug-flags` 的第 3 位（`VDS_DEBUG_PDU`，即 `pdu`）目前只是占位，没有实现（留给 T-D2）。

### 5.2 目录布局

```
<debug-dir>/<vm>-<pid>/
├── session-3-20261010-140312/
│   ├── session.log
│   ├── client.json
│   ├── frames.csv
│   ├── pointer.csv
│   ├── input.csv
│   ├── audio.csv
│   └── video-head0.h264
└── session-4-20261010-142001/
```

- `<vm>` 是 `VdsConfig::vm_name`，`<pid>` 是宿主进程号，这一层让多次运行的记录不会混在一起。
- 会话目录名带 UTC 时间戳；同一秒内重名时追加序号。
- 文件在会话开始时创建，**会话结束时不删除**，由使用者自行清理。

### 5.3 文件

| 文件 | 内容 | 何时写 |
|---|---|---|
| `session.log` | 该会话的日志，过滤器固定为 `vds_core=debug,vds_rdp=debug,ironrdp_server=debug,ironrdp_egfx=debug`，**与 stderr 的过滤级别互不影响**。首行是 `# session <sid> started <RFC3339>`，末行是 `# session <sid> finished after <n> ms` | 始终 |
| `client.json` | 客户端与协商结果（见 5.4） | EGFX 协商完成后先写一次，会话结束时按最终状态重写 |
| `frames.csv` | 每帧的耗时与大小 | 始终 |
| `pointer.csv` | 每次指针更新 | 始终 |
| `input.csv` | 客户端键盘与鼠标事件 | 仅 `debug-input=on` |
| `audio.csv` | 播放与音量事件 | 始终 |
| `video-head<N>.h264` | 原样 Annex-B 码流，可用 `ffplay` 直接回放 | 仅 `debug-video=on` |

**`client.json`** 记录：`sid`、`peer`、`user`、`vm`、`started`、`collected_ms`、`transport`、
`avc420`、`gcc`（键盘 layout/type、IME 文件名）、`desktop`（宽高）、`egfx`（客户端 advertise 与最终确认的能力集）、
`channels`（本次注册的虚拟通道）、`unavailable`。

`unavailable` 列出**当前拿不到**的字段——客户端 build/name/product id、键盘子类型、GCC core 标志、
客户端能力集全集、客户端请求的 DVC 列表。这些要么被 `ironrdp-server` 吞掉，要么根本不上浮，
只能等 IronRDP 本地补丁（T-X3）之后补上。**这里必须如实列出，不能假装记录到了。**

**CSV 列**（时间单位一律是毫秒，从会话开始算，UTC 起点写在 `session.log` 的第一行 `# session <sid> started <RFC3339>`）：

```
frames.csv   time_ms,head,frame_id,kind,bytes,qp,regions,region_area,age_us,convert_us,encode_us,handoff_us,in_flight,ack_us
pointer.csv  time_ms,kind,width,height,hot_x,hot_y,pdu,and_mask_len,dropped
input.csv    time_ms,kind,source,scancode,extended,down,value
audio.csv    time_ms,kind,block,samples,bytes,left,right,mute,client_ts,wave_ts
```

- `frames.csv` 的行在收到该帧的 ack 之后才写（这样才有 `ack_us`）；超过 2 秒仍未 ack 的帧也会写出来，
  `ack_us` 留空。**因此 ack 乱序或丢失不会丢行。**
- `frames.csv` 的四个耗时列（T-V2）按一帧经过的顺序排：

  | 列 | 起点 | 终点 | 覆盖了什么 |
  |---|---|---|---|
  | `age_us` | 宿主把这块 damage 交给 `vds_head_submit` | 视频线程开始编码这一帧 | QEMU 的刷新周期与脏页扫描、`Head::submit` 的拷贝、帧率限速（`1000/max_fps`）、线程唤醒 |
  | `convert_us` | 开始颜色转换 | 转换完成 | `I420Frame::convert`（BGRX→I420）；`encoder=vaapi` 时是把脏区复制进上传缓冲（转换在 GPU 上，计入 `encode_us`） |
  | `encode_us` | 开始编码 | 编码器返回 | 一次 `OpenH264Encoder::encode`（或 VA-API 编码） |
  | `handoff_us` | 编码结束 | EGFX PDU 进入会话事件循环的队列 | 组装 PDU、DVC 封装、`send_event` |
  | `ack_us` | 上面全部结束 | 收到该帧的 `FrameAcknowledge` | **写 socket、客户端解码、ack 的返程**（三项在 libvds 里分不开） |

  `age_us` 从**这一帧携带的最老的那块 damage** 算起：宿主先提交两次、之后才编码一帧时，画面从第一次提交起就不新了。
  `kind=refine` 的帧没有新的 damage，`age_us` 为 0。
  `convert_us`/`encode_us`/`handoff_us` 不受“有没有 damage”影响，永远是这一帧自己的耗时。
  一帧从 damage 到被确认的时间上限是这五列之和；客户端把它画到屏幕上的那一段**没有记录**，只能在真实客户端上测。
- `pointer.csv` 的 `kind`：`shape`（形状更新）/ `transparent`（隐藏光标）；`pdu`：`color` / `rgba`。
  `and_mask_len` 是随形状发出的 AND mask 长度；`dropped` 留给"被 IronRDP 丢掉"的情况（见 I-2）。
- `input.csv` 的 `kind`：`char` / 具名键（`ctrl`、`enter`、`delete`…）/ `lock-sync` / `move` / `rel` /
  `button` / `wheel` / `wheel-h` / `wheel-v` / `other`；`source`：`fast-path` / `ainput` / `unicode`。
  **鼠标坐标永远不写进 `input.csv`**，只写事件类型。
- `audio.csv` 的 `kind`：`wave`（发出的 Wave PDU）/ `volume` / `confirm`。
  `wave` 的 `wave_ts` 是这一波的**媒体时间戳**（按采样数推算的捕获时刻，见 T-A1），
  `confirm` 的 `client_ts` 是客户端回显的时间戳（MS-RDPEA 2.2.3.4 里是"播完 + 保持时间"），
  **不是**严格的往返时延。两者配对（同一个 `client_ts` 与 `wave_ts` 相差保持时间，通常只有几十毫秒）
  可以算出：保持时间 = `(client_ts - wave_ts) & 0xffff`；往返 ≈ 两行 `time_ms` 之差 − 保持时间。
  `wave` 行的 `block` 留空，波按 block 号从 0 递增发出，所以第 n 个 `wave` 行的 block 是 n−1。
  同一时刻已经打好的波会合并成一个 Wave 再发（`samples` 列是合并后的长度，10 ms 的整数倍），
  因为 `ironrdp-server` 一批只保留最新 4 个波、更早的直接丢掉；开始播放（以及静音超过 100 ms 之后）
  先攒 60 ms 再一起发，给客户端留缓冲。相邻两行 `wave` 的 `wave_ts` 之差应等于前一行的时长，
  多出来的部分说明中间有音频没发出去：客户端积压超过阈值被丢弃（T-A1 第 3 条）。
  前者在 `RUST_LOG=vds_rdp=debug` 的 stderr 上有对应的 `discarding late audio`；那条日志出在
  QEMU 的音频回调线程上，不在会话 span 里，所以**不会**进 `session.log`。
- `video-head<N>.h264` 每个 head 一个文件，超过 200 MB 后**从头覆盖**（丢掉最老的帧），
  保证单个文件有界，不会写满磁盘。

### 5.4 `input.csv` 的脱敏规则（Q-3）

`input.csv` 是唯一会记录输入内容的文件，规则**由人类拍板**，实现不得放宽：

- **只记录修饰键和非字符键**：Ctrl、Alt、Shift、Win、Del、End、Home、PgUp/PgDn、Insert、
  方向键、F1–F12、Esc、Tab、Enter、Backspace、CapsLock、NumLock、ScrollLock、PrintScreen、
  小键盘的 Enter 和 `/`。这些记 `kind` + 扫描码 + `extended` 标志 + 按下/抬起。
- **字符键（字母、数字、标点、空格……）只记成 `kind=char`**，`scancode`、`down`、`value` 全部留空。
  也就是说，从 `input.csv` **无法**还原出敲了哪些字符，只能知道"有字符键输入"。
- `source=unicode` 的事件（`UnicodePressed` / `UnicodeReleased`）同样只记成 `char`，不记 Unicode 码点。
- **只有 `debug-input=on` 时才写这个文件。** 没有打开时，输入路径上一次磁盘操作都没有。
- 扫描码的解释必须带 E0 前缀：`0x53` 带 E0 是 `delete`，不带 E0 是小键盘的小数点，属于字符键。
- `sas-hotkey` 把 Ctrl+Alt+End 翻译成 Ctrl+Alt+Del（默认）时，
  `input.csv` 记的是**翻译后**的 `delete`——注入 guest 的就是它，排查"按下去了没反应"要看这个。

`input.csv` 里的鼠标部分只记"发生了什么"（移动/按下/抬起/滚轮、来源是 fast-path 还是 ainput），
不记位置。这一条与第 2 节一致：坐标属于敏感数据。

### 5.5 性能

- 关闭时（默认）：输入路径一次原子读（判断 `debug-input` 是否打开），其余路径一次 `Option`
  判空。不分配、不格式化、不取锁。唯一的固定开销是每帧两次 `AtomicU32` 操作，用于维护在途帧数，
  那属于流控，与调试无关。
- 打开时：CSV 用 `BufWriter` 追加写，每行一次 `write!`，**写完立刻 flush**。每帧的
  `video-head<N>.h264` 也立刻 flush。也就是说调试目录里的内容**在会话还在跑的时候就是可读的**，
  进程被强杀（`taskkill`、宿主机断电）时已经记下的部分不会留在缓冲区里丢掉。
  代价是每个记录一次系统调用，但只有打开调试时才付。
- 实测（`vds-bench debug`，release，Windows/WHPX 开发机，i5 级 CPU）：

  ```
  $ ./target/ucrt64/release/vds-bench.exe debug
  debug off (no directory, 200000 records)
         1.4 ns/record       0 allocations
  debug on  (frame+pointer+input, 20000 records, each row flushed)
      8140.8 ns/record   40001 allocations  1969537 bytes written
  ```

  `vds-bench debug` 用的就是这个仓库里分配器计数（每个 `alloc`/`realloc` 记一次）和真实的记录调用点形状，
  所以"关闭时零分配"是被量出来的，不是推断的。三次重复的 `off` 结果都是 `1.4 ns / 0 allocations`。
  打开时每次记录约 8 µs，几乎全部是 flush 那一次系统调用；一行约 98 字节。
- **末行的含义**：会话正常结束时（客户端断开、`vds_server_stop()` 等）会写
  `# session <sid> finished after <n> ms`，它是"文件齐了"的标志。这一行由会话结束的代码写出，
  **不依赖对象析构**——调试目录对象会被 IronRDP 的任务树多持有很久，
  等析构可能永远等不到。析构里也会补写一次，作为兜底。
- `video-head<N>.h264` 写的是编码器已经产出的字节，**不额外拷贝编码路径上的数据**（`debug-video` 关闭时不写）。

### 5.6 敏感性与使用注意

调试目录里可能有：客户端 IP 与用户名、主机名、桌面尺寸、完整码流。
它**和日志同等敏感**，不要直接贴到公开 issue 里。字符键已按 5.4 脱敏，但 `client.json` 里的
用户名和地址、`video-head*.h264` 里的画面内容仍然会泄露隐私，分享前先人工过一遍。
