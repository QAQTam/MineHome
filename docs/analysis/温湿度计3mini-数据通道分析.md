# 米家智能温湿度计3 mini 数据通道分析

> 2026-11-22 实测 + 仓库既有逆向证据汇总。
> 设备：**米家智能温湿度计3 mini**，model `xiaomi.sensor_ht.mini`，
> did `<did-温湿度计>`（pid=6 标准米家 BLE），广播 devid `0x55b5`，
> 硬件 `MJWSD06MMC`，MAC `AA:BB:CC:DD:EE:08`（广播明文出现，RPA 帧除外）。
> 问题：能否 **BLE 直连拿数据**？能否从 **米家本地模式/网关本地** 拿 BLE/Zigbee 数据？

---

## 0. 结论速览

| 通道 | 可行性 | 状态 | 延迟/新鲜度 |
|---|---|---|---|
| A. MiBeacon 被动广播解密（无连接） | ✅ **可行** | Rust 已实现（`mijia-core/src/ble.rs`），真机帧已钉死 | 广播周期级（约 10s~1min 一帧） |
| B. BLE GATT 直连 + spec 通道 | ✅ **可行** | 协议已在充电器上全链路闭环（`ble_auth.rs`），本机 spec 官方声明 gatt-access，**待真机 probe 一次** | 秒级（主动读 + properties_changed 推送） |
| C. 云端缓存（现状通道） | ✅ 已在用 | `home-control-mcp` 真机验证 27.4℃/79%/100% | 网关上报级（变化才报，分钟~小时陈旧） |
| D. 米家本地模式/网关本地拿 BLE/Zigbee | ❌ **本家不可行** | 本轮 6 组探测实测：网关裸 miIO 已关闭、非中枢（见 §5） | — |

**一句话**：这台温湿度计的数据，「BLE 直连」两条子路径都通（被动广播已实现+验证，主动 GATT 协议栈已闭环只差一次真机确认）；「从网关本地拿」在你这台 mgl03 上走不通——你的记忆对应的是**小米中枢网关**的本地 MQTT 能力，那需要中枢网关硬件，mgl03 不具备。

---

## 1. 设备身份与绑定拓扑

- pid=6 = 标准米家 BLE（与酷态科充电器同类，**不是 BLE mesh**；早期 SESSION_STATE 里把 pid=6 写成"BLE mesh"是口径错误，已由充电器实测纠正——pid6 设备云 RPC 被拒 `bluetooth device does not support sending rpc`，数据走 miotspec 缓存）。
- 它经 **网关（lumi.gateway.mgl03）的蓝牙网关功能** 接入米家：网关替它连云、把属性变化上报云端缓存。BLE 直连时**不需要网关参与**（蓝牙网关只是延长覆盖 + 喂云缓存）。
- miot-spec 实例（released，`urn:...:temperature-humidity-sensor:0000A00A:xiaomi-mini:1:0000D063`）：

| 服务 | 属性 | format | cloud access | **gatt-access** |
|---|---|---|---|---|
| s1 设备信息 | p1-5 厂商/型号/ID/固件/SN | string | read | **read** |
| s2 温湿度 | p1001 温度 | float -30..100 (0.1℃) | read+notify | **read+notify** |
| s2 | p1002 湿度 | uint8 0..100% | read+notify | **read+notify** |
| s3 电池 | p1003 电量 + 低电量事件 | uint8 | read+notify | **read+notify** |

> `gatt-access` 是官方对「这些属性支持 GATT 直连读写/推送」的显式声明——通道 B 的权威依据。
> 注意云端读是**自定义号 1001/1002/1003**，标准号 s2/p1|p2 报 -704040003（DESIGN.md 已实测）。

---

## 2. 通道 A：MiBeacon 被动广播（无连接，已实现）

仓库证据链（全部真机）：

