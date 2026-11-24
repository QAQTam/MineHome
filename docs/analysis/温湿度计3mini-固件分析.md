# 温湿度计 3 mini（MJWSD06MMC）固件分析：广播触发 / LCD 蓝牙图标 / GATT 直连

> 2026-11-23。
> 设备：米家智能温湿度计 3 mini，model `xiaomi.sensor_ht.mini`，硬件 **MJWSD06MMC**，
> 广播 devid `0x55b5`，本项目实测 MAC `AA:BB:CC:DD:EE:08`（稳定）+ `AABBCCDDEE09`（RPA）。
> 原厂固件 **2.1.1_0009**（二进制取证），SoC **Telink TLSR8251**，传感器 SHV4（SHT4x 系）/ GXHT40（克隆），LCD 驱动 AIP31620E。
> 证据来源标注：〔固件〕=原厂 fullflash 二进制取证（本会话，工件在 `_work/`）；〔pvvx〕=pvvx 仓库/wiki；〔flasher〕=mj6-flasher 源码；〔实测〕=本项目真机 hilog/协议实测；〔推断〕=待真机验证。
> 姊妹文档：`温湿度计3mini-数据通道分析.md`（A/B/C/D 四通道可行性）、`蓝牙标准认证协议.md`（充电器协议闭环）。

---

## 0. 结论速览

| 问题 | 结论 | 置信度 |
|---|---|---|
| ①广播触发条件 | 稳态周期突发：**每 2~3min 一个突发 = 6 帧 0x4859 + 6 帧 0x5859**（帧间隔 ~100-150ms，突发总长 ~1.9s，实测间隔 62s/120s/179s 波动）；**原厂间隔不可配置**；绑定模式切高频快闪（新帧型 0x1059 密集 → 0x3058 每 60s）；变化/短按触发待分离验证 | 突发结构=2026-11-23 PC 扫描实测实锤 |
| ②LCD 蓝牙图标 | **闪烁 = 绑定/配对模式**（真机+同代 #813 双实锤）；**常亮 = GATT 连接中**（**2026-11-23 真机实锤**：PC 连上后图标立刻常亮）；长按 ~5s 进绑定模式（屏幕闪烁） | 双实锤 |
| ③GATT 直连 | **mible 标准认证协议实锤**（0xFE95 + 0x0010/0x0019，三源证据链）；**06 实机服务表已 dump**：spec 通道是 **0x001A 单通道双向（writeNoRsp+notify）**，无 0x001B/0x0005/0x001C；**免登录快路径实锤**：ebe0ccc1 直读 `24.40℃/64%/3.040V`、ccc4 电量、0004 版本；spec get/推送需登录（未登录写入被接受但静默） | 实机 probe 实锤（登录 get 待 token） |
| 连接性（新发现） | **常规广播（0x4859/0x5859）为不可连接（NONCONN）**——Windows 两种地址类型 + bleak 内部扫描均拒绝；**仅绑定模式广播（0x1059/0x3058）可连接**。米家「有概率连上」= 赶上了可连接窗口 | PC 侧三轮连接实验实锤 |

**对产品方向的意义**：档位 B 的落地形态修正——**App 无法随时直连常态设备**；「即连即读」需要设备进入可连接窗口（长按绑定模式，或米家自己的连接机制）。免登录快路径（connect→read ebe0ccc1→disconnect，全程 <5s）在绑定窗口内成立；mible 登录协议与 `ble_auth.rs` 同栈，通道号改 0x001A 即可。

---

## 1. 设备身份与固件代际

〔固件〕本项目下载了 pvvx 抓取的原厂 fullflash（512KB，`_work/mjwsd06mmc_ff.bin`）做字符串取证：

- 版本串：`2.1.1_` + `0009` → 原厂固件 **2.1.1_0009**，与 pvvx issue #636 标题一致（[pvvx]）。
- 设备名：`MJWSD06MMC`（GAP 设备名/型号串）。
- **`mible-login-info`** 字符串：与充电器逆向出的 HKDF info 标签逐字一致 → 固件内置 mible 标准认证协议。
- 特征名表（运行期 GATT 表构建用）：`standard`、`spec_rd`、`spec_version`、`spec_wr`、`Authentication`、`ota_ctrl`、`ota_data`、`sys_info`，以及秒秒测家族历史数据服务名（`Data Notify`、`Current Temp and Humi`、`Battery`、`Time`、`Data Read`、`Data Count`、`Reset Data`、`Disconnect`、`Change Time Mode`、`Set Notify Size`）、调试残留（`debug protocol`、`factory check`、`properties_changed test 2.4!!!`）。
- `properties_changed test 2.4!!!` 测试串直证固件实现了 **miot SPEC properties_changed 推送**（spec 通道 opcode=4）。

〔pvvx〕硬件信息：TLSR8251 + SHV4（SHT4x 系 Sensirion）/后期批次 GXHT40 克隆 + AIP31620E LCD；原厂功耗实测 avg 26~29µA、睡眠 11µA、RF TX 15mA（https://pvvx.github.io/MJWSD06MMC/ ）。

