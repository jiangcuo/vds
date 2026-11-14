# GL 画面 dmabuf 零拷贝设计（草案）

状态：已实现（libvds + QEMU `rdp` / `rdp-11` 两条分支），尚未提交。2026-11-12 人类批准放开 D-004（仅限 VA-API 路径）。
`AGENTS.md` §2、`docs/spec/*`、`README.md` 和各 crate 的 `AGENTS.md` 已同步更新。实现与本设计的差别见第 12 节。

## 1. 背景

virgl（`virtio-gpu-gl`）画面现在的路径：

```
virgl 纹理 ─blit─▶ egl-headless FBO ─glReadPixels(整帧, 主线程)─▶ 2D surface
   ─vds_head_submit(主线程拷贝)─▶ libvds 影子缓冲 ─BgrxFrame 拷贝─▶ av_hwframe_transfer_data(上传) ─▶ VPP ─▶ h264_vaapi
```

用 VA-API 编码时，一帧画面要在 GPU 和 CPU 之间来回两次，CPU 上再拷两次。`glReadPixels` 在 QEMU 主线程上同步等 GPU。virgl 每帧报的脏区都是全屏，所以这些拷贝每次都是整帧。

已验证的事实（`research/dmabuf-verify/`，AMD Renoir + Mesa 25.0 radeonsi + QEMU 10.0）：

- `egl_get_fd_for_texture()` 导出的 fd 是 GPU 缓冲区，modifier 是 `DRM_FORMAT_MOD_INVALID`（隐式布局）；
- radeonsi 的 VA-API 用 `DRM_PRIME_2` 加 `MOD_INVALID` 可以导入它，VPP（RGB→NV12）加 `h264_vaapi` 提交，1280×800 下平均每帧约 0.35 ms，CPU 不碰像素；
- SeaBIOS、OVMF（`virtio-gpu-gl-pci` 和 `virtio-vga-gl` 都测了）和 Linux 桌面阶段都是这样；
- `y0_top=1` 表示内存里的行是从下往上存的，要垂直翻转；`y0_top=0` 表示已经是正的。同一个会话里，fourcc 和 `y0_top` 都会随 scanout 变化；
- virgl 阶段的 `dpy_gl_update` 每次都是全屏；固件（2D）阶段的脏区是精确的。

## 2. 目标与非目标

目标：
- 在 Linux 宿主机上，"GL 控制台 + VA-API 编码 + EGFX AVC420"这种组合下，画面从 guest 纹理到 H.264 码流全程留在 GPU 上；
- QEMU 主线程不再执行 `glReadPixels`，也不再做整帧拷贝。

非目标：
- NVIDIA（VA-API 不能编码，NVENC 被禁止）：探测失败时自动回读；
- Windows 宿主机：没有 dmabuf，行为不变；
- 不做 GPU 上的脏区检测（virgl 仍按全屏编码，静止画面由码控压到很小）；
- 不改 openh264、RemoteFX、bitmap 的编码方式，它们照旧拿 CPU 像素。

## 3. 决策变更（需要写进 AGENTS.md）

`AGENTS.md` §2 的这一条：

> libvds 的显示入口只收 CPU 像素：GL 画面由 QEMU 前端回读到 CPU 后再交给库；唯一例外是可选的 VA-API 编码器打开渲染节点。

改为：

> libvds 的显示入口收 CPU 像素；另外在 Linux 上、`vaapi` feature 打开且配置了 `encoder=vaapi` 时，可以收 dmabuf（`vds_head_attach_dmabuf`），只在 `vds-encode::vaapi` 里通过 VA-API 访问它（导入、VPP、必要时回读）。libvds 不调用 EGL/GL。

`docs/spec/c-qemu-style.md:79` 的这一条：

> 前端只实现 2D 监听；GL 控制台由 `-display egl-headless` 回读到 2D surface，前端不调用 GL。

改为：

> `-rdp gl=on` 时，前端自己持有 EGL 上下文（`ui/rdp-gl.c`），GL 控制台导出 dmabuf 交给 libvds；libvds 拒收时由前端回读。不开 `gl=on` 时行为不变（仍可用 egl-headless）。