- `mijia-core/src/ble.rs`：V2/V3 legacy + **V4/V5** 帧解析与 AES-CCM\* 解密；V4/V5 强制 MIC 校验。
- 关键布局（`_work/v5_brute.py` 真机爆破 + `_work/v5_golden.py` 对拍）：nonce(12B) = `mac逆序6B ++ devid2+cnt1 ++ salt3`；**密钥 = 云端 device token（12B）按 legacy magic `8d3d3c97` 扩展**，MIC 全通过（`ble.rs` 注释「真机温湿度计 3 mini 实测走这版」）。beaconkey 路线非主路径。
- 明文 object TLV：`0x100d` 温湿度 4B（T/H 各 int16 ÷10）、`0x1004` 温度、`0x1006` 湿度、`0x100a` 电量（`try_beaconkey.py` 有完整语义表）。
- 两类帧：带 MAC 帧（0x5859…）与省电无 MAC 帧（0x4859…）——后者 nonce 需要外部已知 MAC（上层单目标扫描时已知）。
- token 实测稳定：老端点 `/v2/home/device_list` 与 `device_list_page` 的 token 逐台一致（2026-11-22 复核，16 台全部 `==`）。

落地：ArkTS `@ohos.bluetooth.ble` 扫描过滤 0xFE95 服务数据 → base64 → napi `mijia-core::ble::decode_adv_mac`。**无需连接、无需配对，一次登录拿 token 后永久可解**。代价：只有广播频率的新鲜度，且息屏/省电模式下帧率降低。

## 3. 通道 B：BLE GATT 直连 + 0x001B spec 通道（协议已闭环，待一次真机 probe）

协议已在充电器（同 pid=6、同 GATT 服务表）上端到端实测（`docs/analysis/蓝牙标准认证协议.md`）：

1. GATT 服务 0xFE95：0x0010 命令通道 + 0x0019 认证流 + 0x001B spec 数据通道。
2. 登录：`24 00 00 00` 起手 → HKDF-SHA256(ikm=**12B token**, salt=appRandom‖devRandom, info="mible-login-info") → 双向 HMAC confirm → 0x21 成功。
3. 0x001B 通道：AES-CCM（RX key=okm[0:16]，TX key=okm[16:32]），明文 = **miot SPEC v2 帧**（opcode get/get_rsp/properties_changed）。
4. Rust 已实现：`mijia-core/src/ble_auth.rs`（HKDF/HMAC/CCM/SPEC 帧编解码，cargo test 通过）；ArkTS 只需 GATT 字节搬运。

对本机的适用性：spec 实例声明 p1001/p1002/p1003 **gatt-access read+notify** ⇒ 官方支持直连读+推送。预期行为：登录后 opcode=get 读三属性，且属性变化设备主动 push（opcode=4）——正是温湿度场景的理想通道。**未决**：本机固件 0x001B 的实际推包频率/是否需要先版本交换（opcode 0xF0），一次真机 probe 即可钉死（复用 `_work/ble_login.py` 换 MAC 即可）。

## 4. 通道 C：云端缓存（现状，已验证）

`/miotspec/prop/get` 返回网关上报的缓存值 + updateTime：DESIGN.md §9.2 真机 27.4℃ / 79% / 100%。局限：BLE 子设备云 RPC 不可用（只有缓存读），新鲜度取决于网关上报策略。

## 5. 通道 D：米家本地模式 / 网关本地拿 BLE/Zigbee —— 实测不可行

### 5.1 本轮实测（2026-11-22，`_work/probe_local_ht*.py` 六组）

| # | 实验 | 结果 |
|---|---|---|
| 1 | mDNS 组播浏览（zeroconf，8s）`_miot-central._tcp` / `_miio._udp` | **0 条** |
| 2 | 网关单播 mDNS（192.168.66.144:5353，3 个 qname） | **无响应**（连 `_services._dns-sd._udp` 都不答） |
| 3 | miIO UDP hello → 校验 `checksum==MD5(token)`（新旧 token、双端点） | 响应=**请求原样回显**（stamp=0，checksum=ffff…），不匹配 |
| 4 | 全网段 192.168.66.2-254 hello 扫描（32 并发） | 仅 .144/.205/.36 三台响应，**全部为 hello 回显** |
| 5 | 垃圾包对照 + 非零 stamp hello + 广播 255.255.255.255 + TCP 54321/54322/9898/6668 | 垃圾包**不回**；非零 stamp **超时**；广播 **0 响应**；TCP 全部 refused |
| 6 | 用云端 token 加密 `miIO.info`/`get_subdev_list`（mesh/zigbee/ble） | **全部超时**（密文命令不被处理） |
| 7 | 身份锚定：`arp -a` | **`192.168.66.144 ↔ AA-BB-CC-DD-EE-01` = 云端清单网关 MAC，逐字节一致** → 响应者确系 mgl03 本尊，非 IP 漂移 |