**代际对齐**〔pvvx/实测〕：MJWSD05MMC 原厂 2.0.0_0026 / MJWSD06MMC 2.1.1_0009 / 酷态科充电器 2.1.2_0073——同一版本体系、同一 mible 协议栈世代。充电器协议已在 `ble_auth.rs` 全链路闭环，这是复用判断的基石。

---

## 2. 广播刷新触发条件（问题①）

### 2.1 实测行为〔实测〕

本项目 hilog（2026-11-22，手机侧）+ PC 侧 bleak 扫描（2026-11-23，`_work/ble_scan_mj6.py`/`native_probe.log`）：

- **稳态突发结构**（PC 侧毫秒级时间戳）：每突发 = **6 帧 0x4859（无 MAC 省电帧）+ 6 帧 0x5859（带 MAC 帧）**，帧间隔 ~100-150ms，突发总长 ~1.9s；样本：00:46:19.963~00:46:21.844 连发 12 帧。
- **突发间隔**：实测 00:41:20 → 00:43:20 → 00:46:19 → 00:47:21 = 120s / 179s / 62s——**62~180s 波动**，与手机侧 1~3min 结论吻合（可能是 180s 定时 + 事件提前触发）。
- 均为 MiBeacon V5 加密帧（`ble.rs` 两种帧控都已解）。
- **绑定模式帧型**（长按后）：入口瞬间 **0x1059 明文帧密集快闪**（`1059 b555 <cnt> <mac反序>`，无加密无 object，帧控 bit12=带 MAC），持续 ~2s；之后进入稳定绑定态 **0x3058 每 60s 一帧**。
- 手机侧解帧到卡片刷新毫秒级；新鲜度瓶颈完全在设备广播周期。

### 2.2 触发器结构分析

原厂固件广播逻辑在 TC32 机器码里，本轮未做反汇编级确认；结合协议结构与同代设备证据，触发器候选如下：

| 触发器 | 状态 | 依据 |
|---|---|---|
| 稳态周期突发（62~180s） | **实测实锤** | 〔实测〕PC 扫描毫秒级时间戳 |
| **绑定模式入口快闪**（0x1059 密集 ~2s → 0x3058 60s 周期） | **实测实锤** | 长按后广播切换，可连接 |
| 温湿度变化（越阈值）立即广播 | 〔推断〕待验证 | MiBeacon 事件帧惯例；**V1 实验钉死** |
| 短按（切页）触发一帧 | 〔推断〕待分离验证 | 短按时间点与周期突发重合过；绑定模式下短按无屏幕反应、未见新帧 |
| GATT 连接期间 | 广播转绑定态（0x3058 持续） | 连接中扫描仍见 0x3058 帧 |

能耗侧写〔pvvx〕：原厂平均 26~29µA、TX 15mA——秒级常发在这个功耗预算下站不住，稀疏突发 + 事件触发是原厂的省电设计。**这意味着「靠广播拿到秒级新鲜度」在原厂固件上无解，秒级必须走 GATT 直连（§4/§6）。**

### 2.3 能否更快？

- 原厂固件：**不可配置**。广播间隔是编译期策略，米家 App 也没有任何下发间隔的机制（spec 属性里没有相关项）。
- 刷 pvvx 自定义固件：广播间隔可配 10~60s + 事件即时帧（BTHome v2 / ATC / MIJIA 加密格式自选）——但见 §7 的生态取舍。
- **不刷固件的更快的路 = 档位 B GATT on-demand**（§6）：弹层打开时主动连一次，秒级新鲜、读完断开。

---

## 3. LCD 蓝牙图标条件（问题②）

| 状态 | 触发 | 证据 |
|---|---|---|
| **闪烁** | 绑定/配对模式（长按 ~5s 进入，屏幕闪烁） | 〔实测〕2026-11-23 真机：长按 5s 屏幕闪 + 蓝牙图标闪，同时广播切 0x1059 快闪；〔pvvx〕#813 同代 MJWSD05MMC 原文 "bluetooth icon flashing" |
| **常亮** | **GATT 连接中** | 〔实测〕**2026-11-23 真机实锤**：PC `GattSession` 连上后用户即刻观察图标常亮（"长按后很快蓝牙icon就常亮——意味着连接成功"） |
| 熄灭 | 稳态广播/空闲 | 〔实测〕默认观察 |

要点：

