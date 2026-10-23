# xtask — 设计与约束

改本 crate 时先读根目录 `AGENTS.md`；改完在同一次改动里更新本文件。

## 职责

构建辅助命令：

```text
cargo run -p xtask -- gen-abi              # 用 cbindgen 重新生成 include/vds.h
cargo run -p xtask -- check-abi            # include/vds.h 过期时失败
cargo run -p xtask -- install --prefix DIR # release 构建，装 libvds.so 和 libvds.so.0（SONAME）+ 头文件 + vds-1.pc
cargo run -p xtask -- c-smoke              # 编译并运行 C ABI 冒烟测试
```

Windows 上在 MSYS2 UCRT64 shell 里执行。

## 约束

- 只用 `anyhow`，不依赖 libvds 的库 crate。
- `install` 的产物布局（`lib/`、`include/`、`lib/pkgconfig/vds-1.pc`）是 QEMU 构建的约定，改了要同步 `docs/spec/c-abi.md` 和 `docs/spec/c-qemu-style.md`。
