# 自实现 vs 官方 MIoT SDK：层对齐矩阵（学到几层 / 还能学什么）

> 2026-11-22。基准：miloco `backend/miot`（官方 SDK，24 模块 + 27 测试）↔ 本仓
> mijia-core（16 模块 ~6.6k 行）+ mijia-napi（797 行）+ ArkTS App + home-control-mcp（~2.7k 行）。
> 口径：只对照**事实与架构**，不抄代码（license 红线 + 语言异构）。
> 评级：✅ 已掌握（自实现+真机验证）· ➕ 反超（官方没有的层）· ◐ 半成品 · ❌ 空白。

## 总览

| 层 | 官方出处 | 我们的实现 | 评级 |
|---|---|---|---|
| L0 协议知识/逆向 | `const.py` + knowledge/ | 协议面分析报告、蓝牙认证协议、APK 全景 | ✅（互补） |
| L1 账号凭据 | `oauth2.py`/`ha_api.py`/`auth_state.py` | `auth.rs` 密码登录+2FA+passToken 续期 | ✅（路径不同，OAuth2 待学） |
| L2 云端 HTTP+签名 | `cloud.py` | `crypto.rs`+`cloud.rs`+device/room/card_net | ✅（端点子集） |
| L3 spec/设备模型 | `spec.py`/`iid.py`/`types.py` | `device.rs`+`card.rs`+SpecKit+`profile.rs` 指纹 | ✅（异构等价） |
| L4 云端实时推送 | `mips_cloud.py`（1035 行） | **无（轮询）** | ❌ **最大空白** |
| L5 LAN/OT 发现 | `lan.py`（972 行） | 本轮 OT 探针（脚本级，未进产品） | ◐ |
| L6 中枢本地控制 | `mdns`+`cert`+`central_hub`+`mips_local` | 静态逆向笔记（证书链已由官方口径闭环） | ❌（无硬件） |
| L7 BLE 直连 | **官方无此层** | `ble.rs`+`ble_auth.rs`+`ble_cache.rs` | ➕ **反超** |
| L8 摄像头 | `camera.py`+`decoder.py`+`libmiot_camera_lite` | PPPP/MISS 部分 RE + 路线图 | ❌（有知识储备） |
| L9 控制安全/审计 | **官方无此层**（自家 Agent 直控） | `safety.rs`+home-control-mcp 全套闸门 | ➕ **反超** |
| L10 工程质量/连接生命周期 | 27 测试+CodeQL 修复+节点监控 | cargo test 79+、fixtures 对拍、密钥扫描 | ◐（形态可学） |

**结论：10 层里到位 6 层**（4 层与官方对齐 + 2 层官方根本没有），半成品 2 层，空白 3 层。
官方没有而我们有的两层的存在本身就是情报：**官方对 BLE 的产品态度=只走云端缓存/网关上报**
（miloco SDK 零 BLE 代码），我们的 GATT 直连+本地/云端新鲜度合并（`ble_cache.rs`）是差异化能力；
**官方对控制面的态度=信任后端 Agent**，我们坚持门禁+审计是定位差异，不是落后。

## 各层要点与"还能学什么"

### L4 云端 MQTT 推送 —— 最值得学（❌→目标 ✅）

官方事实（`mips_cloud.py`）：
- **MQTT v5 / TLS 8883**，按机房选 broker（`const.py`）；
- **username = OAuth2 client_id，password = OAuth2 access_token**（`update_access_token` 轮换）；
  ⇒ **推送层的前置是 OAuth2 凭据流**（L1 联动），现有密码登录的 serviceToken 进不了 broker；
- 订阅**精确叶子 topic**（不用通配符——broker ACL 会拒 0xA2），断线重连自动重订阅
  （`_subs` 留底），bind/unbind 推送监听（`welcome_service` 的上游）；
- 属性/事件推送去重（`message_dedup.py`）+ 状态对齐（`state_align.py`）。

对我们的意义：16 台设备的卡片状态从"打开 App 才轮询"变成实时。Rust 侧 `rumqttc` 即可，
无新硬件、无新协议逆向，只有凭据流一项前置工作。**优先级最高。**

