# Fork 后怎么把它跑起来（MineHome / HarmonyOS NEXT · API 26）

面向"克隆这份仓库、想在**自己手机/平板**上编译运行"的人。全程不需要小米账号以外的私有材料。

> 这份文档里的每条命令都在 Windows + DevEco 命令行工具链上实测过；路径按你的机器改。

---

## 0. 你需要准备什么

> 下表全部东西装没装齐，不用人肉核对：仓库根跑 `python tools/check_env.py`
> （或 `just check`），`[x]` 项按提示补，`[!]` 项只影响对应构建环节。
> 给 CI / AI agent 消费的机器可读版：`python tools/check_env.py --json`。

| 必需 | 说明 |
|---|---|
| DevEco Studio / Command-Line Tools | 需要 **API 26（HarmonyOS NEXT 5.x）** SDK。下文用 `$CLT` 代指 `command-line-tools` 根目录 |
| Rust + `ohrs`（ohos-rs）| 交叉编译 napi 用；`ohrs -V` 能出版本号即可 |
| 一台真机或模拟器 | ArkTS 用例（hypium）与最终验收都要跑在设备/模拟器上，**没有纯 CPU 的 ArkTS 测试** |
| 你自己的小米系账号 | 设备列表、房间归属、卡片模板全部来自云端实时拉取，仓库里不含任何账号数据 |

## 1. 签名配置（**必须自己做，仓库里故意没有**）

`app/build-profile.json5` 被 `.gitignore` 忽略，因为它含 `keyPassword`/`storePassword`（用**你本机主密钥**加密，抄走也没意义）以及带用户名的绝对路径。

```powershell
cd app
Copy-Item build-profile.json5.example build-profile.json5
```

然后用 DevEco Studio 打开 `app/` 工程：

**File ▸ Project Structure ▸ Project ▸ Signing Configs ▸ 勾选 Automatically generate signature**

DevEco 会在你本机 `~/.ohos/config/` 下生成 `.p12/.cer/.p7b`，并把四行 `material` 写回 `build-profile.json5`。确认 `products[0].signingConfig` 与 `signingConfigs[0].name` 都是 `default` 即可。

> 改完别提交：`git status` 里不应出现 `app/build-profile.json5`。

## 2. 原生库（`libmijia_napi.so`）

**公开仓库刻意不携带预编译 `.so`**：Rust 产物内嵌编译机的绝对路径与用户名（panic 元数据），
发布二进制等于泄露开发者本机信息。克隆后必须先自编一次（hvigor 对 `file:` 本地包标
`isSO:false`、不会自动打进 HAP，所以产物要双拷贝到）：

```
app/native/libmijia_napi/libs/<abi>/libmijia_napi.so
app/entry/libs/<abi>/libmijia_napi.so
```

只有改了 `mijia-core/` 或 `mijia-napi/` 的 Rust 代码才需要重编：

```powershell
$env:OHOS_NDK_HOME  = "$CLT\sdk\default\openharmony"
$env:HOS_SDK_HOME   = "$env:OHOS_NDK_HOME"
$env:DEVECO_SDK_HOME= "$CLT\sdk"
cd mijia-napi
ohrs build                       # 产出 dist/<abi>/libmijia_napi.so + dist/index.d.ts
```

**⚠ 这一步不能省**：`ohrs build` 出来的是**未剥离**的 debug 产物，arm64 单文件约 **78 MB**，直接塞进 HAP 会爆。必须 strip 后再拷：

```powershell
$strip = "$CLT\sdk\default\openharmony\native\llvm\bin\llvm-strip.exe"
foreach ($a in 'arm64-v8a','armeabi-v7a','x86_64') {
  foreach ($d in "app/native/libmijia_napi/libs/$a", "app/entry/libs/$a") {
    Copy-Item "mijia-napi/dist/$a/libmijia_napi.so" "$d/libmijia_napi.so" -Force
    & $strip "$d/libmijia_napi.so"          # 78 MB → ~9 MB
  }
}
Copy-Item mijia-napi/dist/index.d.ts app/native/libmijia_napi/index.d.ts -Force
```

`index.d.ts` 是 napi-derive 生成的，**不要手改**；ArkTS 的类型全靠它，改了 Rust 接口忘了拷就会出现"d.ts 里没有这个函数"的编译错。

## 3. 编译 HAP

```powershell
$env:DEVECO_SDK_HOME = "$CLT\sdk"
$env:HOS_SDK_HOME    = "$CLT\sdk"
$env:PATH = "$CLT\tool/node;$env:PATH"
cd app
& "$CLT\bin\hvigorw.bat" --mode module -p product=default -p buildMode=debug assembleHap --no-daemon
```

产物：`app/entry/build/default/outputs/default/entry-default-signed.hap`。

装到设备（`hdc` 在 `$CLT\sdk\default\openharmony\toolchains\hdc.exe`，PATH 常常不生效，建议写全路径）：

```powershell
& $hdc tconn <设备IP:端口>          # 或 USB 直连后 hdc list targets
& $hdc install -r <相对路径>.hap     # ⚠ hdc 的路径参数用**相对名**，给绝对路径会被拼成 cwd+绝对路径
```

`install -r` 不会清数据，所以重装**不会丢已登录会话**；千万别用"先卸载再装"来省事。