## 4. 总体数据通路

```
                    QEMU 主线程                                   libvds 编码线程
virgl 纹理 ──egl_get_fd_for_texture──▶ fd ──vds_head_attach_dmabuf──▶ dup(fd)，代号 +1
vfio/blob QemuDmaBuf ─────────────────▶ fd ─┘                          │
dpy_gl_update ─glFlush─▶ vds_head_damage ──────────────▶ take_frame ──▶ 按代号缓存 VA surface（DRM_PRIME_2 导入）
                                                                       └▶ VPP（翻转、RGB→NV12）──▶ h264_vaapi
```

需要 CPU 像素时（openh264 回退、RemoteFX/bitmap 客户端、ClearCodec 瓦片），libvds 在自己的线程上用 VA 回读（见 6.4）。QEMU 只有一条路径。

## 5. QEMU 侧（`ui/rdp-gl.c`，新文件）

### 5.1 参数与初始化

- 新增 `-rdp gl=on|off`，默认 off；沿用已有的 `rendernode=`，QEMU 的 EGL 和 libvds 的 VA 用同一个节点（已经通过 `cfg.render_node` 传给 libvds）。没有指定时，取 `egl_rendernode_init` 实际打开的节点再传给 libvds，避免两边各自挑了不同的 GPU。
- **初始化时机**：`qemu_rdp_init()` 是在 `qemu_init_displays()` 里调用的，那时设备已经创建好了，而 `virtio-gpu-gl` 在 realize 时就要求 `display_opengl` 已经设置（`hw/display/virtio-gpu-gl.c:136`）。所以要在 `qemu_create_early_backends()` 里加一个挂钩 `qemu_rdp_early_init()`：解析 `gl=`，调用 `egl_init(rendernode, DISPLAY_GL_MODE_ON)`。spice 的 `gl=on` 也是这样做的（`ui/spice-core.c:839`）。vl.c 里只加这一个挂钩。
- 一个控制台只能有一个 GL 上下文，而 egl-headless 只接受 2D 监听（`ui/egl-headless.c:187`）。所以 `gl=on` 时如果已经有别的 GL 显示（比如同时指定了 `-display egl-headless`），**不报错**，而是 `warn_report` 一次，然后退回 2D 监听，挂在 egl-headless 后面（也就是今天的行为）。
- 只在 `CONFIG_OPENGL && CONFIG_GBM` 下编译。其他平台指定 `gl=on` 时 `warn_report` 后当作 `gl=off`，不报错退出。
- 回退的完整规则见第 7 节。

### 5.2 每个控制台的 GL 监听

`gl=on` 时，`rdp-display.c` 给 GL 控制台注册另一张 ops 表（放在 `rdp-gl.c`），并用 `qemu_console_set_display_gl_ctx()` 挂上 RDP 自己的 `DisplayGLCtx`：

| 回调 | 行为 |
|---|---|
| `dpy_gl_ctx_create/destroy/make_current` | 和 dbus 一样：先 `eglMakeCurrent(qemu_egl_rn_ctx)`，再 `qemu_egl_create_context` |
| `dpy_gl_ctx_create/update/destroy_texture` | 留空（NULL）：2D surface 不上传成纹理，继续走现有的 CPU 路径 |
| `dpy_gfx_switch/update`、`dpy_refresh` | 复用 `rdp-display.c` 现有的 2D 逻辑（固件和 VGA 阶段的 `attach_shared`/`submit`，脏区精确） |
| `dpy_gl_scanout_texture` | `egl_get_fd_for_texture()` → `vds_head_resize` + `vds_head_attach_dmabuf`；失败就进入回读模式（5.4） |
| `dpy_gl_scanout_dmabuf` | 直接取 `qemu_dmabuf_get_fd/width/height/stride/fourcc/modifier/y0_top`，其余同上 |
| `dpy_gl_release_dmabuf`、`dpy_gl_scanout_disable` | `vds_head_detach`（新接口，见 6.1），退回 2D 路径 |
| `dpy_gl_update(x,y,w,h)` | `glFlush()`；有客户端要帧时调用 `graphic_hw_gl_block(con, true)`，然后 `vds_head_damage`（5.3） |
| `dpy_gl_cursor_dmabuf` / `dpy_gl_cursor_position` | 光标很小：导入纹理后用 `egl_fb_read` 读回，再 `vds_cursor_define`（和 dbus 的做法一样）；virtio-gpu 自己的光标仍走 `dpy_cursor_define` |

