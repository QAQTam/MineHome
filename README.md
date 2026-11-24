# MineHome

> 鸿蒙 HarmonyOS NEXT（API 26）上的**非官方**米家生态客户端：ArkTS 界面 + 纯 Rust 协议核心 + 设备控制 MCP Server。
>
> English: an **unofficial** Mi Home (Mijia) client for HarmonyOS NEXT — ArkTS UI, a pure-Rust core library (`mijia-core`), NAPI bridge, and a fail-closed Home-Control MCP server. Not affiliated with Xiaomi.

- 登录你自己的小米系账号，设备列表 / 房间归属 / 卡片模板 / 属性读写全部走云端实时接口，**仓库内不含任何账号数据**。
- 所有凭据只落在你自己设备的私有目录；提交前有一道动态泄漏扫描闸门（`tools/check_secrets.py`）。
- 「只读先行，控制过闸」：一键开关要过 Rust 侧三重安全闸（`safety.rs`），宁缺勿滥。

## 功能一览

| 面 | 现状 |
|---|---|
| 账号 | 密码登录（含 2FA 通知确认分支）、三方平台 passToken 导入换会话、退出登录 |
| 首页 | 米家风格双列卡片、房间分组、did 锚定排序、拖拽换位、拼卡轻档（可整卡隐藏）、圆形快控钮 |
| 设备弹层 | spec 驱动全量可读属性、控制/观察分区、乐观更新压制回读、联动禁用灰显、缓存新鲜度文案 |
| 数据通道 | `/miotspec/prop/get` 统一读（WiFi / Zigbee / BLE mesh / BLE 直连全覆盖，带 `updateTime`）→ `/home/rpcv2` 实时回落；`/home/rpcv2` 写 + `action` |
| 本地蓝牙 | 档位 A：全局常驻 MiBeacon 广播收数（0xFE95，V2–V5，AES-CCM* 解密 + beaconkey 预取）；档位 B：GATT 直连保活会话（订阅 notify「变化秒推」，轮询兜底，5 分钟上限自动断开） |
| Agent 接入 | `home-control-mcp`：白名单动作、静默时段、冷却、审计、fail-closed 启动自检；空调类设备代码级永不控制 |
| 档案 | 可共享的设备卡片档案（导出即脱敏：无 did / uid / token / 用户设备名，有单测钉死） |

已知边界（诚实清单）：云端实时推送（官方 mips/MQTT 通道）尚未接入，弹层内为 6 s 轮询；BLE mesh 子设备没有本地监听通道；摄像头仅有协议研究笔记，无播放实现。详见 `docs/analysis/自实现与官方SDK-层对齐.md` 的十层矩阵。

## 架构

```text
┌─────────────────────────────  app/（ArkTS，HarmonyOS NEXT） ─────────────────────────────┐
│  Index.ets 卡片首页/弹层   BleKit/BleIngest 广播收数   BleGatt 直连保活   SpecKit spec 缓存 │
└──────────────────────────────────────┬───────────────────────────────────────────────────┘
                                napi（mijia-napi，ohos-rs 桥）
┌──────────────────────────────────────┴───────────────────────────────────────────────────┐
│  mijia-core（纯 Rust，可 PC 测试）                                                          │
│  auth(登录/passToken) cloud(RC4 签名请求器) device(属性读写/rpc) room layout profile        │
│  card+card_net(米家卡片模板引擎) safety(一键安全闸) ble(MiBeacon 解码) ble_auth(mible GATT) │
│  ble_cache(本地/云端新鲜度合并) session(会话持久化)                                          │
└───────────────────────────────────────────────────────────────────────────────────────────┘
   home-control-mcp（独立 crate）：把上面的读通道 + 白名单写通道包成 MCP server 给 Agent 用
```

## 快速开始

环境准备、签名、原生库编译（`ohrs build` + strip）、HAP 打包、真机安装、提交前三条门——全部在 **[docs/FORK-SETUP.md](docs/FORK-SETUP.md)**，每条命令都在 Windows + DevEco 命令行工具链实测过。

TL;DR：