### L1 OAuth2 凭据流 —— 与 L4 捆绑

官方走 `/app/v2/ha/oauth/get_token`（client_id/secret 应用身份 + unionid→uid 换取），
密码永不出库、token 可静默续期。我们已有扫码/WebView 基建，接入 OAuth2 授权码流
即可拿到 MQTT 门票，也顺带解决 serviceToken 过期行为未测的遗留项（SESSION_STATE §三.4）。

### L5 OT 发现状态机 —— 把探针变成产品

官方 `lan.py` 的形态值得整套搬架构（不是代码）：
保活宽限期 → 离线定时器 → **跨网段单播补探**（广播会被 AP 隔离/风暴控制丢）→
按网卡 fan-out + 子网定向广播解析 → 单播 socket 只信白名单目标 IP（防局域网任意主机伪造）。
我们的 32B OT 编解码已验证，差的就是这圈状态机与 IP↔did 持久化。

### L6 中枢本地控制 —— 归档待硬件

证书链/mDNS 技巧/TLV/选路已全部归档进《miloco-机会分析.md》§3。无中枢硬件前不投入；
若走"PC 常驻 miloco + App 消费 API"路线（花样④），此层可长期外包。

### L8 摄像头 —— 学方法论 + 学分发协议

官方给出的新事实：C ABI 的 `info={did,model,ip,channel_count}` + `config={qualities,enable_audio,pin_code}`
→ **裸 H.264/H.265 帧回调**。对我们可学三件：
1. 观测法：在 PC 用官方 lib 跑通后抓流量，对照 `camera_protocol_notes.md` 钉死 PPCS/MISS；
2. `ws.py` 的直播分发协议（文本 init + 二进制 NAL、闲时拆除、背压、`multi_reg` 共享解码）——
   App 端自实现 WS 客户端即可吃官方后端的流；
3. 摄像头 did 的 LAN 特殊处理（`set_camera_dids`——相机不走 OT 保活，单独通道）。

### L2/L3 补充项（低成本清单）

- **手动场景 API**（get/trigger）：App 可加"场景卡片"，一组属性一次下发；
- **action_result/result_codes 错误分类学**：我们现在只有 `code!=0` 穷尽 match，
  官方把云端返回码分成可重试/凭据类/设备类——我们的 session 续期触发条件可以直接对齐这套分类；
- spec 缓存版本化与 i18n 描述：SpecKit 已有磁盘缓存，差版本策略与多语言；
- OTA 推送的 `name` 形状陷阱（连字符 vs 下划线）→ 白名单丢弃+日志，不猜映射——我们做
  spec 驱动渲染时同款防御。

### L10 工程形态（学方法不学代码）

- **假 gateway 测试法**：官方 27 个测试里一半是对着假 broker/假网关的状态机测试——
  我们测过假云（FakeCloud），连接层还没测过（MQTT 接入后正好用同款手法）；
- 凭据文件 **0600 pre-create**（写前建文件，防 inotify 窗口）——比 chmod-after-write
  严格的时序洞察，session.json/beaconkey 落盘时适用；
- 续期 sweep + per-connection 锁防重连竞态、优雅停机的 producer→consumer 顺序、
  `/health` "只报不健康"语义。

## 行动清单（按性价比）

1. **OAuth2 流 + 云端 MQTT 推送**（L1+L4）：Rust `rumqttc`，App 卡片实时化；纯云、零硬件。
2. **OT 状态机下沉**（L5）：OT 编解码进 `mijia-core`，设备列表加"局域网在线"。
3. **场景 API + 错误分类学**（L2）：App 场景卡片；session 续期条件对齐官方分类。
4. **摄像头专项立项**（L8）：先 PC 观测，再决定 Rust 复刻 or 长期走 miloco WS。
5. **工程质量补课**（L10）：MQTT 层的假 broker 测试、凭据落盘 0600 pre-create。

> 反超层的维护提醒：`ble.rs`/`ble_auth.rs`/`ble_cache.rs` 官方无对照，回归全靠我们自己的
> golden 向量与真机对拍——这套 fixtures 是资产，别随重构丢掉。