注：`QemuDmaBuf` 的 `x/y/w/h` 子矩形（`backing_width` ≠ `width`）在第一版只支持 x=y=0 的情况，其他情况进入回读模式，并用 trace 记录。

### 5.3 同步与节流

> **已改（见 12 节）：不挡 guest。** 下面挡 guest 的方案实现过，A/B 测试证明它让拖动卡顿、跳帧，已删除。

- `glFlush()` 保证 virgl 的渲染命令已经提交；在 amdgpu、i915 上，dmabuf 的隐式栅栏会让 VA 的读取等这些命令执行完。第一版不传显式的 sync_file（风险见第 10 节）。
- 和 dbus、spice 一样，用 `graphic_hw_gl_block(con, true)` 阻塞 virtio-gpu 的命令队列，直到 libvds 的 `consumed` 回调（编码线程在 VPP 提交之后调用）。回调里只做 `qemu_bh_schedule`，在 BH 里调用 `graphic_hw_gl_block(con, false)`（主线程规则和 `rdp_release` 一样）。
  - 效果：guest 不会在编码读取的过程中改写同一块缓冲区（没有撕裂），guest 的帧率自然被限制在编码速率上；
  - 没有客户端，或者 `vds_server_wants_frame()` 为 false 时，不阻塞，只累积脏区；
  - 保险：阻塞超过 `2 * 1000/max_fps` ms 还没回调，就用定时器强制解除，并 `warn_report` 限流打印，防止 libvds 卡住时把 guest 也卡死。

### 5.4 回读模式

每个 head 有三种输入模式：`DMABUF` → `READBACK` → `OFF`，状态机见第 7 节。以下情况进入 `READBACK`：`vds_head_attach_dmabuf` 返回 `VDS_E_UNSUPPORTED`（没有 vaapi feature、`encoder=openh264`、探测导入失败、NVIDIA……）、导出 fd 失败、子矩形 scanout，或者 libvds 通过回调要求 CPU 像素。此时前端退回到 egl-headless 的做法：`egl_fb_setup_for_tex` 加 `egl_fb_blit`，`dpy_gl_update` 时用 `egl_fb_read_rect` 读进一张 2D surface，然后走现有的 `submit`。这部分代码按 egl-headless 的思路写在 `rdp-gl.c` 里，所以 `gl=on` 在任何 GPU 上都能用，只是没有零拷贝。启动时 `info_report` 一次，说明是走零拷贝还是回读，以及原因。

### 5.5 trace-events

新增 `rdp_gl_scanout(index, w, h, fourcc, modifier, y0_top, mode)`、`rdp_gl_update(index, x, y, w, h, blocked)`、`rdp_gl_unblock(index, waited_us, forced)`。原有的 `rdp_display_submit` 继续记录主线程开销，便于做前后对比。

## 6. libvds 侧

### 6.1 C ABI（`vds.h`，只做兼容的增加，按规则不改版本号）

```c
#define VDS_E_UNSUPPORTED -4   /* the host or build cannot take this kind of buffer */

/* A dmabuf scanout. Linux only; the fd is dup()ed by libvds. */
typedef struct {
  int32_t  fd;
  uint32_t width, height, stride;
  uint32_t fourcc;      /* DRM_FORMAT_* */
  uint64_t modifier;    /* DRM_FORMAT_MOD_INVALID = implicit */
  bool     y0_top;      /* true: rows are stored bottom-up (QEMU convention) */
} VdsDmabuf;

/* Called once per frame after libvds has finished reading the buffer. [any-thread callback] */
typedef void (*VdsConsumedFn)(void *opaque);

/* VdsCallbacks gains one member (appended, compatible): */
/* libvds wants head `index` as CPU pixels (cpu = true) or takes dmabufs again (cpu = false).
   Raised when the session's VA encoder cannot open or fails, when a client only negotiates
   RemoteFX/bitmap, or when importing/encoding an attached dmabuf fails; `reason` is for logs.
   Delivered from vds_server_dispatch() on the main thread. */
void (*head_input)(void *opaque, uint32_t index, bool cpu, const char *reason);

int32_t vds_head_attach_dmabuf(VdsHead *head, const VdsDmabuf *buf,
                               VdsConsumedFn consumed, void *opaque);
int32_t vds_head_detach(VdsHead *head);   /* back to the shadow buffer, content black */
```

