# Xiaomi Miloco 机会分析（对 mijia-ohos / MineHome）

> 2026-11-22。样本：`D:\xiaomi-miloco`（github.com/XiaoMi/xiaomi-miloco，v2026.10.16）。
> 定位：小米官方开源"全屋智能 AI 方案"，OpenClaw 插件 + FastAPI 后端 + **官方 Python MIoT SDK（`backend/miot`，9300 行）** + 官方摄像头原生库（预编译）。
> 问题：能不能为我们的米家控制开新花样？**能，而且其中两件正好砸在我们此前工作的盲点上。**

---

## 0. 一句话结论

miloco = 我们过去几轮手工逆向的东西的**官方人类可读版** + 我们没啃下来的两块硬骨头（**中枢本地控制**、**摄像头直播**）的官方实现 + 一层我们完全没有的能力域（**AI 感知/身份/记忆**）。四个可直接吸收的花样 + 一次重大谜底修正（OT 协议）。

**License 红线先说**：`LICENSE.md` = 仅限非商业用途，**未经小米书面授权不得用于开发 APP/Web 服务**。所以：协议格式/参数/流程（事实）可以学习复刻进我们的 Rust/ArkTS；**miloco 的代码不能拷进 MineHome，`libmiot_camera_lite` 不能打包进我们的 App**。自托管运行 miloco 并由我们的 App 消费其 HTTP/WS API，属于运行该软件的灰色边缘，只建议个人自用。

---

## 1. 谜底修正：UDP 54321 的"hello 回显"是 OT 协议（推翻我们上轮的推断）

上轮温湿度计分析（§5.1 探测）结论"网关裸 miIO 已关闭"——**错了一半**。miloco `lan.py` 揭示该端口跑的是小米 **OT（发现/保活）协议**，从来不是 miIO RPC：

```text
OT 探测帧(32B) = "!1\x00\x20" + ff×16 + [请求方 did u64 BE] + 00000000
                          ↑ 与 miIO hello 前 20B 完全同源
OT 应答(32B)   = "!1" + ?? + [设备 did u64 BE @4:12] + [时间戳 u32 BE @12:16] + ff×16
```

- 我们的探测把应答 `[4:8]` 读成"stamp=0"——那其实是 **did 的高 32 位**（did 都 < 2³²，高半字恒 0）；`[8:12]` 里就是设备 did 本尊。回显≠echo，设备在**报身份**。
- 2026-11-22 实测闭环：子网定向广播 `192.168.66.255:54321`（`255.255.255.255` 在 Windows 被拒，miloco 注释同款坑）→ **3 台应答，did 与云端清单逐台命中**（网关 <did-网关> / 空调 <did-空调> / 空调伴侣2 <did-空调伴侣>）。应答源 IP 都显示 192.168.66.1 —— **小米路由器在代答聚合**（OTU Ping）。
- 推论修正：本地控制不走 UDP；控制=云端 HTTP/MQTT 或 **MIPS（中枢 mTLS MQTT）**。`协议面分析报告.md` §4 的"miIO UDP 直连控设备"只对老固件/部分设备成立，需按此修正。

**新花样 ①：免 token 局域网设备发现。** 广播一轮拿全屋 did+时间戳，与 device_list 关联即可做：附近设备页、配网入口、设备在线状态本地校准（不必每次问云）。Rust 侧半天的活，协议零门槛。

## 2. 摄像头直播：官方原生库把最硬的骨头给了（新花样 ②）

我们 camera_roadmap 路线 A（直播）卡在 PPCS/MISS 逆向。miloco 直接内置 `libmiot_camera_lite`：

- **C ABI 极薄**：`miot_camera_init/update_access_token/new(info)/start(config)/stop/status` + `register_raw_data` 回调。
  - info = `{did, model, ip, channel_count}`（双摄支持）
  - config = `{video_qualities*, enable_audio, pin_code}`
  - 帧回调 = header `{codec_id, length, timestamp, sequence, frame_type, channel}` + **原始 H.264/H.265 裸帧**
- **仓库自带预编译**：linux x86_64/arm64、darwin x86_64/arm64（`backend/miot/src/miot/libs/`）。
- miloco 自己的用法：PyAV 解码 → WebSocket 按"文本 init 消息 + 二进制 NAL"广播 → 浏览器 jmuxer/MSE 无插件直播（`miot/ws.py::MIoTVideoStreamManager`，含订阅闲时拆除/背压/录像片段）。

对我们的三条路：
| 路线 | 做法 | 判定 |
|---|---|---|
| a. 最快出效果 | PC/WSL 跑 miloco，MineHome 连它的 WS 消费直播流（自实现 WS 客户端，协议在 ws.py 可读） | **推荐，半天级**；App 里摄像头直播立刻成立 |
| b. 中期自主 | 借 lib ABI 在 PC 跑起来 + Wireshark 观测其网络面，钉死 PPCS/MISS 握手 → 复刻进 Rust | 比盲 RE 快一个数量级，立项评估 |
| c. lib 进 App | linux-arm64 .so 是 glibc，OhOS 加载不了；且 license 禁止 | ❌ 明说不可行 |