```powershell
# 0) 新机器第一件事：环境自检（缺什么、怎么装，逐项给提示）
just check                          # 没装 just 就：python tools/check_env.py

# 1) Rust 原生库（公开仓库不携带 .so 预编译产物，见 FORK-SETUP §2 的原因）
cd mijia-napi ; ohrs build          # 产出 dist/<abi>/libmijia_napi.so + index.d.ts
#    然后 llvm-strip 并拷到 app/native/libmijia_napi/libs/<abi>/ 与 app/entry/libs/<abi>/

# 2) 回归测试（不需要账号、不需要鸿蒙设备）
cd ../mijia-core        ; cargo test
cd ../home-control-mcp  ; cargo test
python ../tools/check_secrets.py

# 3) 组 HAP 装机
cd ../app ; hvigorw --mode module -p product=default assembleHap
```

## 测试策略（为什么 clone 下来就能跑）

1. **协议对拍夹具**：`mijia-core/fixtures/vectors.json` 由 `tools/gen_fixtures.py` 从实测闭环的 Python 原型（`tools/proto/mijia_proto.py`）生成——签名/RC4/nonce 全部是**合成密钥材料**，字节级对拍，不含真实凭据；
2. **transport 注入 mock**：`mijia-core/tests/middleware.rs`、`home-control-mcp/tests/` 用假云端跑全链路编排；
3. **真机冒烟**：ArkTS 侧无纯 CPU 测试，验收清单在 `docs/FORK-SETUP.md`。

`docs/` 里是这套协议的分析档案（登录链路、卡片模板、中枢网关、BLE 认证、摄像头许可事实等）。**仅描述事实与架构**——其中参考的第三方源码（含小米官方开源件）均未复制进本仓库，各文件头部的许可结论见 NOTICE。

## 安全与隐私

- 漏洞报告与凭据处理规范见 [SECURITY.md](SECURITY.md)。
- 本工具连接的是小米云端私有接口，用途限于 interoperability 与学习研究；请只在**你自己的账号**上使用，并自行承担使用条款层面的责任。
- 本软件按「现状」提供，不作任何担保；错误控制可能对真实设备产生影响，务必先用只读模式验证。

## Disclaimer（免责声明）

本项目为第三方独立实现，与小米科技（Xiaomi Inc.）无任何关联、背书或赞助关系。「小米」「米家」「Mi Home」「Mijia」及相应产品标识均为小米科技和/或相关权利人的商标。本仓库不包含小米官方应用、SDK 或固件的任何代码与二进制产物；协议交互仅基于公开可获取的接口行为与规范文档。

## 🤖 AI 友好型项目 / AI-Friendly by Design

**中文**：本项目**欢迎 AI agent 来完善它**——Claude、Codex、Copilot、Cursor 或你自己的
agent，与人类贡献者同规则、同验收（见 [CONTRIBUTING.md](CONTRIBUTING.md)）。为此仓库是
按"无人值守也能正确干活"设计的：

- **一条命令自检环境**：`just check`（`just check --json` / `python tools/check_env.py --json` 机器可读），
  缺什么、怎么装，逐项给提示；
- **验收标准完全可复现**：`just gate` = 两个 crate 的 cargo test + 一次泄漏扫描，
  与 GitHub CI 逐字一致，不依赖维护者的口头知识；
- **离线可跑**：测试向量全部是合成密钥材料，不需要小米账号、不需要鸿蒙设备、不需要网络；
- **文档即事实档案**：`docs/` 每条结论带实测日期与证据链，协议判断以仓库为准；
- **红线由机检守**：真实凭据/账号痕迹由 `tools/check_secrets.py` 拦截，豁免必须写理由。

给正在读这段的 AI：这个仓库没有隐藏状态和部落知识，先 `just check` 再 `just gate`
建立基线，改动后重跑同一组命令。不要把任何二进制提交进 git；不要把你机器上的真实
did / token / 设备名写进任何被跟踪文件——扫描闸门会拦，但更好的是别写。

**English**: Contributions produced with (or entirely by) AI agents are welcome and
judged by the same reproducible gates as human ones. Environment check is one command
and machine-readable (`just check -- --json`), CI mirrors `just gate` verbatim, the
whole test suite runs offline with synthetic vectors, and docs/ record dated facts —
an agent can work correctly here without tribal knowledge. Please never commit
binaries or real account data; the secret gate will catch it anyway.

## License

[Apache-2.0](LICENSE)。贡献即表示同意你的贡献按 Apache-2.0 分发（见 [CONTRIBUTING.md](CONTRIBUTING.md)）。