1. **图标不是"可连接"开关**：常规态设备就不可连接（见 §4.5），图标亮/闪只反映"绑定模式/连接中"。
2. **进入绑定模式**：长按 ~5s 至屏幕闪烁〔实测〕。⚠️ 用户实测发现长按效果等同"重置"——设备进入未绑定等待态（帧型变明文 0x1059/0x3058），**绑定超时未完成配对后应回到常态**；**勿在米家 App 里重新添加**（会产生重复设备）。退出绑定模式的确切行为（超时时长、是否自动复原）待观察。
3. **短按在绑定模式下无效**（屏幕不切页、无可见新帧）——短按的功能只在常态有意义〔实测，单次观察〕。
4. 对照：pvvx 自定义固件 Button=Connect → 短按开 80s BT4.0 可连接窗口（[pvvx] README）——即自定义固件把"可连接"变成按键门控。**原厂常态不可连 + 绑定模式可连**，与自定义行为同构但入口不同。

---

## 4. GATT 直连路径（问题③，核心）

### 4.1 GATT 服务表（06 实机 dump，2026-11-23）

〔实测〕PC 原生连接（`_work/ble_native_quick.py`，绑定模式窗口内）+ UNCACHED 枚举：

**0xFE95 服务**：

| char | props(winrt 位) | BLE 语义 | 用途（名表对应〔固件〕） |
|---|---|---|---|
| 0x0004 | 2 | read | `Version` = **`2.1.1_0009`**（免登录可读） |
| 0x0010 | 20 | writeNoRsp + notify | `Authentication`（0x24 login / 0x21 成功 / 0x23 拒绝）——同充电器 |
| 0x0017 | 24 | write + notify | `ota_ctrl` |
| 0x0018 | 20 | writeNoRsp + notify | `ota_data` |
| 0x0019 | 20 | writeNoRsp + notify | `standard`（认证流，分片 confirm/random）——同充电器 |
| **0x001A** | **20** | **writeNoRsp + notify** | **spec 双向通道（RX notify + TX writeNoRsp 同一特征）** |

⚠️ **06 实机没有 0x0005、0x001B、0x001C**——与同代 MJWSD05MMC dump（有 0x001A spec_rd + 0x001B spec_wr）不同，06 固件把 spec 收发合并到 0x001A 单特征，与充电器（0x001B 单特征双向）完全同构。

**ebe0ccb0 服务（秒秒测家族，14 特征）**：ccb7 Time(read/write, 运行秒数)、ccb9 Data Count(read, u64=11737 条历史)、ccba/ccbb Data Read(read)、ccbc Data Notify(notify)、ccbd/ccbe/ccd4-reset/cce4/ccea 控制、**ccc1 Current Temp and Humi(read+notify)★**、ccc2 Set Notify Size(writeNoRsp)、ccc4 Battery(read, u8 %)、ccc8(writeNoRsp)。

**其它**：0x1800/0x1801 常规、0x180A 设备信息（本轮 UNCACHED 枚举时未列 2A2x 子特征——下次 dump 补）、Telink OTA `00010203-…-1912`（char …2b12 props=6 write+notify?）、0xFF00 debug、fafafa00 factory。

> 每次连接 dump 服务表用 **`BluetoothCacheMode.UNCACHED`**——cached 模式返回空（Windows GATT 缓存坑，实测踩过）。

### 4.2 认证协议与 `ble_auth.rs` 的同一性（三源证据链）

1. **〔固件〕** fullflash 含 `mible-login-info` 字符串——HKDF info 标签逐字一致（`蓝牙标准认证协议.md` §4：`okm = HKDF-SHA256(ikm=12B token, salt=appRandom‖devRandom, info="mible-login-info", L=64)`）。
2. **〔flasher〕** mj6-flasher/index.html（pvvx TelinkMiFlasher v14.1 同源）的 Mi 登录实现：
   - MJWSD06MMC 识别分支：`bluetoothDevice.name == "MJWSD06MMC" && devinfo.frstr.startsWith("0009")` → 展示 Mi Token + Mi Bind Key 登录 UI（源码 2357 行附近）；
   - `gattServer.getPrimaryService(0xfe95)` → `getCharacteristic(0x0010)` + `getCharacteristic(0x0019)`（1952~1960 行）；
   - `sendLogin()`：先 `mainCharSend("24000000", 0x0010)`，随后流帧 `0000000b0100`（CMD start，type=0x0B=11=appRandom）→ 0x0019 ——**与 `_work/ble_login.py`（充电器闭环脚本）逐字节同协议**。
3. **〔pvvx〕** issue #636：对原厂 2.1.1_0009 实测「Enter Mi Token / Mi Bind Key → **Login** 成功」后执行 OTA（附截图）。

结论：**`mijia-core/src/ble_auth.rs` 的登录栈（HKDF + 双向 HMAC confirm + 0x0019 分片流状态机）直接适用于温湿度计**；ikm = 云端 `device_list_page.token`（12B，项目 session 已有）。充电器的血泪教训同样生效：设备每连接只评估第一个 appConfirm，错 token 回 0x23 后静默。

### 4.3 spec 通道：06 实机 = 0x001A 单通道双向（与充电器 0x001B 同构）

