# QEMU fork（C）代码规范

适用于 `../qemu` 中所有与 RDP 相关的改动。基线 QEMU 10.0.10。
总原则：**完全遵守 QEMU 自身的风格**（`qemu/docs/devel/style.rst`），这样以后 rebase 到新版 QEMU 时冲突最少。

## 1. 格式

- 以 `qemu/docs/devel/style.rst` 为准；本节只列最常被违反的几条：
  - 4 空格缩进，不用 Tab；
  - 行宽 80，最多不超过 100；
  - `if`/`for`/`while` 即使只有一行也必须加花括号；
  - 只用 `/* */` 注释，不用 `//`；多行注释：
    ```c
    /*
     * Multi-line comment: first line empty,
     * asterisks aligned.
     */
    ```
  - 变量声明放在块开头。
- 每次改动必须通过：
  ```bash
  ./scripts/checkpatch.pl --branch <base>..HEAD
  ```
  修改的是已有文件中原本不合规的旧代码时，不要顺手重排旧代码。

## 2. 文件布局

| 文件 | 职责 |
|---|---|
| `include/ui/qemu-rdp.h` | 给 `vl.c`、`ui-qmp-cmds.c` 用的钩子；**不包含 `vds.h`** |
| `ui/rdp.h` | 前端内部声明（包含 `vds.h`） |
| `ui/rdp-core.c` | 选项解析、server 生命周期、wakeup/BH/dispatch、QMP/HMP、事件、tls-creds/secret |
| `ui/rdp-display.c` | 2D DisplayChangeListener、共享 surface（`vds_head_attach_shared`）、光标、布局（GL 经 egl-headless） |
| `ui/rdp-input.c` | 键盘、鼠标、锁定键同步 |
| `ui/rdp-clipboard.c` | QemuClipboardPeer：文本和 PNG（`QEMU_CLIPBOARD_TYPE_PNG`） |
| `audio/rdpaudio.c` | audiodev 驱动 `rdp` |
| `ui/rdp-audio.c` | 其它 audiodev 用 `AUD_add_capture` 抓混音输出（`-rdp audiodev=`） |
| `chardev/char-rdp.c` | （未实现）`rdpusb`、`rdpport` |

前端直接编入 QEMU，QAPI 用 `'if': 'CONFIG_RDP'`，不需要模块桩。

- **新功能写在新文件里。** 对已有文件（`system/vl.c`、`qapi/ui.json`、`ui/ui-qmp-cmds.c`、`meson.build` 等）的改动只做"挂钩"：
  加一个 case、一个调用、一个 QAPI 分支，逻辑放到 `ui/rdp-*.c`。
- 在已有文件中插入的代码块用 `CONFIG_RDP` 保护，并尽量紧挨着对应的 spice 代码，方便对照。
- 每个新文件开头写 SPDX 头：
  ```c
  /*
   * QEMU RDP display (libvds frontend)
   *
   * SPDX-License-Identifier: GPL-2.0-or-later
   */
  ```

## 3. 命名

- 函数、静态变量：`rdp_` / `qemu_rdp_` 前缀；QMP 实现按 QAPI 生成的名字（`qmp_query_rdp`）。
- 结构体：`RdpDisplay`、`RdpInput`（QEMU 风格的 CamelCase typedef）。
- 选项名、QAPI 成员：kebab-case（`password-secret`、`one-time`）。
- trace event：`rdp_<主题>_<事件>`。

## 4. QEMU 惯用法（必须遵守）

- 内存：`g_new0` / `g_free`，优先 `g_autofree` / `g_autoptr`。
- 错误：函数用 `Error **errp` 参数返回错误，用 `error_setg()`；调用方用 `ERRP_GUARD()` 或 `&error_fatal` / `&error_abort`。
- 线程：
  - 所有对 QEMU 子系统的调用（`qemu_input_*`、`dpy_*`、`qemu_clipboard_*`、QAPI 事件）必须在主线程、持有 BQL 时进行。
  - libvds 的回调只会在 `vds_server_dispatch()` 中触发，而 `vds_server_dispatch()` 只在 bottom-half 中调用——
    这保证了回调在主线程。**不要**在其它地方调用 `vds_server_dispatch()`。
  - `wakeup` 回调可能在任意线程被调用，里面**只允许**调用 `qemu_bh_schedule()`。
- 鉴权数据：从 `-object secret` 读取（`qcrypto_secret_lookup_as_utf8`），不接受命令行明文密码选项。
- TLS：使用 `tls-creds-x509` 对象，把 PEM 内容交给 libvds（`vds_set_tls`）。
- 日志：见 `logging.md` 第 3 节。禁止 `printf`。

## 5. 与 libvds 的边界

- QEMU 侧只包含 `<vds.h>`，不假设库内部实现。
- 传给 libvds 的像素格式统一为 BGRX（`PIXMAN_x8r8g8b8`，小端）。其它格式在前端用 pixman 转换，或在
  `dpy_gfx_check_format` 中拒绝，让 QEMU 提供兼容的 surface。
- 前端只实现 2D 监听；GL 控制台由 `-display egl-headless` 回读到 2D surface，前端不调用 GL。
- 使用 `vds_head_wants_frame()` 判断是否提交；返回 false 时继续累积脏区，不得丢弃脏区。
- 刷新频率按已初始化的活动客户端决定：连接期间保持 `max-fps` 节拍，无客户端才使用空闲间隔。
  编码器/FrameAck 的短暂背压只跳过提交，不得把刷新间隔降到空闲值；替换客户端时按 sid 区分旧连接的断开事件。

## 6. 构建与验证

先把 libvds 安装到某个前缀（见 `c-abi.md`），再让 pkg-config 能找到 `vds-1.pc`：

```bash
# Linux
PKG_CONFIG_PATH=$PREFIX/lib/pkgconfig ../configure --target-list=x86_64-softmmu --enable-rdp
ninja -C build qemu-system-x86_64
# Windows：必须在 MSYS2 的 UCRT64 shell 中执行（Git Bash 的运行时与 MSYS2 工具混用会 fork 失败）
MSYSTEM=UCRT64 /c/msys64/usr/bin/bash -lc 'cd /home/User/qemu-rdp/qemu/build &&   PKG_CONFIG_PATH=/home/User/qemu-rdp/prefix/lib/pkgconfig:/ucrt64/lib/pkgconfig   ../configure --target-list=x86_64-softmmu --enable-rdp --disable-libcbor && ninja -j4 qemu-system-x86_64.exe'
```
内存紧张的机器用 `-j4` 或更小。

另外：
- `--disable-rdp` 也必须能编译通过（桩实现生效、QMP 命令返回"rdp 未启用"错误）。
- 改动 QAPI 后运行 `make check-qapi-schema`（如可用）。

## 7. 提交

QEMU fork 的提交信息遵循 QEMU 习惯，见 `AGENTS.md` 第 7 节。