## 4. 提交前必须过的三条门

```powershell
cd mijia-core ; cargo test          # 单元 + 集成（含卡片引擎、安全闸、房间、layout）
cd home-control-mcp ; cargo test    # 另一个 crate 有自己的 35 条用例
python tools/check_secrets.py       # 退出码 0 = 没有把真实账号痕迹带进仓库
```

`check_secrets.py` 的判定清单**动态取自**你本机 `_work/`（那目录不入库）里的真实产物，所以换账号不用改代码。它有一条硬约束：**所有豁免必须写理由**，review 时看理由成不成立，而不是把检查关掉。目前仅存的三个豁免是：

1. `mijia-core/fixtures/vectors.json` —— 确定性合成密钥材料；
2. `mijia-core/src/ble.rs` —— 真机抓的 MiBeacon 广播帧回归向量（合成帧测不出解码细节）；
3. `home-control-mcp/src/lib.rs` —— `FORBIDDEN_DIDS` 是**刻意不经 `policy.json` 覆盖**的
   did 级硬闸（误改配置也永不控制名单内设备）。公开仓库里放的是占位哨兵，让启动自检
   用例始终可测；fork 后把你自己家的敏感设备 did 填进去（机型前缀
   `FORBIDDEN_MODEL_PREFIXES` 是主闸，已覆盖空调/空调伴侣这类舒适控制）。

## 5. 目录里都有什么

| 路径 | 作用 |
|---|---|
| `mijia-core/` | 纯 Rust 核心：RC4 签名协议、auth、设备/房间、卡片模板引擎、安全闸、layout。**可 PC 测试** |
| `mijia-core/fixtures/` | 集成测试夹具：云端卡片模板 + miot-spec 样本 + **按 model 索引**的 spec 分类（无 did、无用户设备名） |
| `mijia-napi/` | 鸿蒙 NAPI 绑定（ohos-rs）。`ohrs build` 的产物目录 `dist/` 不入库 |
| `app/` | ArkTS 应用。`entry/src/main/ets/pages/Index.ets` 是主页面，`miot/` 是 SDK 薄封装 |
| `tools/` | `check_env.py`（新机器环境自检）、`make_card_fixtures.py`（重建夹具）、`check_secrets.py`（泄漏扫描）、`gen_fixtures.py`（生成 vectors.json） |
| `中间件设计.md` | 分层与 API 契约设计文档（Rust 侧的"应该长什么样"以它为准） |

**不含**在仓库里、但文档多处提到的 `_work/`：逆向工作目录（真机抓包、jadx 反编译、dumpLayout、日志、截图），全是真实账号数据，所以整目录忽略。新克隆的仓库**不需要它也能编译和测试**（夹具已搬进 `mijia-core/fixtures/`）。

## 6. 加一张新设备卡片（贡献流程草图）

当前状态：卡片模板来自云端 `getappconfig`（无签名 GET），spec 来自公开的 `miot-spec.org`，所以**判定一张卡片对不对不需要任何账号**——这也是把安全闸放在 Rust 的原因（ArkTS 侧测试必须跑设备）。

一张新卡片要过的六关，前四关现在就能离线测：

1. 模板命中/不命中（含 `disable_type` 排除名单）
2. 属性寻址（模板 `prop_key` → `(siid,piid)`，解析不到必须跳过而不是崩）
3. 归一化（`kind` 分类、label/单位本地化、`ratio/format`、`range/step`、枚举 `choices`）
4. 联动求值（`disabled_when` 在当前值下的 `enabled`；注意布尔与字符串 `"false"` 都要能对上）
5. **安全闸**（`safety::quick_switch_ref`）：能不能给一键。三重口径，宁缺勿滥 —— 路由器/网关/micloud 一律拒；能解析到 spec 规范 `on` 属性时要求模板主 toggle 与它同址；只有 spec 缺失才按中文 label 兜底，且布防/网络/童锁/指示这类词优先拦
6. 渲染契约（文案不得漏 `basic_arming` 这类未翻译协议 token；卡片位置按 did 锚定不跳位）

第 5 关的期望值应写成可断言的数据而不是"看图说没问题"。相关设计与未完成的采集端在 `_work/卡片上线标准设计.md`（该文件不入库，内容待整理进仓库文档）。

## 7. 已知的坑（省你几小时）

- **`ohrs build` 的产物必须 strip**，见 §2。
- **`.so` 要双拷贝**，见 §2。
- **`hdc` 路径参数用相对名**，见 §3。
- 长构建请等进程真正结束再 `install`：构建未完时装的是**上一个** HAP，界面显示旧内容，极容易误判成"我的改动没生效"。对时间戳（hap vs 源码）来确认。
- `Web` 组件的 `src` 只在首帧生效，换 URL 必须用 `WebviewController.loadUrl()`；UA 要在 `onControllerAttached` 里 `setCustomUserAgent`（`userAgent` 属性自 API 10 废弃）。
- 用 `uitest` 做自动化时，**点击坐标必须取 `dumpLayout` 的 `bounds`**，别按截图像素手算；`keyEvent Back` 在没有弹层时会把应用退到后台（不是崩溃，`pidof` 还在）。
- 系统符号 `sys.symbol.*` 的名字要先对 SDK 的 `toolchains/id_defined.json` 校验，写错是编译期硬错。