- 声明放在 `#ifdef __linux__` 外面也可以，但在非 Linux 上固定返回 `VDS_E_UNSUPPORTED`，这样 Windows 侧 C 代码不用写条件编译；
- 尺寸必须等于 head 当前的尺寸（先 `vds_head_resize`）；
- 再次 attach 会替换旧的缓冲区（代号 +1）；`resize`、`detach`、`submit`、`attach_shared` 也会放掉它；
- 挂上 dmabuf 后脏区照旧通过 `vds_head_damage` 报告；
- 探测：attach 时在服务器级别的 VA 显示上试导入一次（Mesa 只是建一个 surface，不碰像素，属于 ms 级开销），失败就返回 `VDS_E_UNSUPPORTED` 并附带原因。

### 6.2 vds-core

- 新建 `vds-core::platform`（目前还没有这个模块，`AGENTS.md` 要求平台代码集中在这里）：`#[cfg(unix)] DmabufSource { fd: OwnedFd, width, height, stride, fourcc, modifier, y0_top }`；
- `display.rs`：`External` 改成枚举 `Cpu { pixels, stride }` / `Dmabuf { source: Arc<DmabufSource>, generation, consumed }`；
- `take_frame` / `with_frame` 的 `FrameRef` 加一个变体（或者另开一个 `take_gpu_frame`），让消费方自己选择：要么拿 dmabuf 自己处理，要么调回读接口取 CPU 像素（6.4）。这里优先保证现有 CPU 路径的代码不动。

### 6.3 vds-encode::vaapi

- shim 新增：
  - `vds_vaapi_import(h, const vds_dmabuf*) -> surface`；
  - `vds_vaapi_encode_surface(h, surface, flip, force_idr, pts, out)`：VPP 时设置 `mirror_state = VA_MIRROR_VERTICAL`（`flip = y0_top`），先检查 `mirror_flags`，不支持就在 open 时报不可用；
  - `vds_vaapi_release_surface`；
- fourcc 对照：`XRGB8888→BGRX`、`ARGB8888→BGRA`、`XBGR8888→RGBX`、`ABGR8888→RGBA`（virgl 实测用的是后两种）。VPP 的输入 surface 格式按每次导入的格式决定，不能写死成 BGRX；
- `VaapiEncoder` 按 `(head, generation)` 缓存导入得到的 surface，代号变化时销毁旧的，再导入新的；
- `egfx.rs`：`Picture` 新增 `Gpu`（没有像素，只记录 generation），`Encoder::Vaapi` 遇到它就调用 `encode_surface`；编码线程在 VPP 提交之后调用 `consumed`；
- 不需要新增依赖：`va_drmcommon.h`、`va_vpp.h` 属于已有的 libva。

### 6.4 需要 CPU 像素时：VA 回读

以下情况需要 CPU 像素：会话的 VA 编码器打开失败而回退到 openh264；客户端只协商到 RemoteFX/bitmap；ClearCodec 瓦片。这时在消费方线程上用 `vaGetImage`（按矩形）把 dmabuf 读进影子缓冲，后面照旧走 CPU 路径。读回发生在 libvds 线程上，不占 QEMU 主线程。

- 第一版：dmabuf head **关闭 egfx-mixed**（virgl 的脏区是全屏，`blocks` 永远判断不出静止块，混合模式本来就不起作用），并用 `info!` 记录一次；
- refine 帧：VA 本来就把它当普通帧编码，没有影响。

## 7. 回退

原则：**GL 或 VA 出问题时，画面不能中断，也不能让 VM 退出**。能退到哪一级就退到哪一级，并且每次降级都 `warn_report`（libvds 用 `warn!`），限流打印，说明原因。从高到低：