| | 充电器（njcuk，实测闭环） | **温湿度计 06 实机（2026-11-23 dump）** |
|---|---|---|
| spec 通道 | **0x001B**（writeNoRsp+notify 双向单特征） | **0x001A**（writeNoRsp+notify 双向单特征） |
| 认证命令 | 0x0010 | 0x0010（相同） |
| 认证流 | 0x0019 | 0x0019（相同） |

→ **`ble_auth.rs` 的 SpecRxState/CCM 解密/组帧一字节不用改，只把通道号常量 0x001B 换成 0x001A**（ArkTS 侧订阅与写都指向 0x001A）。spec get 帧格式（opcode=2，APK `kt4` 组帧器逆向）：

```
Get:  [0x2000|total_len u16 LE][tid u16 LE][0x02][count u8] + count×[siid u8][piid u16 LE]
      例：get s2.p1001/p1002 + s3.p1003 → len=15 → 0f 20 <tid> 02 03 02 e903 02 ea03 03 eb03
Set:  [0x2000|len][tid][0x00][count] + count×[siid u8][piid u16][vtype<<12|vlen u16][value]
Ver:  [0x2005][tid][0xF0]      # 版本交换，r5e/R2 常量 8197 交叉验证
```

未登录写入实测：0xF0 与 get 均被物理层接受（status=0 无协议错误）但设备**静默**——spec 数据面受 mible 登录门控，与充电器一致。

### 4.4 免登录快路径：**已实锤**（2026-11-23）

〔实测〕未登录直读数据（设备绑定模式窗口内，`_work/ble_native_quick.py`）：

| char | 值 | 解码 |
|---|---|---|
| 0x0004 | `'2.1.1_0009'` | 固件版本（明文） |
| **ebe0ccc1** | `88 09 40 e0 0b` | **temp=int16 LE ÷100 = 24.40℃、humi=u8 = 64%、电压=int16 LE ÷1000 = 3.040V**（5 字节） |
| ebe0ccc4 | `0x64` | 电量 100% |
| ebe0ccb9 | `f9 2d 00…` | 历史记录条数 u64 = 11737 |
| ebe0ccb7 | 递增 | 设备运行秒数（592→663→826 随连接间隔递增） |

**ccc1 编码交叉验证**：MJWSD05MMC 同特征 dump 值 `06 09 1c 99 0c` = 23.10℃ / 28% / 3.225V——两台设备两种解释完全自洽，格式定案。连接期间重读 ccc1 数值随环境变化（24.40→24.30，设备被移动），为实时值。

**notify 也可免登录（2026-11-24 真机实锤）**：`setCharacteristicChangeNotification(ebe0ccc1, true)` 订阅成功后，设备在**数值变化时主动推**——App 侧观察到 24.20↔24.30℃ 边界以 6s 节奏来回推、以及电压 3.053→3.052V / 温度 24.40→24.50℃ 的单向变化即推（无变化时静默）。即"档位 B 保活 + 秒推"不需要 mible 登录；spec 通道的 `properties_changed`（opcode 4）仍受登录门控（V3.3/V3.4）。

→ **档位 B 快路径成立**：`connect → read ebe0ccc1 + ccc4 → disconnect`，全程 <5s，无需 token。前提是设备处于可连接窗口（§4.5）。

未登录 spec（0x001A 写 0xF0/get）：写入被接受但设备静默 → spec get/properties_changed 推送需要 mible 登录（V3.3/V3.4 待 token）。

### 4.5 连接策略：**常态不可连接，绑定模式才可连**（2026-11-23 实测推翻旧结论）

〔实测〕PC 侧三轮连接实验（bleak connect ×2 + 原生 winrt ×2）：

| 设备状态 | 广播帧型 | 可连接性 |
|---|---|---|
| **常态**（稳态广播） | 0x4859/0x5859 突发 62~180s | **不可连接**——bleak connect `BleakDeviceNotFoundError`；原生 `from_bluetooth_address_async` 返回 null（PUBLIC/RANDOM 双类型均试，扫描保持活跃也一样）→ 承载 PDU 为 ADV_NONCONN_IND |
| **绑定模式**（长按 ~5s） | 入口 0x1059 密集快闪 ~2s → 稳态 0x3058 每 60s | **可连接**——原生 API 秒连（连接期间 0x3058 持续可见） |

**PC 侧成功连接配方**（`_work/ble_native_quick.py`）：bleak `BleakScanner` 保持活跃（Windows watcher 依赖）+ `BluetoothLEDevice.from_bluetooth_address_async(mac_int, BluetoothAddressType.PUBLIC)` + `GattSession.maintain_connection=True` → 绑定模式开启后数秒内 Connected。**bleak 的 `BleakClient.connect()` 对此设备不可用**（内部发现阶段等不到突发）。

对 〔pvvx〕#636「TelinkMiFlasher 直接连接」的再解读：flasher 连接必然发生在设备处于绑定模式时（其 OTA 流程前的注册/配对环节），或 Web Bluetooth chooser 只列可连接设备——**不能作为"常态随时可连"的证据**。米家 App「有概率连上」（用户原始观察）= 赶上了可连接窗口；米家如何触发该窗口（绑定模式？短按开窗？网关通道？）**未解，影响档位 B 交互设计**。

