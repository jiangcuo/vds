# vds-demo — 设计与约束

改本工具时先读根目录 `AGENTS.md`；改完在同一次改动里更新本文件。

## 职责

不用 QEMU 的独立 RDP 服务器：`vds-rdp` + `vds-test-host` 的动画测试图，用来测客户端兼容性。

```text
vds-demo [--listen 127.0.0.1:3389] [--user U --password P] [--auth none|tls|nla]
         [--size 1920x1080]... [--codecs avc420:remotefx:bitmap] [--fps 60]
         [--bitrate KBPS] [--cert cert.pem --key key.pem] [--log info] [--debug-dir DIR]
```

`--debug-dir` 写出的会话文件与 QEMU `-rdp debug-dir=` 相同。

## 约束

- 只组合现有 crate，不在这里实现协议或编码逻辑。
- 画面是一直在动的，测不出"静止画面"相关的问题（如首帧、空闲时的行为）。