## 3. 中枢本地控制：整条协议链有了官方对照本（新花样 ③）

上轮结论"本地模式需要中枢硬件，你家没有"维持不变；但 miloco 把"如果买中枢"的**全部协议细节**公开了，我们报告里三个"未逐行验证"项直接闭环：

| 我们的空白 | miloco 出处 | 要点 |
|---|---|---|
| 中枢 TLS 证书申请链 | `cert.py` + `cloud.py` | **Ed25519** 密钥 → CSR（CN=`mips.{uid}.{sha1(did)}.2`，C=CN，O=Mijia Device）→ `POST /app/v2/ha/oauth/get_central_crt` 签发；CA 内置常量 + SHA256 pin；到期自动续期 |
| `_miot-central._tcp` 发现 | `mdns.py` | **legacy-unicast**（临时源端口，应答单播回来）+ **子网定向广播代替 224.0.0.251 组播**（绕 macOS 组播权限/接口绑定问题）；一次查询带回 PTR+SRV+TXT+A；15s 轮询 |
| 中枢通道语义 | `central_hub.py` + `mips_local.py` | **MQTT over mTLS**，每 group_id 一条连接；载荷=MIPS **TLV 信封**（`len 4B LE + type 1B + data`，类型 ID/FROM/PAYLOAD…）包 miot spec JSON；控制选路**本地优先、云端兜底、本地超时不云端重发**（防双发）；属性推送按设备单源（云端 MQTT 或本地中枢，判据 `pushAvailable`，与控制的 `specv2Access` 两套独立标志） |

Rust 复刻路径：`mdns.py → cert.py → central_hub.py → mips_local.py → client.py`，每步都有可对拍的测试（miot/tests/ 下 27 个）。

## 4. 把 miloco 当"家的大脑"，MineHome 当前端（新花样 ④）

miloco 不只是协议参考，它本身是个能跑的服务：

- **REST/WS API**：`POST /api/miot/devices/{did}/control`、设备状态容器（实时属性）、规则、任务、家庭记忆、`/api/monitor/nodes`。
- **内置 MCP server**（`miot/mcp.py`）：手动场景列表/触发、App 通知、家庭/设备查询——我们的 home-control-mcp 可以直接作为 MCP 客户端接它的场景与通知能力；其 MCP 面设计（i18n、ha_api、资源暴露）值得抄作业（设计层面）。
- **感知事件**：摄像头 Omni（危险分级预警、家庭成员身份、宠物、事件反馈）→ 我们 App 目前完全没有的能力域。给 MineHome 加一个"家里此刻/事件流"面板，数据源=miloco，这就是 App 从"遥控器"升级为"家的界面"的路径。
- 成本提示：感知/Agent 走云端大模型 API（MiMo），持续计费，面板里有用量页。

## 5. 可借鉴的工程细节（低成本高价值）

- `lan.py` 保活状态机：宽限期/离线定时器/跨网段单播补探（广播会被 AP 隔离丢）/按网卡 fan-out/单播 socket 缺失按需补建。
- `message_dedup.py`/`state_align.py`/`state_push.py`：属性推送去重与状态对齐——我们 App 状态面（ monitors/陈旧度）的现成设计。
- 网关 `{"name":"ota_state"}` 形状陷阱：有的推送用 name 而非 siid/piid，且 spec `type_name` 连字符 vs 推送 name 下划线，映射错会**静默写错属性**——miloco 选择白名单丢弃+INFO 而不是猜映射。我们做 spec 驱动渲染时同款坑。
- OAuth2 登录流（`/app/v2/ha/oauth/get_token` + unionid→uid）与我们的密码登录互补——App 长期凭据续期可换这条官方流。

## 6. 落地建议（按投入产出排序）

1. **P0 · WSL 部署 miloco**（`user_guide` WSL 镜像网络三步）→ 绑账号 + 开摄像头感知 → MineHome 接 WS 直播流 + 事件推送。一次部署，同时点亮花样②④。
2. **P0 · OT 发现进 MineHome**：Rust 加 OT 探测/应答编解码（32B 定长，零依赖），设备列表加"局域网在线"标识。
3. **P1 · 摄像头协议专项**：借 `libmiot_camera_lite` ABI 在 PC 跑通后抓包，把 PPCS/MISS 复刻评估立项（对照我们 `camera_protocol_notes.md`）。
4. **P1 · 中枢协议储备**：把 §3 表格内容合并进 `协议面分析报告.md`（闭环三个未验证项），中枢硬件入手后按图施工。
5. **P2 · MCP 互联**：home-control-mcp 增加到 miloco 的 MCP 客户端 transport（场景触发/通知）。

## 7. 本次验证记录

- `192.168.66.255:54321` OT 子网广播 → 3 台应答，did 与 `/v2/home/device_list` 逐台一致（脚本见会话，可并入 `_work/probe_local_ht*.py` 系列）。
- miloco 源码引用：`backend/miot/src/miot/{lan,mdns,cert,central_hub,mips_local,camera,cloud,mcp}.py`、`knowledge/01-overview/overview.md`、`knowledge/03-features/device-control.md`。