---

## 5. 真机验证清单

> 流式日志：`hdc tconn 192.168.66.179:45413` → `hilog | grep <pid> | grep -E "miot-ble"`（快照用 `hilog -x` 会滚动）。判定帧出现 = `ble reading mac=…` 或 `multi hit`。

### V1 广播触发器（10 分钟搞定）

| # | 操作 | 判定 |
|---|---|---|
| V1.1 | 静置 10min 基线（对照已有 1~3min 突发数据） | 间隔分布吻合 |
| V1.2 | 吹气加湿（湿度 +5~10%） | <10s 出帧 ⇒ 变化触发实锤 |
| V1.3 | 手捂升温（+1℃） | 同上，交叉验证 |
| V1.4 | 短按按键（切页） | 立即出帧 ⇒ 按键触发实锤；无帧 ⇒ 切页纯本地 |

### V2 蓝牙图标

| # | 操作 | 判定 |
|---|---|---|
| V2.1 | 长按梯度 3s/5s/8s | 屏幕闪烁或蓝牙图标闪 = 绑定模式入口时长钉死；同时 hilog 广播频率显著上升 |
| V2.2 | App 发起 GATT connect（V3 脚本或 App） | 连接期间图标常亮、断开熄灭 ⇒ 常亮=连接中实锤 |

### V3 GATT probe（工具 `_work/ble_probe_mj6.py`，已移植 native 连接配方）

> ⚠ **连接前置**：设备必须长按 ~5s 进绑定模式（常态 NONCONN 不可连，§4.5）。
> probe 连接器可挂起等长按；bleak `BleakClient.connect()` 对此设备必失败，
> 必须走 native 配方（§4.5）。

| # | 操作 | 判定 | 状态 |
|---|---|---|---|
| V3.1 | connect + dump 服务表 | 对齐 §4.1 | ✅ 0x001A 双向 spec、无 0x0005/0x001B/0x001C |
| V3.2 | **未登录**读 `ebe0ccc1`/`ccc4`/`0x0004` | 可读性 + 编码格式钉死 | ✅ 编码定案（§4.4），快路径成立 |
| V3.3 | mible 登录（0x24 → 0x0019 流） | 0x21 成功 ⇒ 协议同一性终验 | ⏳ 待云 token（用户跑 `do_login.py`） |
| V3.4 | 0x001A write spec get（s2.p1001/p1002、s3.p1003） | 0x001A 收 get_rsp，值与屏幕对拍 | ⏳ 待 V3.3 |
| V3.5 | 挂 30min 观察 properties_changed | 推送频率（变化时是否秒推）钉死 | ⏳ 待 V3.3（`--listen` 长挂） |
| V3.6 | 故意错 token | 0x23 静默（复现充电器负样本行为） | ⏳ 待 V3.3 |

### V4 档位 B 端到端（App，2026-11-24 真机首验）

| # | 操作 | 判定 | 状态 |
|---|---|---|---|
| V4.1 | 打开设备弹层（设备在可连接窗口） | <5s 显示 GATT 直读值，读完自动断开 | ✅ **实锤**：`miot-ble-gatt: gatt read ok mac=a4c138dd97f0 t=2108ms raw=fc083bed0b bat=64`（23.00℃/59%/3.053V/100%），二次 2350ms；横幅 `23.4℃ · 60% · 电量 100% · 3.06V · 22:14:56（GATT直读）`，弹层温湿度磁贴带「蓝牙直收」标，**关弹层后首页卡片 24.4℃/64% → 23.4℃/60% 即时追平**；bt 栈日志确认读完 HCI 即断 |
| V4.1b | 打开设备弹层（设备常态不可连） | 横幅提示「长按 ~5s 进绑定模式」而非静默失败 | ⏳ 代码路径已备（connect 10s 超时→`not_connectable`）；设备仍滞留绑定窗口，待回常态实测 |
| V4.2 | 手机断网再开弹层 | 仍能秒级刷新（quickRead+bleIngest 全程不触网） | ⏳ 待人工断网复测 |
| V4.3 | 米家 App 同时打开插件页后立即连 | 竞争/退避行为（连接独占） | ⏳ 未测 |
| V4.4 | **保活会话 + notify 秒推**（形态 A） | 弹层停留期间连接保持、变化即推、关弹层即断 | ✅ **实锤**（2026-11-24）：`ccc1 notify armed → session open t=2020ms live=true`；推送序列 88093bed0b → 88093ced0b（电压 3.053→3.052V）→ 92093bed0b（温度 24.40→24.50℃），无变化时静默；另一轮 22:29:31 订阅后连推 9 条、会话存活 >2min；横幅 `24.4℃ · 60% · 电量 100% · 3.05V · 22:37:11（GATT实时）` |

---

