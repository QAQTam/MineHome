# Security Policy

## 凭据模型（先读懂这个再用）

- 登录产物（`session.json`：userId / ssecurity / serviceToken / nonce）是**完整账号会话**，
  拿到即可读写你家全部设备。App 把它只写进应用私有目录（`filesDir`），不上传、不备份、不同步。
- 「三方平台导入」与「退出登录」同理只在本地读写；退出会调用官方 logout 并删除本地文件。
- 本仓库**永不**提交任何真实凭据：`.gitignore` 屏蔽 `session.json` / `pass_token.json` /
  `cookies*.txt` / `beaconkey*`，并且 `tools/check_secrets.py` 会在提交前用**动态生成的
  真实值清单**（did / uid / 设备名 / 住址 / 凭据形态）扫描全部待纳管文件，命中即非零退出。

## 若你发现了问题

请使用 GitHub 的 **Private Security Vulnerability Reporting**（Security ▸ Report a
vulnerability）上报，不要开公开 issue。维护者承诺 72 小时内回应初步评估。

## 已知敏感面（自查时重点关注）

| 面 | 风险 | 现有防线 |
|---|---|---|
| 日志 | hilog / 控制台可能带出 token、did、明文响应 | `native.ets` 出口与 Rust 侧日志只打长度/前缀；新增日志前先跑 check_secrets |
| 抓包/调试留档 | `_work/` 类目录天然含真实账号数据 | 整目录忽略；对外分享日志/截图前先脱敏 |
| 设备档案导出 | 卡片档案可能被用户转发 | 导出即脱敏（无 did/uid/token/设备名），`profile.rs` 有负向断言单测钉死 |
| MCP 控制面 | Agent 误控真实设备 | allowlist + 静默时段 + 冷却 + 审计 + fail-closed 启动自检 + 空调类代码级禁用 |

## 版本支持

安全修复只应用于当前 main 分支；发布 tag 起不做回溯承诺（0.x 阶段）。
