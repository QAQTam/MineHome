# 摄像头 Rust 复刻：许可事实 + 协议分层 + 行动路线

> 2026-11-22。触发问题："miloco 是 Apache 2 吗？能不能搬库/翻译成 rs？"
> **先纠正前提，再给路线。**

## 1. 许可事实（本节是全文的前提）

| 资产 | 许可证 | 我们能做什么 |
|---|---|---|
| **xiaomi-miloco 仓库整体**（含 camera lib 二进制） | **《Xiaomi Miloco License Agreement》——不是 Apache 2**：非商业专用；明文排除"开发 APP/Web 服务"；"entire content...including **object code**"均属授权作品 | 只读事实；自托管运行；**不搬代码/不打包 lib/不翻译进 rs** |
| **XiaoMi/ha_xiaomi_home**（米家 HA 官方集成） | **Apache-2.0**（miloco cert 模块自注"ported from"它） | **可合法翻译成 Rust**（Apache 2 允许衍生作品，需附 NOTICE+许可声明）；覆盖 cloud/MQTT/mdns/cert/lan/spec/i18n——**不含 camera、不含 BLE** |
| `libmiot_camera_lite.so` 内嵌的 **PPCS** | ThroughTek Kalay 第三方商用 SDK（我们 camera_protocol_notes.md 早已定性：viewer 用 PPCS_API.dll、米家用 libTUTKGlobalAPIs/PPPP_API） | 只做**事实级互操作**：观测自有设备流量 + 公开资料（Mandiant 2021 Kalay 协议研究、V380 系项目） |
| 我们自己的 `camera_protocol_notes.md`/`pppp_probe.py` | 自有资产 | 直接用 |

> 结论：**"抱着学习的态度复刻 rs"在协议层没问题**（协议事实不受版权保护，这是 Samba/FFmpeg
> 式互操作的传统），但有两个纪律：① 翻译参照选 ha_xiaomi_home（Apache 2），miloco 只取
> 事实；② camera lib 是闭源二进制，只"运行+观测"，不反汇编照抄实现。

## 2. `libmiot_camera_lite.so` 静态解剖（2026-11-22，linux x86_64 样本）

基本盘：8.6 MB，**完全自包含**（DT_NEEDED 仅 libc/libstdc++/pthread），对外只导出
14 个 `miot_camera_*` ABI——PPCS、MISS、OpenSSL、libcurl 全部静态链死在里面。

四层协议栈（由字符串证据钉死）：

```text
┌ 应用 ABI：miot_camera_init / new(info{did,model,ip,channel_count})
│           start(config{video_qualities,enable_audio,pin_code}) / raw_data 回调
├ MTP 通道复用："mtp read fail, channel %d"、ucChannelId > MTP_CHANNEL_RDT、
│              "failed to configure/start channel"（CTRL + RDT 通道，帧头
│              {codec_id,length,timestamp,sequence,frame_type,channel}）
├ MISS 会话层（小米）：miss_client_session_open / miss_rdt_send、
│              MISS_ERR_{CLOSE_BY_LOCAL,CLOSE_BY_REMOTE,AUTHORIZED...}、
│              "miss, [lvl:%d] %s" 日志面
├ PPCS 传输层（ThroughTek Kalay）：完整错误码族 ERROR_PPCS_{NOT_INITIALIZED,
│              TIME_OUT,INVALID_ID,DEVICE_NOT_ONLINE,INVALID_APILICENSE,INVALID_DSK...}、
│              PPCS_Initialize/NetworkDetect/Listen/Share_Bandwidth、
│              "P2P Server IP resolved/Hello Ack"（UDP P2P + 中继）
└ 内嵌云信令（libcurl + HSTS 缓存）：
   /app/v2/home/device_list_page        ← 设备定位
   /app/v2/pincode/check_generic        ← pin_code 配置项的校验（客户端侧）
   /app/pincode/check_alone
   /app/v2/device/miss_get_vendor_security ← 密钥/厂商参数获取
   /app/v2/device/camera_perf_data      ← {"vendorItem":%d,"linkType":"%s",
                                           "channelId":...,"supportRP2P":1} 性能上报
```