## 6. App 档位 B「打开弹层即连即读」落地设计（2026-11-23 按 §4.5 修订）

### 6.0 连接性前提（实锤，设计的第一约束）

常态广播 ADV_NONCONN_IND **不可连接**；只有绑定模式（长按 ~5s）有可连接窗口。
米家「有概率连上」= 赶上窗口（米家如何主动触发窗口**未解**，§9）。交互形态选型：

- **形态 (a) 弹层内保活（✅ 2026-11-24 已实现并真机闭环）**：弹层打开即 connect，**保持会话**直到弹层关闭（或 5min 到点）；期间订阅 ebe0ccc1 notify 做"变化秒推"（§4.4），notify 订阅失败自动退化为 10s 轮询。连不上（10s 超时）→ 横幅提示「长按设备 ~5s 至屏幕闪烁后再打开弹层」。
  选择保活而非"读完即断"的理由：① 常态不可连 → **连接本身是稀缺资源**，断了可能再也进不去；② 建链开销远大于空闲监听，连着推反而更省；③ 用户已确认"会话期间米家连不上"可接受。
- 形态 (b)（复刻米家机制）待未解项 4 解开后评估；形态 (c)（后台常驻长连接）需 `KEEP_BACKGROUND_RUNNING` + `BLUETOOTH_INTERACTION` 长时任务，代价（电池 10~40×、长期独占、断线不可回）暂不采用。

### 6.1 双轨通道

```
打开弹层 ──┬─ 免登录会话（✅ 已闭环）：connect(≤10s) → 串行 read ebe0ccc1 → read ccc4
           │    → 订阅 notify → **保活**（变化即推，无变化静默；订阅失败退化 10s 轮询）
           │    → 关弹层 / 5min 到点 → 断
           │   解码在 Rust（bleParseHt），合成 MiBeacon JSON 落 ble_cache → apply_local
           └─ 登录路径（待 V3.3/V3.4 真机钉死后上）：同一条会话内 mible login(ble_auth.rs)
               → 0xF0 → spec get(s2.p1001/p1002, s3.p1003) → get_rsp → 0x001A 订阅
               properties_changed → 离开弹层即断
```

**前置**：两轨都要求设备处于可连接窗口（§4.5）。快路径失败提示长按；
登录路径失败回退快路径；两者都失败回退现状（广播突发 + 云缓存）。

### 6.2 改动点清单（2026-11-23 状态）

**Rust（mijia-core，cargo test 全绿）**：
1. ~~spec 通道号参数化~~ → **实机 dump 后修正：Rust 侧本无通道常量**。0x001A/0x001B
   是 GATT 特征选择问题，由 ArkTS 按型号选（温湿度计 0x001A 单通道双向，充电器 0x001B）。
2. ✅ TX 组帧：`build_spec_get` / `build_spec_ver` / `build_stream_frames` /
   `build_stream_ack` / `LOGIN_CMD`（golden 与实机 probe 手搓帧逐字节对拍）。
3. ✅ 快路径解码：`parse_ebe0ccc1`（06 实机 5B 格式，05 样例交叉验证）+ `parse_ebe0ccc4`。
4. ✅ napi 出口：`bleParseHt` / `bleBuildSpecGet` / `bleBuildVerFrame` /
   `bleBuildStreamFrames` / `bleSpecEncrypt` / `bleLoginCmd`；BLE_SESSION 槽加 SpecTxState
   （TX sn/roll 会话态，登录路径下行加密用）。
5. ✅ 落库链路复用既有 `ble_cache::ingest` → `apply_local`（本地优先合并规则不动）。

**ArkTS**：
1. ✅ 新建 `BleGatt.ets`：`createGattClientDevice → connect(10s TO) → getServices →
   read ebe0ccc1/ccc4 → napi 解码 → 合成 MiBeacon JSON 走 bleIngest → disconnect`。
   fail 分类：`bt_off` / `not_connectable`（提示长按）/ `gatt_error`；busy 防并发。
2. ✅ `Index.ets` 弹层 onShow → `BleGatt.open(mac, push, closed)`（型号白名单门控）；
   首读/推送/会话终止三路上报 → 横幅 `bleText` + `reloadDetail(false)`（apply_local 合并）
   + 首页卡片 throttle 追平；弹层 `onDisappear` → `BleGatt.close()`。
3. RPA 设备（`AABBCCDDEE09` 类）：用最近一次广播帧的 addr 直连（RPA 轮换后连不上 →
   等下一帧刷新地址重试）；stable-MAC 机（`A4C138DD97F0`，Telink 公网地址段）直连无忧。

### 6.3 约束与兜底

- **连接独占**：设备一次只接受一条 GATT 连接；会话期间米家 App/插件连不上（已确认可接受）。
  同 mac 连点两次只建一条链（pending 去重）；切设备/关弹层旧会话立即让位（gen 代号作废
  在途建链，防迟到的 connect 覆盖新会话）。
