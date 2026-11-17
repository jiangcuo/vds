# vds-connect — 设计与约束

改本工具时先读根目录 `AGENTS.md`；改完在同一次改动里更新本文件。

## 职责

打开 PVE「控制台 → RDP」下载的 `.vds` 文件（`application/x-vds`，`key=value` 格式，字段见 `src/main.rs`
顶部），启动 FreeRDP 连接：

- `transport=tcp`：FreeRDP 直接连 `address:port`；
- `transport=websocket`：在 `127.0.0.1` 上开一个本地端口，把 RDP 字节流经 `wss://` 转发（`src/tunnel.rs`，
  rustls + 证书指纹 `ws_fingerprint` 校验），FreeRDP 连这个本地端口。

RDP 证书由 FreeRDP 按 `rdp_fingerprint` 校验（`/cert:fingerprint:sha256:`）。

## 模块

| 文件 | 内容 |
|---|---|
| `main.rs` | 参数（路径或 `file://` URI）、`.vds` 解析、日志、`connect()` 主流程 |
| `finder.rs` | 找 FreeRDP 客户端 |
| `launcher.rs` | 拼 FreeRDP 参数并等它退出 |
| `tunnel.rs` | WebSocket 隧道 |
| `macos.rs` | 只在 macOS 编译：`NSApplication` 委托接收 Finder 打开的文件 |

FreeRDP 查找顺序：`FREERDP_PATH`；Windows 是本程序所在目录；macOS 是本程序旁边的
`MacFreeRDP.app/Contents/MacOS`（安装包自带的），再 `/opt/homebrew/bin`、`/usr/local/bin`；其它系统
`/usr/local/bin`、`/usr/bin`；最后 `PATH`。

日志：Windows `%LOCALAPPDATA%\vds-connect\vds-connect.log`，macOS `~/Library/Logs/vds-connect/vds-connect.log`，
其它系统临时目录下的 `vds-connect.log`；从终端运行时同时输出到 stderr。

## macOS 接收文件

Finder 双击文件时不会把路径放进命令行参数，而是启动 app 后发 Apple Event，由 `NSApplication` 交给委托的
`application:openURLs:`。所以没有命令行参数时，`macos::run` 跑事件循环：每收到一个文件就在新线程里跑一次
`connect()`，事件循环继续，运行中再双击的文件（LaunchServices 发给已运行的 app）也会各开一个会话；最后一个
会话结束时以它的 FreeRDP 退出码退出。启动时带的文件在 `applicationDidFinishLaunching:` 之前到达，到那时还
没收到文件就报错退出。app 用 Accessory 激活策略，不占 Dock 和菜单栏。

`unsafe` 只出现在 `macos.rs`（objc2 的类声明和 AppKit 调用），每处都写 `// SAFETY:`。

## 工具链

独立 crate，不在 libvds workspace 里，固定 Rust 1.77.2（最后一个支持 Windows 7 的版本）。目标：
`x86_64-pc-windows-msvc`（静态 CRT，不需要 VC 运行库）、`aarch64/x86_64-apple-darwin`、
`x86_64/aarch64-unknown-linux-musl`（静态）。依赖见 `Cargo.toml`，新增依赖要人类批准。

## 打包

`.github/workflows/vds-connect.yml` 构建，tag 时发到 GitHub release：

| 包 | 工具 | 构建机 | FreeRDP |
|---|---|---|---|
| deb（amd64、arm64） | cargo-packager | ubuntu-24.04(-arm) | 依赖 `freerdp3-sdl \| freerdp-sdl \| freerdp2-x11` |
| rpm（x86_64、aarch64） | cargo-generate-rpm | 同上 | 依赖 `freerdp` |
| Windows `*-setup.exe`（NSIS，x64） | cargo-packager | windows-latest | 自带 freerdp-nightly-windows 的 `sdl-freerdp.exe`（win64 vs2017，单文件），装在 `vds-connect.exe` 旁边 |
| macOS `.dmg`（universal） | cargo-packager 出 `.app`，`hdiutil` 出 dmg | macos-14 | 自带 `jiangcuo/freerdp-macos` 的 `MacFreeRDP.app`（按 `.sha256` 校验），放在 `Contents/MacOS/`；需要 macOS 15 |

配置在 `Cargo.toml` 的 `[package.metadata.packager]` 和 `[package.metadata.generate-rpm]`。`.vds` 关联：

- deb / rpm：`packaging/vds-connect.desktop`（`MimeType=application/x-vds`，`NoDisplay`，不进菜单）+
  `packaging/vds-connect.xml`（shared-mime-info，`*.vds`），由系统的 dpkg/rpm 触发器更新数据库；
- Windows：NSIS 写注册表 `.vds` 关联；
- macOS：`Info.plist` 的 `CFBundleDocumentTypes`。

没有 Apple 开发者证书，app 只做临时签名（`codesign -s -`），首次打开要右键「打开」放行。

图标源文件是 `packaging/icons/vds-connect.svg`，PNG/`.ico`/`.icns` 不入库，由 `packaging/icons/render.sh`
（rsvg-convert、icotool、png2icns）生成到 `packaging/icons/out/`，CI 的 `icons` job 负责运行。PVE 网页里
RDP 菜单的 16×16 图标也取自这里。

本地打 Linux 包：

```bash
packaging/icons/render.sh
cargo build --release --target x86_64-unknown-linux-musl
cargo packager --release --target x86_64-unknown-linux-musl --formats deb
cargo generate-rpm --target x86_64-unknown-linux-musl
```

## 约束

- 密码只经 FreeRDP 的 `/p:` 参数传递，不写日志；`.vds` 解析错误只报行号。
- 平台代码用 `#[cfg]` 隔开：Windows 的子系统和 `CREATE_NO_WINDOW`，macOS 的 `macos.rs` 和日志目录。