```
DMABUF（零拷贝）──▶ READBACK（前端 glReadPixels，等同今天的 egl-headless）──▶ 2D（不用 GL 监听）
                                                              └─(读回也失败)──▶ OFF（停在最后一帧，等下次 scanout 重试）
```

### 7.1 启动时

| 失败点 | 行为 |
|---|---|
| 宿主机没有编译 OpenGL/GBM（Windows 等），却指定了 `gl=on` | warn，按 `gl=off` 运行 |
| 已经有别的 GL 显示（egl-headless） | warn，RDP 用 2D 监听挂在它后面（今天的路径） |
| `egl_init` 失败（没有 render node、权限不够、缺少 `EGL_MESA_image_dma_buf_export`……） | 前端改用 `Error *err`，**不用 `error_fatal`**：warn 后按 `gl=off` 继续启动。**但要注意**：VM 里如果有 `virtio-gpu-gl` 设备，它在 realize 时会因为 `display_opengl == 0` 失败（`hw/display/virtio-gpu-gl.c:136`），QEMU 仍然会退出。这和今天 egl-headless 失败的结果一样，前端救不了它 |


### 7.2 每次 scanout（QEMU 主线程）

| 失败点 | 行为 |
|---|---|
| `egl_get_fd_for_texture` 失败、子矩形 scanout、`attach_dmabuf` 返回 `VDS_E_UNSUPPORTED` | 这次 scanout 进入 `READBACK` |
| 读回时出现 GL 错误（`glGetError`、`glCheckFramebufferStatus`），或者 `EGL_CONTEXT_LOST` | 进入 `OFF`：detach，客户端停在最后一帧；`error_report` 一次。下一次 `dpy_gl_scanout_*` 时从 `DMABUF` 重新尝试 |
| 同一个 head 连续 3 次 scanout 都在 `DMABUF` 阶段失败 | 在这个 VM 的生命周期内，这个 head 固定使用 `READBACK`，不再尝试，避免来回切换（OVMF 下 scanout 很频繁）。`query-rdp` 里显示当前模式和原因 |

### 7.3 运行时（libvds → QEMU）

libvds 通过 `head_input(index, cpu=true, reason)` 要求 CPU 像素，QEMU 在主线程上把这个 head 切到 `READBACK`。条件变化后，libvds 用 `cpu=false` 通知，QEMU 在下一次 `dpy_gl_update` 时重新 attach dmabuf。

| 触发 | libvds 的行为 |
|---|---|
| 会话的 VA 编码器打不开，回退到 openh264 | `cpu=true`；会话结束后 `cpu=false` |
| 客户端只协商到 RemoteFX/bitmap | `cpu=true`；客户端断开后 `cpu=false` |
| 编码线程导入、VPP 或编码这块 dmabuf 失败 | 这一帧丢掉，下一帧用 IDR；`cpu=true`。同一个代号的缓冲区不再导入 |
| 第 3 阶段实现了 VA 回读（6.4）之后 | 上面前两种情况改由 libvds 自己读回，不再通知 QEMU；VA 回读本身失败时仍然走 `cpu=true` |

切换期间的那一帧：QEMU 收到 `cpu=true` 后会立刻做一次全屏读回并 `submit`，libvds 对这个 head 发 IDR。客户端最多看到一帧旧画面，不会黑屏。

### 7.4 节流的回退（已随 5.3 删除）

`gl_block` 超时（5.3）会强制放行。同一个 head 连续 3 次超时，就不再阻塞这个 head（接受偶尔撕裂），warn 一次。下一次 scanout 时恢复阻塞。

### 7.5 怎么测试回退

尽量用自然的触发条件，不加专门的故障注入开关：

- `rendernode=/dev/dri/不存在` → 7.1 的 `egl_init` 失败；
- `encoder=openh264` → 一开始就进入 `READBACK`；
- FreeRDP `/gfx:off` 连接 → 7.3 的旧客户端；连接中途再换成 `/gfx` 客户端 → 恢复 `DMABUF`；
- 在 NVIDIA 宿主机上 → 探测失败；
- 第 1 阶段的单元测试里，传一个故意写错的 `VdsDmabuf`（stride 或 fourcc 不对），覆盖导入失败的路径。