- **GATT 操作必须串行**（真机踩坑）：首读未回就订阅会被系统拒
  `2900011 The operation is busy`——顺序固定为 read ccc1 → read ccc4 → 订阅 notify。
  订阅若仍被拒，退路是直接写 CCCD(0x2902)=0x0001（实测可行），再不行才 10s 轮询。
- **省电**：连接会暂停设备广播且耗电（TX 15mA）→ 弹层关闭即断 + `SESSION_HOLD_MS=5min`
  到点自断（防忘记关弹层长期挂机）。
- **进程残留链路**：`aa force-stop`/崩溃不发 disconnect，设备端要等监督超时（实测 ~20-40s）
  才释放，期间新会话连不上 → 横幅会误报"不可连"。正常关闭弹层无此问题。
- **型号门控**：只有带秒秒测 ebe0ccc1 的型号才发起连接（白名单 `HT_MODELS`）；连上后
  枚举无该特征则记负样本 10min 冷却（实测 AA:BB:CC:DD:EE:0A 会报 `ebe0ccc1 not in GATT table`）。
- **token 供给**：登录路径用 device_list_page 的 12B token；beaconkey 只用于广播解密。
- **兜底链路不动**：GATT 失败 → 广播突发（62~180s）+ 云缓存，本地优先合并规则照旧。
- **已知噪音**：`getServices()` 枚举时系统 `bt_napi_ble_utils ConvertBLEDescriptorToJS memcpy_s error`
  会成串打印（描述符空值转 JS 的系统侧缺陷，2026-11-24 真机三轮 GATT 读成功时均出现），
  不影响读数，勿当成故障。

---

## 7. 刷固件路线取舍（已决策：2026-11-23 用户确认走「原厂不动」）

| 路线 | 收益 | 代价/风险 | 建议 |
|---|---|---|---|
| **原厂不动（✅ 已选定）** | 档位 B 双轨已给秒级新鲜度；保留米家生态（网关云缓存、家人米家 App 不受影响）；零刷机风险 | 广播被动新鲜度维持 1~3min（对"补数"够用） | **主线** |
| 刷 pvvx（实验机） | 广播 10~60s 可配、BTHome v2/ATC 明文帧（无 token 依赖，`ble.rs` 需新增 ATC/BTHome 解码器）、按钮 Connect 80s 窗口 | 设备脱离米家生态（网关不再代上云）；OTA 或编程器；`ble.rs` 加解码器工作量 | 留一台对照实验，量产主线不走 |
| 全量刷 pvvx | 数据新鲜度最好 | 与「兼容原厂」产品方向冲突 + 每台都有刷机变砖风险暴露 | 不建议 |

刷机技术面（若走）：pvvx issue #636 实证原厂 2.1.1_0009 可 OTA 到 MJ6_vNN（TelinkMiFlasher：连上 → Login(token+bindkey) → OTA）；社区 flasher `Liuhaoyu99/mj6-flasher` 加了 write-without-response + 12 次重连 + **断点续传**。风险：同代 MJWSD05MMC 有 OTA 变砖案例（#813，批次敏感）；pvvx README 表格对 MJWSD06MMC 仍标 "Programmer"（不同批次/固件版本行为可能不一致）→ 刷前务必先连接确认 `frstr`、电量 >40%。注意 mj6-flasher 里的 `MJ6_v59_official.bin` 经二进制取证是 **pvvx 自定义 V5.9**（含 'V5.9' 串与 0x2A6E/0x2A6F 环境传感 UUID），并非小米原厂包，其 README「官方原版救砖用」标注与二进制证据不符，救原厂要用 pvvx 仓库的 `Original_OTA_Xiaomi_*` 系列。

---

## 8. 证据工件与来源清单

### 本会话二进制取证工件（`_work/`）

| 文件 | 内容 |
|---|---|
| `mjwsd06mmc_ff.bin` | MJWSD06MMC 原厂 fullflash 512KB（pvvx 抓取，`MJWSD06MMC_ff.zip`） |
| `Firmware_mj6_ff_new.bin` | 新批次（GXHT40 传感器）原厂 fullflash 512KB |
| `ff_scan.py` / `ff_dump2.py` | 字符串/UUID 扫描脚本（`mible-login-info`、特征名表即出自此） |
| `mj6_index.html` / `mj6_core.js` | mj6-flasher 刷机页源码（Mi 登录流程在其 index.html） |
| `mj6_v59_official.bin` | pvvx 自定义 V5.9（非原厂，见 §7 警示） |
| `ble_login.py` | 充电器 mible 登录闭环脚本（V3 probe 直接改 MAC 复用） |

### 外部来源

