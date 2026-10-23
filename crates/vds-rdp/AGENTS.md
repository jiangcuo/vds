# vds-rdp — 设计与约束

改本 crate 时先读根目录 `AGENTS.md`；改完在同一次改动里更新本文件。

## 职责

基于 IronRDP 的 RDP 服务端后端。只通过 `vds_core::Core` 与宿主交互，不知道 QEMU 的存在。
协议细节以微软 Open Specifications 为准，注释里写章节号（如 `MS-RDPEGFX 2.2.4.4`）；
IronRDP 的 API 以 `Cargo.lock` 锁定的 rev 源码为准。

## 模块

| 模块 | 内容 |
|---|---|
| `lib` | `RdpServer`：在独立线程上跑 tokio runtime + `LocalSet`（IronRDP 的连接 future 不是 `Send`） |
| `manager` | 监听与接受连接、持有会话；最多 `MAX_SESSIONS`（8）个，停止时等待 `SHUTDOWN_GRACE` |
| `session` | 一个客户端连接：每个连接新建一个 `ironrdp_server::RdpServer`，保证上一个客户端排队的事件（尤其 EGFX 帧）不会漏给新客户端；`SessionShared` 是会话内各后端共享的状态（事件发送端、传输方式、剪贴板状态等），实现 `SessionSink` |
| `display` | `RdpServerDisplay`：编码选择（`vds_core::codec::select`，等待 EGFX 的宽限期 `egfx_grace_ms`）、legacy 路径（RemoteFX/bitmap，宽度按 4 对齐）、光标更新（形状发 RGBA 指针，无形状或隐藏时发透明指针）、DisplayControl 布局请求 |
| `egfx` | EGFX AVC420：每会话一个视频线程处理所有显示头；每个头一个编码线程（`EncodeWorker`），两块图片缓冲（`PICTURE_BUFFERS`），帧 N+1 的转换和帧 N 的编码重叠，帧按顺序发出；`FrameAcknowledge` 控制节奏，最多 `MAX_FRAMES_IN_FLIGHT`（3）帧在途；编码器按配置选 openh264 或 VA-API（不可用时回退）；`egfx-mixed` 时静止区域改发 ClearCodec 无损图块；没有图块在途时才发图块，保证 ClearCodec 序号连续 |
| `blocks` | 把刷新分成"视频区"和"静止区"（64×64 块，最近 8 次刷新中变化 ≥3 次算视频）|
| `clearcodec` | ClearCodec 残差层编码（MS-RDPEGFX 2.2.4.1）|
| `input` | 客户端输入 → `ClientEvent`：扫描码（E0 前缀）、绝对/相对鼠标、滚轮（fast-path 每格 120，ainput 的值先除以 `0x10000`）、Ctrl+Alt+End→Ctrl+Alt+Del（`sas-hotkey`）、`input.csv` 记录 |
| `sound` | rdpsnd：48 kHz/16 位/立体声；宿主的 PCM 打包成 10 ms 的 wave，环形缓冲预先分配；新流先攒 60 ms 预缓冲，100 ms 没有数据就重新预缓冲；同一批就绪的 wave 合并成一个事件发出（IronRDP 每批只保留 4 个 wave，逐个发会被丢掉，表现为杂音）；积压超过 200 ms 丢弃 |
| `clipboard` / `clipboard_image` | cliprdr：文本（`CF_UNICODETEXT`）和图片。向客户端提供注册格式 "PNG"、"image/png" 和 `CF_DIB`；从客户端优先取 PNG 格式，否则把 `CF_DIB` 转成 PNG。图片转换在短命线程上做，不阻塞宿主主线程或会话任务 |
| `client_info` | 连接时客户端信息与已打开的通道，写进 `client.json` |
| `tls` | `TlsIdentity`：证书链、私钥、CredSSP 用的公钥；未配置证书时用 rcgen 生成自签证书 |
| `error` | 错误类型 |

## 线程

RDP 线程（tokio `LocalSet`）跑所有连接；每会话一个视频线程；每头一个编码线程；剪贴板图片转换用临时线程。
CPU 密集的工作（颜色转换、编码、图片转换）一律不在 async 任务里做。

## 约束

- 编码协商顺序固定 AVC420 → RemoteFX → bitmap，不做 AVC444，不开 QOI/QOIZ。
- 不在日志里记录按键内容、剪贴板内容；只记长度和格式。
- 改动 EGFX 或 legacy 路径时，两个传输都要考虑：分辨率变化在 legacy 下是 Deactivation-Reactivation，在 EGFX 下是重建 surface。
- 光标：客户端在本地画光标，所以宿主给出形状就一直发形状；只有宿主明确隐藏或没有形状时才发透明指针。
- 已知限制：IronRDP 网页客户端不协商 EGFX，只走 legacy；Pause 键在 IronRDP 里丢了 E1 前缀，收到的是 NumLock。