## 8. 平台和 GPU 支持矩阵

| 宿主机 | GPU | `gl=on` 的结果 |
|---|---|---|
| Linux | AMD（radeonsi）、Intel（iHD / i965） | 零拷贝（AMD 已验证；Intel 待验证） |
| Linux | NVIDIA | 探测失败 → 回读模式（等同于现在的 egl-headless） |
| Linux | 编译时没有 vaapi，或 `encoder=openh264` | 回读模式 |
| Windows | 任意 | `gl=on` 给出警告后按 `gl=off` 运行；行为不变 |

## 9. 实施分阶段

0. **测基线**（在你自己的 VM 上，由你启动）：用 `-display egl-headless -rdp encoder=vaapi` 在 1080p 下跑办公和视频场景，采集 `rdp_display_submit` 和 libvds 的 `submit`/编码 trace，得到主线程每帧开销和 CPU 占用。如果收益不明显，就在这里停。
1. **libvds ABI + vaapi 导入 + `head_input` 回调**：`vds.h`、`vds-capi`、`vds-core::platform`、`display.rs`、shim；回退（7.3）和零拷贝在同一阶段完成，不允许只有零拷贝、没有回退的中间版本；`vds-test-host` 用 `vaExportSurfaceHandle` 自己生成 dmabuf 做单元测试，不需要 QEMU，也不新增依赖。
2. **QEMU `rdp-gl.c`**：early 挂钩、GL ctx、scanout/update/block、`DMABUF/READBACK/OFF` 状态机和 7.1–7.4 的全部回退，按 7.5 逐项验证；`rdp` 和 `rdp-11` 两条分支同步；过 checkpatch。
3. **VA 回读**（6.4），让 openh264 回退和旧客户端在 dmabuf head 上也能用。
4. **文档和对比测量**：重复第 0 步的场景，把前后数据写进 README。

每个阶段都要：Linux 和 Windows（MSYS2 UCRT64）宿主机都能编译；更新受影响的 `AGENTS.md`；只有人类明确要求时才提交。

## 10. 风险和待验证的问题

| 问题 | 验证方法 |
|---|---|
| 只靠隐式同步：`glFlush` 之后 VA 读到的是否总是完整的帧 | 第 2 阶段跑高帧率动画，看有没有撕裂或残影；有的话改用 `EGL_ANDROID_native_fence_sync` 导出 sync_file，VA 读之前先等它 |
| `vaGetImage` 能不能正确读取导入的隐式布局（tiled）RGB surface | 第 3 阶段之前用 `research/dmabuf-verify` 的工具先测 |
| Intel iHD 驱动对 `MOD_INVALID` 导入和 VPP 垂直镜像的支持 | 需要一台 Intel 宿主机 |
| `graphic_hw_gl_block` 对 guest 帧率和交互延迟的影响 | 第 2 阶段对比阻塞和不阻塞两种情况 |
| OVMF 下 scanout 频繁切换（XB24/AB24 交替）导致反复导入 | trace 统计导入次数；必要时按 fd 的 inode 去重 |
| 多 head、分辨率切换时，代号和编码器重建的正确性 | 第 2 阶段测双屏和动态改分辨率 |
| Windows guest 的 viogpudo 在 `virtio-gpu-gl` 设备上能否正常工作 | 需要你的 Windows VM |

## 11. 实现时要同步更新的文档

- `AGENTS.md` §2（见第 3 节）；
- `docs/spec/c-qemu-style.md`：第 33 行（文件职责表里加 `ui/rdp-gl.c`）和第 79 行；
- `docs/spec/c-abi.md`：新增的错误码和 `VdsDmabuf` 的平台说明；
- `README.md`：Architecture 一节的数据通路，第 149 行的 GL guest 用法（`-rdp gl=on` 和 egl-headless 的区别）；
- `crates/vds-core/AGENTS.md`、`crates/vds-encode/AGENTS.md`、`crates/vds-capi/AGENTS.md`、`crates/vds-rdp/AGENTS.md`；
- `../qemu-rdp/AGENTS.md`：新文件 `ui/rdp-gl.c`，以及 vl.c 的 early 挂钩。