结论：**当前固件的网关对局域网只留了一个「hello 回显」探活口，裸 miIO 命令通道已关闭**；也没有中枢网关的 `_miot-central._tcp` mDNS 足迹。

### 5.2 与生态事实对齐（官方 HA 小米集成 README，2026-11 抓取）

- 米家**本地模式** = **小米中枢网关**（固件 ≥3.3.0_0023）或**内置中枢网关的设备**（软件 ≥0.8.9，清单见其 wiki Central-hub-gateway-device-models）提供的**局域网 MQTT broker**；没有中枢网关时一切走云。
- **Xiaomi LAN control（miio UDP）只能控 IP 设备，"cannot control BLE Mesh, ZigBee, etc."**——官方白纸黑字。
- 中枢网关仅中国大陆，本家用例中无任何内置中枢设备 → 本地模式无锚点。

### 5.3 你记忆中「直接跟网关拿 BLE/Zigbee 数据」的出处

这条记忆对应三个真实存在的东西，但都不属于这台网关的当前固件：

1. **老 mgl03 固件时代的 miIO UDP 本地控制**（社区常用：网关 token + `get_prop`/`zb_end_provision` 等，APK 内 `MiioLocalAPI` 也确实实现了 `miIO.get_subdev_list`）——本轮实测该口已关（§5.1）。
2. **米家 App 的 LocalControl SDK（lan_dm）**：`liblocalcontrol.so` 以网关为一台 miIO 设备走 UDP（`lan_dm_init_device_cipher` 按设备加密 → `get_subdev_list`/`sendRPC`）——通道存在于 App 内，但同样落在被关掉的 UDP 面 + 私有加密握手，复刻需 RE `liblocalcontrol.so`，且网关侧是否仍接受未验证（hello 回显不等于 lan_dm 已开）。
3. **中枢网关本地 MQTT**（真正的"本地模式"，BLE/Zigbee 全覆盖）——需要中枢网关硬件。

> 若未来把网关换成**小米中枢网关**（或在有内置中枢的设备旁），本项目的落地顺序建议从 协议面分析报告 §5.3 走：mDNS `_miot-central._tcp` + CentralProfile 校验 + TLS 客户端证书（`miIO.get_central_link_passcode` 配对一次）+ topic API，即可本地拿全屋 BLE/Zigbee spec 数据。

---

## 6. MineHome（本项目 App）落地建议

1. **立即收益 = 通道 A**：Rust 侧 `ble::decode_adv_mac` 已完成并带 golden 测试；ArkTS 加一个 BLE 扫描 + 0xFE95 过滤 + napi 调用即可把「温湿度计实时卡片」做出来（token 已在 session 里）。同时覆盖手环/门磁等其它 BLE 广播设备。
2. **次一步 = 通道 B**：`ble_auth.rs` 补 GATT 传输层（`@ohos.bluetooth.ble` 连接 + 0x0010/0x0019/0x001B 三个 characteristic），先用 `_work/ble_login.py` 在 PC 对本机做一次 probe 钉死 0x001B 行为，再上真机。收益：秒级新鲜度 + 主动读。
3. **通道 C 保持现状**：云端缓存作为兜底（设备离手机蓝牙范围外时）。
4. **通道 D 不投入**：除非增加中枢网关硬件。

## 7. 未决项

- [ ] 本机温湿度计 0x001B spec 通道真机 probe（推包频率、版本交换、get 响应形状）。
- [ ] `liblocalcontrol.so` lan_dm 加密握手 RE（只有想复活网关本地通道才值得做）。
- [ ] 省电无 MAC 帧（0x4859）在多台同型号并存时的 MAC 归属策略（RPA 轮换）。
- [ ] 温湿度计广播帧率与息屏/省电模式的关系（影响通道 A 的新鲜度预期）。
