# vds-bench — 设计与约束

改本工具时先读根目录 `AGENTS.md`；改完在同一次改动里更新本文件。

## 职责

测量 BGRX→I420 转换和 openh264 编码在典型桌面负载下的耗时和码量，以及调试目录记录路径的开销。

```text
vds-bench [--frames 120] [--threads 2] [--bitrate 20000] [--qp-max 30]
vds-bench debug [--records 200000]
```

场景：`typing`（64×16 小块变化）、`window`（400×300 区域移动）、`scroll`（整行带滚动）、`video`（整帧变化）。
`--qp-max 51` 是不设量化上限的对照。

## 约束

- 用 release 构建测量；debug 构建的数字没有意义。
- 只依赖 `vds-core`、`vds-encode`；场景与 `vds-encode` 的实际调用方式保持一致。