## 12. 实现说明（与上文的差别）

- **没有实现 6.4 的 VA 回读**：所有需要 CPU 像素的情况（openh264 回退、legacy 客户端、导入或编码失败）都经
  `head_input(cpu=true)` 交给 QEMU 回读（7.3 表格最后一行描述的“之前”的状态）。ClearCodec 图块在 GPU 缓冲上直接跳过。
- **不挡 guest。** 最初按 5.3 实现了 `graphic_hw_gl_block`：guest 每 flush 一帧就被挡住，直到 libvds 在下一个
  视频周期取到这一帧并编完（`consumed` 在 `encode_gpu` 返回后触发），每帧挡 20–25 ms（取帧等待中位数约 15 ms，
  编码 3–10 ms）。期间 guest 的 Xorg 卡在 GL 调用里，鼠标事件在 guest 里堆积，拖动先卡后跳；libvds 这边
  仍然是约 60 fps，所以服务端数据看不出问题。同一个镜像、只差挡不挡的两台 VM 对比：挡的卡，不挡的流畅。
  现在 QEMU 不再挡 guest，`consumed` 传 NULL。libvds 的 `consumed` 接口保留（可选），以后要做“只挡到 VPP
  拷完”的无撕裂方案时可用。
- 回读模式下客户端忙时跳过的更新会记下来，下次读整帧（原实现直接丢掉，那块区域不会再发）。
- `rdp-gl.c` 是模块 `ui-rdp-gl`（依赖 `ui-opengl`），经 `RdpGlOps` / `rdp_gl_register()` 注册；`--enable-modules`
  下 EGL 辅助函数不在主程序里，这是唯一可行的做法。模块缺失也只是退回 2D。
- `RdpGlOps.display_init` 返回监听 ops（QEMU 11 注册监听时要传 ops），两条分支的接口相同。
- 子矩形 scanout、多平面缓冲（QEMU 11）都走回读；回读读整帧（`egl_fb_read`），只提交脏区矩形。
  `egl_fb_read_rect` 没有按 y 偏移目标行，不能用。
- `query-rdp` 新增 `displays`：`input` 为 `copy` / `shared` / `dmabuf` / `readback` / `off`，后三种带 `reason`。
- 光标 dmabuf（`dpy_gl_cursor_dmabuf`）读回后用 `vds_cursor_define` 发给客户端。

### 验证结果（2026-11-12，AMD Renoir，Mesa 25.0.7，pxvdi live ISO，`sdl-freerdp` 3.32）

| 场景 | 结果 |
|---|---|
| `gl=on,encoder=vaapi`，AVC420 客户端 | `input=dmabuf`，VA-API 编码，画面方向正确；主线程无回读 |
| `gl=on,encoder=openh264` | 拒收 3 次后固定 `readback`；每帧回读 6.2 ms（最大 7.7 ms），这就是 dmabuf 省掉的主线程开销 |
| legacy 客户端（`-gfx /rfx`） | 连接时 `dmabuf → readback`（“the legacy transport sends CPU pixels”），断开后回到 `dmabuf` |
| `rendernode` 打不开 + `virtio-vga` | 警告后按 `gl=off` 运行，2D 共享 surface 正常 |
| `rendernode` 打不开 + `virtio-gpu-gl-pci` | 设备因没有 GL 无法创建，QEMU 退出（同 egl-headless） |
| `gl=on` + `-display egl-headless` | 警告，经 egl-headless 的 2D surface 正常 |
| OVMF（`virtio-gpu-gl-pci`，`y0_top=1`） | `dmabuf`，UEFI Shell 画面方向正确 |
| QEMU 11 + `--enable-modules` | 模块加载正常；dmabuf 和 legacy 回退同上 |

测试脚本：`research/dmabuf-verify/rdp-test.sh`。没有测：Windows guest、Intel/NVIDIA 宿主机、多 head、Windows 宿主机编译（本机没有 mingw C 工具链，只做了 `vds-core` 的 Windows 目标类型检查）。
