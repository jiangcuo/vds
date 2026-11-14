# vds-encode — 设计与约束

改本 crate 时先读根目录 `AGENTS.md`；改完在同一次改动里更新本文件。

## 职责

视频编码：把显示头的 BGRX 像素变成 H.264（AVC420）码流。与协议无关，只依赖 `vds-core`。

## 模块

| 模块 | 内容 |
|---|---|
| `openh264` | `OpenH264Encoder`：默认编码器，CPU。配置：低复杂度、Baseline profile、按需 IDR、全范围 BT.709 VUI，但用**质量模式**（`RC_QUALITY_MODE`，动态码率）而不是目标码率；不跳帧；量化上限 `qp-max`。`set_rate` 是空操作 |
| `yuv` | `I420Frame` 与 BGRX→I420：只转换脏区（宏块对齐），整行时直接写进平面；SIMD 由 `yuv` crate（yuvutils-rs fork）运行时选择 |
| `bgrx` | `BgrxFrame`：给自己做颜色转换的编码器（VA-API）用的 BGRX 暂存图 |
| `vaapi` | `VaapiEncoder`：可选，`vaapi` feature 且 Linux 才编译；FFmpeg `h264_vaapi` 经 C shim（`vaapi/shim.c`），上传 BGRX，用 VA VideoProc 在 GPU 上转 NV12（BT.709 全范围），码控 VBR；不可用时 `open` 返回 `Error::Unavailable`，调用方回退 openh264。`encode_gpu` 直接编码宿主的 dmabuf（`GpuFrame`）：按 generation 用 `DRM_PRIME_2` 导入一次（支持 XRGB/ARGB/XBGR/ABGR8888，modifier 可以是 `MOD_INVALID`），VPP 读它并在 `y0_top` 时垂直翻转，不上传；导入或翻转不可行时返回 `Error::Unsupported`。`VaapiProbe` 在交接前检查一块缓冲能否导入和翻转。`vds_vaapi_test_dmabuf` 只供测试生成 dmabuf |
| `error` | 错误类型 |
| `lib` | `VideoEncoder` trait、`EncoderConfig`、`RateControl`、`EncodedFrame`（`I420Frame` 在 `yuv`） |

## 构建

- openh264 来自 `gitea.lierfang.com/pxcloud/openh264-rs`，从源码编译，需要 C++ 编译器和 nasm。
- `build.rs` 只编译 VA-API shim；没有 `vaapi` feature 或 pkg-config 找不到 libavcodec/libavutil/libva 时跳过。

## 约束

- 不做其它硬件编码（NVENC/QSV/AMF/MF），不用 GStreamer；不再使用 x264。
- 编码在调用方的专用线程上运行，本 crate 不起线程、不用 async。
- `unsafe` 只允许在 `vaapi` 模块里，必须有 `// SAFETY:` 注释。
- 输出画面尺寸必须是显示尺寸（编码尺寸按宏块补齐，但客户端绘制区域不能被拉伸）。
