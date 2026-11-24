# Contributing

欢迎 PR。先读 [docs/FORK-SETUP.md](docs/FORK-SETUP.md) 把工程跑起来。

## 提交流程

1. 从 `main` 开分支，一个小改动一个分支；
2. 本地过**三条门**（与 CI 相同）：
   ```powershell
   cd mijia-core        ; cargo test
   cd home-control-mcp  ; cargo test
   python tools/check_secrets.py    # 必须退出码 0
   ```
3. 涉及卡片/spec 行为：给 `mijia-core/fixtures/` 补合成夹具，让回归可离线复现；
   禁止把任何真实抓包（含真实 did/token/设备名/住址）直接放进仓库；
4. 涉及安全闸（`safety.rs`）或控制面（mijia-napi 写接口、MCP）：在 PR 描述里写清
   「最坏情况这台设备会发生什么」，并新增负向用例。

## 设计纪律（本项目特有，review 会按此执行）

- **只读先行**：新能力先做只读，控制下发必须过既有安全闸，宁缺勿滥；
- **豁免必须留痕**：`check_secrets.py` 的每条 ALLOW 都要写理由，reviewer 核理由而不是放行；
- **事实与代码分离**：docs/ 里写观察到的协议事实，不搬运任何第三方（含小米官方）源码；
  引用参照实现时注明出处（见 NOTICE 的许可矩阵）；
- **接口稳定**：ArkTS ↔ Rust 的 nAPI 面以 `docs/analysis/中间件设计.md` 的契约为准，
  改接口要同时改 `index.d.ts` 生成链和调用点，不做静默兼容。

## 许可

提交即代表同意你的贡献以 Apache-2.0 授权分发，且为你原创或有权再分发的内容。
