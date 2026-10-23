# vds-rdcleanpath — 设计与约束

改本工具时先读根目录 `AGENTS.md`；改完在同一次改动里更新本文件。

## 职责

网页 RDP 客户端（IronRDP web / iron-remote-desktop）的 RDCleanPath 桥接。浏览器不能直接开 TCP 或自己跑 TLS，
所以它把 X.224 连接请求包在 RDCleanPath 请求里，经 WebSocket 发过来。本工具：

1. 读一个完整的 RDCleanPath 请求（DER）；
2. 连 RDP 服务器，转发 X.224 请求，读回 X.224 确认；
3. 自己和服务器完成 TLS 握手（不校验证书，目标是本机的 QEMU）；
4. 回复 RDCleanPath 响应：X.224 确认 + 服务器证书链（客户端用它的公钥做 CredSSP）；
5. 之后在客户端流和解密后的 TLS 流之间双向转发。

出错时按协议回 HTTP 风格错误码（请求格式错 400，连不上目标 502）。

```text
vds-rdcleanpath --connect HOST:PORT [--listen ADDR:PORT] [--log FILTER]
```

- 不带 `--listen`：在 stdin/stdout 上服务一个连接，给 PVE 用（`rdpproxy` 接口像 `qm vncproxy` 那样启动它，pveproxy 终结 WebSocket）。
- 带 `--listen`：接受 TCP 连接，每个连接一个桥接，给测试用（前面放 websockify）。

## 约束

- **只连 `--connect` 配置的目标**，客户端在 RDCleanPath 里写的 destination 只记日志，绝不用来决定连接哪里。
- 不做鉴权：调用方（pveproxy 的 ticket）在流到达之前已经鉴权；请求里的 auth token 不检查。
- stdout 在 stdio 模式下承载 RDP 数据，日志只能写 stderr。
