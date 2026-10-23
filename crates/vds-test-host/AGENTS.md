# vds-test-host — 设计与约束

改本 crate 时先读根目录 `AGENTS.md`；改完在同一次改动里更新本文件。

## 职责

不依赖 QEMU 的合成宿主：按 `fps` 往 `Core` 的显示头里画动画测试图，并像 QEMU 一样回应客户端事件
（布局请求时调整头尺寸，剪贴板请求时给出固定文本）。给集成测试、`vds-demo`、`vds-bench` 用。

## 接口

- `TestHost::start(core, sizes, fps)`：建头并开始画；`stats()` 返回收到的事件计数；`stop()` 停止。
- 事件处理在它自己的线程上做，相当于 QEMU 主线程的 `drain_events`。

## 约束

- 只依赖 `vds-core`，不依赖协议层。
- 行为要和 QEMU 前端保持一致：QEMU 前端的事件处理改了（如剪贴板新类型、布局规则），这里要跟着改，
  否则测试和 demo 测到的不是真实行为。
- 不进生产构建（`vds-capi` 不依赖它）。