三个直接产出：
1. **坐实我们 11-16 的推断**——camera_protocol_notes.md 写过"会话建立需云端信令先行"：
   lib 自己就打 `device_list_page` + `miss_get_vendor_security`，顺序即"云信令 → P2P"。
2. **`miss_get_vendor` 请求体形态**（当时端点存在但 "nil request"）大概率由 native 层构造——
   现在有了第二个观测样本（miloco lib），抓包对照可钉死。
3. **pin_code 是客户端侧校验**（check_generic/check_alone）——这就是 ABI 里 `pin_code`
   配置的用途：连接前向云验证 4 位连接码（米家 App"查看直播需输入验证码"的那位）。

## 3. Rust 复刻计划（`mijia-camera-rs`，按可测试性排序）

```text
mijia-camera-rs/
├── mtp/     通道复用 + 帧解析器      ← 先做：纯函数、golden capture 可测
├── miss/    会话层：vendor_security 换 key、session_open、rdt_send
├── ppcs/    Kalay 客户端（register/hello/UDP P2P/中继）← 最难，最后做
└── sink/    帧出口：Annex-B → OhOS AVPlayer / 自建 WS（对齐 miloco 的
              init 消息 + 二进制 NAL 形状，浏览器 WebCodecs/MSE 双路径）
```

里程碑（每步都有"观测→golden→实现→对拍"闭环）：
- **M0 观测台**（前置）：WSL 起 miloco 绑自有摄像头；Wireshark 全量抓
  UDP P2P + 云信令，落 `_work/camera_caps/`，标注每个报文属于哪层；
  同时跑 `_work/camera_viewer/PPCS_API.dll`（老路线）做双样本对照。
- **M1 mtp/ 帧**：从抓包切出视频/音频帧，写解析器对拍（codec_id/frame_type 枚举、
  channel 复用）。
- **M2 miss/ 会话**：复用 `mijia-core` 云层补 `miss_get_vendor_security` +
  `pincode/check_*` 两个端点（请求体从抓包钉死）；MISS 握手对拍。
- **M3 ppcs/ 传输**：Kalay 握手（对照 Mandiant 公开研究 + 抓包），先做 LAN P2P
  成功路径，中继模式后置（若相机固件强制云中继，风险在 roadmap 已记）。
- **M4 sink/ 接 App**：帧进 MineHome，ArkTS 侧 AVPlayer 或 WS 播放。

## 4. 合规边界（写给未来的自己）

- **只观测自己的设备、自己的账号**；golden captures 落 `.gitignore`（含设备 did/pin）。
- 不翻译 miloco Python、不反汇编 PPCS 实现、不抄 `libmiot_camera_lite` 逻辑——
  我们要的是**报文形状与状态机**，不是它们的代码。
- 商业风险有两层：① miloco license（避开即可）；② **ThroughTek Kalay 是商业 SDK**，
  商用客户端实现属灰色地带——社区互操作（V380/小米相机项目）是常态，但若 MineHome
  走商业化，摄像头模块要么换授权、要么只支持"本地 RTSP/ONVIF 已开机型"做白名单路线。
- 我们自己的 notes/抓包/解析器都是干净资产（从自己的流量得来）。

## 5. 与既有文档的关系

- `camera_protocol_notes.md`（11-16）：viewer/MMISS/PPCS 定性 + 双向抓包缺口 →
  本文档的 M0 直接补它欠的"完整双向抓包"。
- `camera_roadmap.md`：路线 A 的"多会话专项"有了新开局——miloco lib 给了**第二个
  独立观测样本**（不再只有 viewer exe 和米家 APK 两份）。
- `miloco-机会分析.md` §2：本篇是其摄像头小节的展开与许可修正（当时误按"非商业仅限
  学习"口径，现按 LICENSE 原文精确化：可翻译的对照本=ha_xiaomi_home）。