- pvvx wiki MJWSD06MMC：https://pvvx.github.io/MJWSD06MMC/ （硬件/功耗/fullflash）
- pvvx wiki MJWSD05MMC：https://pvvx.github.io/MJWSD05MMC/ + **UUIDs.txt**（§4.1 服务表出处）
- pvvx/ATC_MiThermometer：README（80s Connect 窗口原文）；**issue #636**（原厂 2.1.1.0009 OTA + token 登录流程）；**issue #813**（MJWSD05MMC 绑定序列 + "bluetooth icon flashing" + OTA 风险）
- Liuhaoyu99/mj6-flasher：README（OTA 断点续传）+ index.html（MJWSD06MMC 登录分支/sendLogin 源码）
- 小米云端 token：`/v2/home/device_list_page`（本项目 session 既有）

### 项目内引用

- `mijia-core/src/ble.rs`：MiBeacon V2~V5 解码（V5 CCM）；帧控 0x4859/0x5859 两型
- `mijia-core/src/ble_auth.rs`：mible HKDF 登录 + 0x0019 分片流 + spec CCM（充电器闭环，cargo test 绿）
- `mijia-core/src/ble_cache.rs` + `card_net.rs`：本地优先合并（快路径落点）
- `docs/analysis/蓝牙标准认证协议.md`：充电器协议全文（§4 HKDF、§8 0x001B 通道、§9 负样本）
- `docs/analysis/温湿度计3mini-数据通道分析.md`：A/B/C/D 通道结论与 spec gatt-access 声明
- 真机 hilog：2026-11-22 广播间隔/帧型实测（§2.1 数据出处）

## 9. 未决项（继承）

- [ ] V1.2/V1.3/V1.4：变化阈值与按键是否触发广播帧
- [ ] V2.1：单键长按进入绑定模式的时长钉死
- [ ] V2.2：图标常亮=GATT 连接中实锤
- [ ] V3.1：06 实机 GATT 表 dump（0x0005 有无、0x001C 归属）
- [ ] V3.2：ebe0ccc1/0x2A19 免登录可读性 + 编码格式
- [ ] V3.5：properties_changed 推送频率
- [ ] 广播 1~3min 间隔与温湿度变化阈值的精确关系（若 V1.2 命中，可差分出阈值）
- [x] 刷固件路线决策（2026-11-23 用户拍板：**原厂不动**，档位 B 为唯一秒级通道；pvvx 路线仅在需要时另议）

### 2026-11-23 实机 probe 后更新

- [x] ~~V1.1 基线~~：突发 6×0x4859+6×0x5859 / 62~180s 间隔，PC 毫秒级扫描实锤
- [x] ~~V2.1 长按进绑定模式~~：~5s 屏幕闪 + 图标闪 + 0x1059 快闪广播
- [x] ~~V2.2 图标常亮=连接中~~：PC 连上瞬间用户观察图标常亮
- [x] ~~V3.1 服务表~~：实机 dump（§4.1），0x001A 双向 spec、无 0x0005/0x001B/0x001C
- [x] ~~V3.2 免登录读~~：ebe0ccc1 编码定案 [temp÷100][humi][电压÷1000]，快路径成立
- [x] 连接性实锤：常态 NONCONN 不可连，绑定模式可连（§4.5，推翻"随时可连"旧结论）
- [ ] **V3.3/V3.4 mible 登录 + get_rsp + 推送频率**：待云 token（session.json 过期，用户跑 `_work/do_login.py`）；probe 脚本已移植 native 连接法（连接器可挂起等长按）
- [ ] 米家连接机制（常态不可连时米家如何连上）→ 决定是否升级形态 (b)（§6.0）
- [ ] 绑定模式超时行为 / 长按是否真解绑（用户体感"重置"；米家 App 设备仍在则绑定保留）
- [ ] 0x180A 2A2x 子特征补 dump
- [x] 档位 B 快路径实现（§6 已按 §4.5 修订；Rust 组帧/解码 + napi 出口 + ArkTS BleGatt/弹层接线落地，cargo test 全绿）
- [x] **V4.1 真机端到端闭环**（2026-11-24）：弹层打开 → GATT 直读 2.1~2.4s → 横幅+磁贴「GATT直读/蓝牙直收」→ 读完即断
- [ ] 档位 B 登录路径 ArkTS 全链（mible login + spec get 订阅推送）：待 V3.3/V3.4 钉死协议细节
- [ ] V4.1b（常态长按提示）/ V4.2（断网复测）/ V4.3（与米家连接竞争）
- [x] ~~体验打磨~~：GATT 直读成功后首页卡片即时追平（onGattResult 复用 bleCardRefresh throttle + refreshCard，2026-11-24 真机验证 24.4→23.4℃）
- [x] **档位 B 形态 A 保活 + notify（2026-11-24 真机闭环）**：弹层会话保持、ebe0ccc1 免登录变化秒推、关弹层/5min 即断、串行化修复 `2900011 busy`、pending 去重防并发双链、型号白名单 + 负样本冷却
- [ ] 可连接窗口 persistence 观察：设备经 ~1 天仍可直连（22:09/22:10/22:13/22:14 四轮成功）→ 绑定/可连窗口超时远大于小时级，或被网关重新触发（关联 §9 绑定超时未解项）
