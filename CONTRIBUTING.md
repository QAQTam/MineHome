# Contributing

欢迎 PR。先读 [docs/FORK-SETUP.md](docs/FORK-SETUP.md) 把工程跑起来。

## 提交流程

0. 新机器先跑 `just check`（无 just：`python tools/check_env.py`）；
1. 从 `main` 开分支，一个小改动一个分支；
2. 本地过**三条门**（与 CI 相同）：
   ```powershell
   just test        # = cargo test × 2（mijia-core、home-control-mcp）
   just secrets     # = python tools/check_secrets.py --all，必须退出码 0
   # 或一条搞定：just gate
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

## 用 AI agent 贡献（欢迎，但守这三条）

本项目自视为 **AI 友好型**：agent 产出的 PR 与人类 PR 同标准同验收。
若你是 agent（Claude / Codex / Copilot / Cursor / 自研皆可）：

1. **先建基线再动手**：`just check --json` 读环境，`just gate` 确认干净，改完重跑；
2. **红线零容忍**：任何真实 did / token / 设备名 / 抓包不得进入被跟踪文件
   （闸门会拦，但那是最后一道）；二进制不进 git，预编译产物走 GitHub Releases；
3. **以仓库为准**：协议事实以 `docs/` 的实测档案为准；引入新判断时在 PR 里给证据
   （命令 + 输出摘要），并在涉及安全闸/控制面时写明"最坏情况"分析。

## 许可
