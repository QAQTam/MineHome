# home-control-mcp 严格设计

> ADR：`qaqh-backend/docs/adr/0024-home-control-mcp-transport.md`（选型决策）
> 本文是**设计规格**，不是完工报告。状态见 §11。
> 日期 2026-11-16 · 决策人：仓库所有者（q2 取值源=小米云；q3 允许低风险动作自动执行）
>
> **修订 2026-11-17（接线轮）**：本期真实可执行设备是落地扇 `dmaker.fan.p5c`（无级风速）
> 而不是空调，故动作模型从「power/mode/temp 三元组」泛化为**策略表里的
> `(实体, 动作) → 属性写入表`**（§3.3/§4）；同时补上 §4.3 的空调硬闸与静默时段、
> §5.2 的 approvals 文件通道、§10 的实机冒烟结果。封闭性没有放松：动作名与值域
> 仍由策略数据封闭枚举，未知名字/越界值一律结构化报错。

## 0. 设计要解决的冲突

用户口径 q3「允许低风险动作自动执行」与我上一轮「一律需显式审批」的推荐冲突。按项目宪法
3（以更好的架构推行、不拖泥带水）收口为：**「低风险」不由模型判断，改由策略层一张封闭
枚举白名单 + 每会话授权开关（默认关）判定；模型只产出结构化意图**。这既满足 q3，也守住
「事实/意图/策略分离」与「模型不是授权者」。

其余不变量（来自 ADR-0024，本文不再论证）：
- Rust 独立进程、stdio、MCP `2026-07-28` Modern stateless only；
- 零 RingingV2 wire 改动；`qaqh-mcp` 一行不改；
- 凭据不进 config.toml / env 明文 / 工具 schema / 日志 / 模型上下文。

## 1. 分层与信任边界

```
┌─ qaqh-daemon (qaqh-backend, LTS) ────────────────────────────────┐
│ qaqh-mcp client: Auto{preferred:[2026-07-28]} → server/discover │
│ 投影 mcp__home__{4工具}；tools 白名单；ToolCategory::Exec       │
│ process-wrap 进程组/Job 隔离、组杀、idle 回收、${secret:} 插值  │
└───────────────────────┬─────────────────────────────────────────┘
                        │ stdio, newline-delimited JSON-RPC
                        │ （不监听端口；无 Mcp-Session-Id；无 initialize）
┌───────────────────────▼─────────────────────────────────────────┐
│ home-control-mcp (本仓, 独立进程)                                │
│  ┌ main.rs（rmcp 接线层）───────────────────────────────────┐  │
│  │ 4 工具 + supported_protocol_versions + 审计/approvals 落盘 │  │
│  └───────────────┬──────────────────────────────────────────┘  │
│  ┌ lib.rs + actuate.rs（纯领域，零 rmcp 依赖）───────────────┐  │
│  │ Allowlist/ActionSpec/WriteSpec/QuietWindow/AutoPolicy/    │  │
│  │ SessionGrant/PlanStore/Cooldown/AuditRecord/              │  │
│  │ CloudError + HomeCloud trait + run_plan 编排              │  │
│  └───────────────┬──────────────────────────────────────────┘  │
│  ┌ cloud.rs（云侧适配，已接线 + 真机验证）───────────────────┐  │
│  │ HomeCloud for MijiaCloud → mijia_core::MiHome（path 依赖） │  │
│  └───────────────┬──────────────────────────────────────────┘  │
└──────────────────┼──────────────────────────────────────────────┘
                   │ HTTPS, RC4 签名（ssecurity 派生）
          api.io.mi.com → 米家设备 / 温湿度传感器
```

依赖方向单向：`main.rs → lib.rs(+actuate.rs) → cloud.rs → mijia-core`。
**lib.rs 不依赖 rmcp**、`actuate.rs` 只依赖 lib.rs 的 `HomeCloud` trait
（⇒ 策略与编排都能用假云单测，不需要真设备、不需要 rmcp）。
凭据只在 `cloud.rs` 与 `MiHome` 持有的 `Session` 里；`lib.rs` 的可序列化类型里没有 did/凭据。

## 2. 协议 / era 接线（已按 docs.rs 核实，不再「凭记忆补」）

| 项 | 值 | 依据 |
|---|---|---|
| rmcp 版本 | **3.9.0** | `qaqh-backend/Cargo.lock:3773-3774`；docs.rs rmcp-3.9.0 (2026-11-05) |
| features | `server`, `macros`, `schemars`, `transport-io`（`default-features=false` 时四项都要显式写；`schemars` **非** default） | docs.rs rmcp 3.9.0 Feature Flags |
| schemars | `^1.0`（rmcp 3.9.0 的 normal optional dep） | docs.rs 依赖表 |
| stdio | `rmcp::transport::stdio()`，需 `transport-io` + `server` | docs.rs `transport/io::stdio` |
| 版本收窄 | `fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]>` 返回 `&[V_2026_07_28]` | docs.rs `ServerHandler::supported_protocol_versions`：provided method，默认全部，override 后被 `Self::discover` 宣告、约束 per-request 版本校验 |
| `server/discover` | **不手写**：`ServerHandler::discover` 是 provided method，默认实现按 `supported_protocol_versions` 宣告 | docs.rs `ServerHandler` trait 方法列表 |
| 宏写法 | 两块：`#[tool_router] impl HomeServer {…}` + `#[tool_handler] impl ServerHandler for HomeServer { fn supported_protocol_versions(…) }` | rmcp-macros README：`server_handler` 会 elide `#[tool_handler]`；要自定义 handler 方法必须用显式 `#[tool_handler]` impl 块 |
| 客户端 | `qaqh-mcp` `auto_lifecycle()` = `Auto{preferred:[V_2026_07_28], legacy:Some(V_2025_11_25)}`，stdio/HTTP 三路径共用 | `adapter.rs:102-107, 209, 239, 287` |

⇒ `Auto` 客户端先发 `server/discover`，我们的 server 只宣告 2026-07-28 → **直连零回退**。
spec 兼容矩阵「Modern client + Legacy server = Fails」反向也说明：一旦 server 退回多版本宣告，
客户端仍会选 modern，但我们的 server 就不再是 modern-only（违反 ADR 决策 3/4），故版本收窄必须写。

## 3. 工具契约（封闭集合，4 个）

`tools/list` 顺序固定（2026-07-28 建议确定性顺序以利 prompt cache）。投影名
`mcp__home__<name>`。配置里再写一份 `tools` 白名单（§8），双保险。

### 3.1 `read_environment` — 读观测值（事实）

- 入参：**无**（零参数）
- 出参 `Environment`：

  ```json
  {"temperature_c":27.4,"humidity_pct":79.0,"battery_pct":100.0,
   "reported_at_ms":1794851986000,
   "monitors":[{"name":"示例门磁·接触态","value":true,"reported_at_ms":1794850021000}],
   "source":"mijia-cloud","observed_at_ms":1794851998000}
  ```

- 语义：从 §9 的 `TempSensor` 一次批量读 `get_properties`；单属性 `code != 0` 时
  **该字段留空**，全部非数字才报 `sensor_read_failed`，绝不返回猜测值（宪规 5）。
  `observed_at_ms` 是本次读取时刻，`reported_at_ms` 是设备/网关的上报时刻
  （BLE 设备靠网关缓存，二者可能差几分钟——模型必须能看到这个差别）。
  `monitors` 是**只读监视器**（门磁接触态、夜灯光照、已登记设备的通断等），
  读失败逐项标 `"error":"read_failed"`，不整批失败。
- 副作用：无。永远 `RiskClass::Auto`。

### 3.2 `list_capabilities` — 列出可操作实体

- 入参：无
- 出参：

  ```json
  {"entities":[{"name":"落地扇","quiet_hours":false,
                "actions":[{"name":"set_speed","description":"无级风速 1-100（注意：不是 1-4 挡位 fan-level）",
                             "requires_value":true,
                             "value_range":{"min":1,"max":100,"integer":true},
                             "needs_approval":true}]}],
   "monitors":["示例门磁·接触态","…"],"temp_sensor_configured":true,
   "grant":{"auto_enabled":false},"cooldown_secs":60}
  ```

- **只返回友好名 + 友好动作 + 值域**，绝不返回 `did/siid/piid/token`；
  `needs_approval`/`grant` 让模型知道当前授权态（它无权改变）。副作用：无。

### 3.3 `propose_action` — 提交意图，返回 plan handle

- 入参 `ActionRequest`（`deny_unknown_fields`）：

  ```json
  {"entity":"落地扇","action":"set_speed","value":66,"reason":"屋里 27.4℃，用户说要凉快些"}
  ```

  `action` 必须是该实体在 `policy.json` 里登记过的友好动作名；`value` 与动作的
  `from_request` 写入项严格配对：该给值的动作缺值 → `value_required`，不该给的
  塞了值 → `unexpected_value`；值域/整数性由策略里的 `min/max/integer` 判定
  （来自 miot-spec 的 `value-range`，见 §9.2）。`reason` 可选，语义见 §3.5。
  多塞任何字段（`did`/`siid`…）由 `deny_unknown_fields` 直接拒。

- 处理：`classify(req, allow, policy, now_secs)` → 值域/静默时段校验 →
  `AutoPolicy` 白名单查 `(entity, action)` → `may_auto_execute(class, grant)` →
  `PlanStore::mint(req, approved)`。
- 出参：`{plan_id, entity, action, value, risk_class:"auto"|"approval", approved, approval_required, next}`
- **不产生任何物理动作**。

### 3.4 `execute_action` — 执行已批准计划

- 入参：`{"plan_id":"p-…"}`（`deny_unknown_fields`）
- 处理：见 §5.1 的六步固定顺序（云侧可用 → peek plan → 静默时段 → 冷却窗口 →
  批准 → 消费 plan+打冷却 → `set_properties` → **实时**回读比对 → 写 `AuditRecord`）。
- 出参：`{plan_id, entity, action, value, ok:true, observed_after:[…], at_ms}`，
  `observed_after` 按 `ActionSpec.verify` 顺序给回读值（**不含坐标**）。
  失败一律 tool-level error（§6）。
- **不暴露 `approve_action` 工具**：审批是用户动作，不能让模型自批自审（越权）。审批注入见 §5.2。

### 3.5 `reason` 的契约（给谁看、不看什么）

`propose_action` 的可选字段 `reason`（≤200 字符，按 `chars()` 计）是**模型写给
审批人看的一句话**，不是写给策略层看的：

| 保证 | 实现 / 证据 |
|---|---|
| 不影响风险判定 | `classify()` 只用它做**长度**校验；`auto_actions`/值域/静默时段都不看内容。单测 `reason_never_changes_risk_but_is_length_capped` 断言：同一动作带不带理由 `risk_class` 必须相等；越界值 + 漂亮理由仍报 `value_out_of_range` |
| 有界 | 超限 → `reason_too_long`（它不是自由文本通道，也不是把 prompt 注入带外文件的口子） |
| 可追溯 | 原样进 pending 队列（§5.4）与审计 JSONL，人工回看时知道「当时它是怎么说的」 |
| 不在信任链里 | 批准判据永远是 plan_id + 友好名 + 值 + 值域；理由只增加上下文，**不增加权限** |

为什么值得加：审批人原本只能看到「落地扇 set_speed 66」，信息不足会把审批退化成
盲点确认。理由走的是**单向**通道——模型写、人读，模型没有任何工具能读回它或据它自批。

## 4. 领域模型与策略（lib.rs）

| 类型 | 约束 |
|---|---|
| `ActionRequest` | `deny_unknown_fields`；`entity: String`、`action: String`、`value: Option<f64>` |
| `EntityBinding` | `{did, model, actions: {友好动作名: ActionSpec}, quiet_hours?}`；siid/piid 一律 `i64`（与 `mijia_core::PropValue` 对齐）；**不实现 Serialize** |
| `ActionSpec` | `{description?, writes: [WriteSpec], verify: [PropRef]}`；`writes` 非空 |
| `WriteSpec` | `{siid, piid}` + 恰一项：死值 `value` **或** `from_request:true`（后者必须带 `min/max`，可标 `integer`）；两者都给/都不给 → 加载失败 |
| `QuietWindow` | `{utc_offset_secs, start_minute, end_minute}`，本地分钟窗口，支持跨零点；不引时区 crate |
| `Allowlist` | `HashMap<友好名, EntityBinding>`，未登记名 → `unknown_entity`；未登记动作名 → `unknown_action` |
| `AutoPolicy` | `auto_actions: HashSet<(String, String)>` 封闭白名单 + `cooldown_secs` + `approved_plan_ids` |
| `SessionGrant` | `{auto_enabled: bool}`，**默认 false** |
| `TempSensor` | `{did, siid_temp, piid_temp, siid_humidity?/piid_humidity?, siid_battery?/piid_battery?}`（一次批量读；湿度/电量坐标必须成对） |
| `MonitorRef` | `{did, siid, piid}` 只读监视器；按 did 分组成一次批量读 |
| `Plan` | `{plan_id, entity, action, value, approved, created_at_ms}`（server-minted handle, SEP-2567） |
| `PlanStore` | `HashMap<plan_id, Plan>`；`peek()` 不吃句柄，`take()` 为 `remove`（一次性消费 = 幂等键） |
| `Cooldown` | `HashMap<entity, last_exec_ms>`；`check()` 只判窗口、`mark()` 记账，避免拒绝调用把带外批准烧掉 |
| `AuditRecord` | `{plan_id, entity, action, value, at_ms, ok, error?, observed_after?}`——**无 did/坐标** |
| `CloudError` | 结构化 enum（§6），**不用 `String` 吞掉错误** |

### 4.1 `classify`（服务端函数，模型不参与）

```
1 allow.resolve(entity)          → 失败 unknown_entity
2 actions.get(action)            → 失败 unknown_action
3 值形：动作要值而 value 缺 → value_required
         动作不要值而给了   → unexpected_value
         越出 min..=max       → value_out_of_range
         integer 项带小数      → value_not_integer
4 now_secs 落在 quiet_hours     → quiet_hours（策略数据，非模型判断）
5 (entity,action) ∈ policy.auto_actions → Auto
6 否则 → Approval
```

`may_auto_execute(class, grant) = class==Auto && grant.auto_enabled`。
执行期（`execute_action`）会**再判一次**静默时段：propose 与 execute 之间可能跨点。

### 4.2 q3 默认策略表（策略数据，可配置，不需改代码）

| 动作 | 默认 RiskClass | 免审批条件 |
|---|---|---|
| 两个只读工具 | Auto（无副作用，永远自动） | — |
| 关机类（`off`） | Auto | `grant.auto_enabled=true` |
| 开机/改值类（`on` / `set_speed` / `set_gear` / `set_wind_mode`） | **Approval** | 仅当该 `(entity,action)` 被显式写入 `auto_actions` 且 `grant.auto_enabled=true` |

即：本期随仓的 `policy.json` 里 `auto_actions=[]`、`grant.auto_enabled=false`
⇒ **任何物理动作都要用户带外批准**（用户口径「物理执行要授权」）。要放开某个动作，
只需在 `policy.json` 加一条 `auto_actions` 并打开会话开关（§8）。**「低风险」的定义
是这张表，不是模型的一句话**。

### 4.3 代码级硬闸（比策略表更高的一层）

两条不随 `policy.json` 变化的规则，写在 `lib.rs` 里，`PolicyFile::validate()` 在
**启动时**执行，违反即拒绝启动（`exit(2)`），不是静默忽略：

1. **空调永不控制**：allowlist 里出现 `FORBIDDEN_MODEL_PREFIXES`
   （`xiaomi.aircondition*` / `lumi.acpartner*`）或 `FORBIDDEN_DIDS`
   （空调 m4 = 用户父亲房间、空调伴侣2）⇒ 启动失败。有人误改配置也不会变成可控设备。
2. **只读设备不进 allowlist**：门磁、夜灯、传感器一律只能出现在 `monitors`
   （`read_environment` 的只读通道）；`allowlist` 里 `actions` 为空同样拒绝启动。

再加一条时间维度的策略数据：示例灯等卧室物理设备带 `quiet_hours`
（23:00-08:00 +08:00），窗口内 `propose_action`/`execute_action` 都报 `quiet_hours`。
硬闸保证「永不」，静默时段保证「深夜不碰」，两者都不是模型的判断。

## 5. 状态机

### 5.1 plan 生命周期（SEP-2567：跨调用状态 = 普通工具参数）

```
(不存在) ──propose_action──► minted{approved:bool}
                              │
              ┌───────────────┴────────────────┐
     approved=true                     approved=false
              │                                │
   execute_action                      execute_action
              │                                │
   冷却 check ──► 批准（approved / 带外句柄 §5.2）
              │            │
              │      approval_required 错误（plan 仍在）
   take() 消费 + cooldown.mark() 同批完成
              │
     set_properties → 实时 get_properties 回读比对 → AuditRecord
             │                    │
          一致 ok=true        不一致 ok=false + verify_mismatch
              │
           (删除，不可重放)
```

- `take()` 是 `remove`：**同一 plan_id 第二次调用必失败**（`unknown_or_consumed_plan`）。
  这是 2026-07-28 移除 SSE 重传后必须自实现的幂等保证（changelog Major change 9）。
- 断流 = 丢在途请求，客户端用**新 request id** 重发；由于 plan 已被消费，重发只会得到
  确定性错误而不会重复开风扇/灯。冷却再加一层防抖。
- 执行顺序固定（`main.rs::execute_action` 的 ①~⑥）：**云侧可用 → peek plan →
  静默时段 → 冷却窗口 → 批准 → take()+mark() → set → 实时回读**。
  前五步任何一步失败都**不消费 plan、不烧批准、不标记冷却**（`Cooldown::check`
  与 `mark` 拆开就是为了这个）；云侧不可用时同样不消费，避免「消费了却没执行」。
- 云调用失败（响应丢了但动作可能已发出）：**plan 不回放**，写审计 `ok=false` 并返回
  明确错误，由用户重新 `propose`。回放会在「动作已执行、响应丢失」时造成重复动作；
  不回放用「确定性失败 + 用户重新决策」守住 at-most-once。
- 回读**必须走实时穿透**（`get_properties_realtime`，rpcv2）：`/miotspec/prop/get`
  是云缓存，刚下发的值有 1~5s 滞后，用缓存回读会把「已生效」误判成 `verify_mismatch`。
  实时读为空时回落缓存读，仍拿不到值 → `verify_empty`（不谎报成功）。

### 5.2 审批注入（带外，不经 MCP 工具）

`propose_action` 返回 `approval_required=true` 时，审批由用户在 QAQH 既有 approval 机制
完成，结果以「批准句柄」**一行一个**写入独立的 approvals 文件
（env `HOME_CONTROL_APPROVALS`，默认与 policy 同目录）。`execute_action` 命中即
**重写文件删掉该行**（一次性），之后才允许物理下发；重写失败 ⇒ 不放行（fail closed）。
`policy.json` 的 `approved_plan_ids` 仍支持（启动时种入内存池，进程内消费），
用于「随策略一起给定的批量预批准」。

两条通道都是**进程本地文件，不是 MCP session** —— 与 stateless 无冲突（stateless 指
协议层无连接会话，server 仍可有持久策略）。关键不变量：**模型没有任何工具能写这两个
文件**（本 server 只有 4 个工具，无 `approve_action`），所以模型无法自批自审。

> 未落地项：与 `qaqh-mcp` 的 per-tool 审批 UI 对接（当前只核实到 `ToolCategory`
> 自动拒绝，未核实 per-tool 审批回调）。UI 侧要做的就是把批准的 plan_id 追加进
> approvals 文件。本期用文件通道冒烟已闭环，UI 对接仍记在 §11。

### 5.3 `securityStatus=16` 的现实约束

`MiHome::login_password` 可能返回 `LoginFlow::NeedNotification{url}`（需手机点「允许」）。
这是交互式人工流程 ⇒ **本 server 不可能完全自主登录**。`cloud.rs` 必须把它做成显式
`CloudError::NeedRelogin`，绝不自动重试、绝不猜密码。完整的登录态设计见 §7.6。

### 5.4 待审批队列（pending 文件 = 审批方的输入面）

§5.2 解决了「人怎么把批准递进来」，没解决「人怎么知道有什么在等」。
`propose_action` 判为需审批时，server 往 `HOME_CONTROL_PENDING`（JSONL）追加一行：

```json
{"plan_id":"p-…","entity":"落地扇","action":"set_speed","value":66,
 "reason":"屋里 27.4℃，用户说要凉快些","asked_at_ms":1794855046000}
```

- **只含友好名 / 值 / 理由**，不含 did/坐标/凭据（集成测试与冒烟脚本都断言这点）；
- `execute_action` 一受理就把它从队列摘掉（成功、失败、`verify_mismatch` 都不留），
  ⇒ 队列里**只有未决项**；未批准时它留在那儿等人来看；
- 模型没有读它的工具，也没有写它的工具：它是**给人/UI 的出口**，不是模型的回声室。
  但它是**模型 authored 的文本给到人眼**⇒ 审批 UI 必须按**惰性纯文本**渲染
  （不解析 markdown/HTML、不当指令），这也是 `reason` 要有 200 字符硬上限的第二个理由。
  审批 UI 的最小闭环 = 读 `pending.jsonl` → 展示 → 用户点同意 → 把 plan_id 追加进 `approvals.txt`。

## 6. 错误码（工具级，全部可达模型，但不泄露凭据）

按 rmcp README「tool-level error `Ok(CallToolResult::error(...))` vs protocol error
`Err(McpError)`」：**领域/业务失败一律 tool-level error**（`isError=true`，模型能看见
并纠正，`content` 是 `{"error":…,"detail":…}`）；只有「请求根本无法路由」才用 protocol
error。实测（§10.3）：入参多塞未知字段由 rmcp 的 `Parameters` 反序列化失败投影成
tool-level error（`failed to deserialize parameters: unknown field \`did\`…`），
本 server 自身不主动产生 protocol error。

| error | 触发 | 模型可纠正 |
|---|---|---|
| `unknown_entity` | allowlist 未登记 | 是（先调 list_capabilities） |
| `unknown_action` | 实体在清单里但动作名没登记 | 是（先调 list_capabilities） |
| `value_required` / `unexpected_value` | 动作与 `value` 字段不配对 | 是 |
| `value_out_of_range` | 越出策略里的 `min..=max`（= spec `value-range`） | 是 |
| `value_not_integer` | `integer` 写入项收到小数 | 是 |
| `quiet_hours` | 实体在静默时段内（§4.3） | 否（等到窗口外/用户决定） |
| `unknown_or_consumed_plan` | plan_id 不存在或已执行 | 是（重新 propose） |
| `approval_required` | plan 未批准 | 否（需用户） |
| `cooldown_active` | 同实体冷却窗口内 | 否（等待） |
| `no_temp_sensor` | 策略既无传感器也无监视器 | 否 |
| `sensor_read_failed` | 读回来全是非零 code / 非数字 | 否（重读） |
| `need_relogin` | `mijia_core::Error::NoSession` / `Login{..}` / `NeedNotification{url}`；**以及本进程已证明凭据不可用之后的每一次同类失败**（§7.6） | 否（人工登录） |
| `cloud_rejected` | `Cloud{code}`（带 code，不带 message 原文里的凭据串） | 否 |
| `write_rejected` | 写入被云拒（`Cloud{code}` 的 set 路径，与读分开记审计） | 否 |
| `credential_invalid` | `Undecryptable`（凭据失效/风控） | 否（人工） |
| `shape_invalid` | 云端返回 JSON 形状不符 | 否 |
| `cloud_transport` | `Transport/Http/Io/Json/Base64` | 否 |
| `verify_mismatch` | 写入被接受但实时回读与期望不符（设备没真生效） | 否（审计已记） |
| `verify_empty` | 写入后回读不到任何值 ⇒ 不能谎报成功 | 否 |
| `cloud_not_wired` | 没配 `HOME_CONTROL_SESSION` / 构造失败 | 否 |

`cloud.rs` 的 `mijia_core::Error → CloudError` 映射是**穷尽 match**，新增变体必须编译期报错
（ADR-0004「解析失败不得静默降级」、宪规 5）。**任何错误都不回显 `ssecurity`/`serviceToken`/
`session.json` 内容**，也不回显 `did`（`detail` 里只出现友好名与 code）。

## 7. 凭据隔离（硬约束）

1. `ssecurity` / `serviceToken` / `passToken` 只在 `cloud.rs` + `MiHome` 的 `Session` 里，
   由 `Session::load(session.json)` 从**文件**读；不进任何可序列化输出。
2. `config.toml` 的 `[mcp.servers.home]` **env 里只放路径**，不放凭据：
   `HOME_CONTROL_SESSION`（session.json 绝对路径）、`HOME_CONTROL_POLICY`（policy.json）、
   `HOME_CONTROL_AUDIT`（审计 JSONL 路径）、`HOME_CONTROL_APPROVALS`（§5.2 的带外批准
   句柄文件）、`HOME_CONTROL_PENDING`（§5.4 的待审批队列）。`${secret:...}` 机制可用于路径中的敏感片段，
   但**不用于承载凭据本体**（明文 env 会直通子进程、进 `/proc`）。
3. `session.json` 上游就是明文落盘（`协议面分析报告.md` §10），这是既有事实；
   生产化应迁到 `secrets.toml [secrets.mcp]`（DPAPI / 0600）或 OS keychain ——
   **列为后续项**，不阻塞本期，也不假装已解决。
4. `stderr` 只写脱敏日志；`stdout` 只写 MCP JSON-RPC（stdio 规范硬要求）。
5. 工具 schema / 描述 / 错误体 / 审计记录里都不含 did（审计用友好名 + plan_id）。
6. 冒烟脚本 `_work/mcp_smoke.py` + 集成测试 `tests/era_auto_client.rs` 断言「不泄露」
   不变量的位置覆盖：工具结果、能力清单、**pending 队列**、审计 JSONL——都不得出现
   `did`、`siid`、`piid`、`blt.` 前缀与真实 did 数字；`tools/list` 的 schema 里也不得出现
   配置面 env 名（`HOME_CONTROL_*`）。

### 7.6 登录态：三层凭据、一次续期、永不自动重发

**事实（本期实测，不是抄文档）**：`session.json` 里同时躺着三种凭据，寿命与作用域不同——

| 凭据 | 作用 | 位置 | 过期了怎么办 |
|---|---|---|---|
| `ssecurity` | 每次 API 请求的 RC4/签名派生根 | 顶层 | 跟着会话一起换 |
| `serviceToken` | 实际调用 `api.io.mi.com` 的会话凭据 | 顶层 + `cookies` | **可免密换回**（下一行） |
| `passToken` | 账号域长效凭据 | `cookies.passToken` | 只能人工重新登录（含 `securityStatus=16` 手机确认） |

现存的 `session.json` 是 11-15 23:26 生成的，11-17 13:30 实测 `probe()` 仍 `alive`
（≈38 小时）。**具体有效期没有实测过**，不许当常量用。

四条硬规则（`src/session.rs`，全部有单测）：

1. **健康必须实测**。`mijia_core::MiHome::is_logged_in()` 的语义是「文件里有没有
   session」，过期 token 一样返回 true——所以 trait 里这个方法改名
   `has_session_file()`，并在启动时用 `probe()`（一次 `list_devices`）真探活，
   日志字段是 `session=alive|expired|unknown has_file=…` 两个独立值。
   没配 session 或探测失败一律 `unknown`，**不许**写成 `alive`。
2. **续期是账号域调用，默认关**。`refresh()` 用 `cookies.passToken` 走
   `/pass/serviceLogin → clientSign → 新 serviceToken`（`mijia-core` 早就实现了，
   本轮才接上）。它会**重写凭据文件**，因此由
   `HOME_CONTROL_ALLOW_REFRESH` 显式打开（默认关 = 一个账号域请求都不发，
   过期就是纯 `need_relogin`）。
   没配 `passToken` ⇒ 直接 `need_relogin`，不回退去猜密码。
3. **一次，且只重试只读**。`may_refresh_session` 是四条件合取：错误形状像凭据失效
   **且** 不是写调用 **且** 开关开着 **且** 本进程还没试过。
   写调用失败时最多 `refresh_without_reissue()`（把会话换回来供**下次**用，
   本次动作**不重发**）——因为「401 之后无法区分没送达与已执行」，重发就是重复
   物理动作，直接违反 §5.1 的 at-most-once。
4. **证明了就持续说实话**。`attempted`（还欠不欠一次续期）与 `dead`（凭据已被证明
   不可用）是两个独立的量：续期失败、或续期成功后重发仍然 401 ⇒ `dead=true`，
   此后同类失败一律报 `need_relogin`，**不许**退化成 `cloud_transport` 噪声
   （否则第二次失败就把「要人工登录」这个唯一有用的信息丢了）。

**模型面**：`login` / `logout` / `refresh` 一个都不暴露（工具集封闭为 4 个）。
模型只能看到 `need_relogin` 这个「该找人了」的信号。`MiHome::logout()` 会**删文件**，
把它接进任何进程内路径都可能顺手清掉用户的登录态，因此本 crate 不引用它。

**运维建议（未做，等用户点头）**：`HOME_CONTROL_SESSION` 指向 `session.json` 的**副本**
（如 `session.mcp.json`）。理由：续期会重写该文件，而 MCP 侧刷新 token 是否会让
手机 App 的旧 token 失效**未实测**——不要让自己和家人的 App 成为这个不确定性的实验品。

## 8. 配置接线（零 wire 改动）

```toml
[mcp.servers.home]
# command 与 url 互斥；这里 stdio，command 必须是绝对路径（adapter.rs:113-115 安全约束）
# 带 .exe 且用反斜杠字面串：resolve_command_path 见到绝对路径就原样交给 CreateProcess
command = 'home-control-mcp\target\debug\home-control-mcp.exe'
args = []
# cwd 留空 = 继承 daemon 进程 cwd；本 server 全部走 env 里的绝对路径，不依赖 cwd（已实测）
env = {
  HOME_CONTROL_SESSION = "session.json",
  HOME_CONTROL_POLICY  = "home-control-mcp/policy.json",
  HOME_CONTROL_AUDIT   = "home-control-mcp/audit.jsonl",
  HOME_CONTROL_APPROVALS = "home-control-mcp/approvals.txt",
  HOME_CONTROL_PENDING   = "home-control-mcp/pending.jsonl",
  # 默认不设 = 关：凭据过期时一个账号域请求都不发，直接报 need_relogin（§7.6）
  # HOME_CONTROL_ALLOW_REFRESH = "1",
}
# 双保险：即便 server 以后新增工具也不会自动暴露给模型
tools = ["read_environment", "list_capabilities", "propose_action", "execute_action"]
resources = false              # 持久层字段名是 resources（qaqh-types/src/config.rs:164）
                               # 运行时结构里才叫 resources_enabled；写错不会报错，会被 serde 静默忽略
default_timeout_secs = 20      # 1..=3600，config.rs 校验
max_concurrent_calls = 1       # 1..=16；1 保证物理动作串行，配合冷却
```

- `transport` 由 `command` 非空推导为 `Stdio`（`config.rs:265-291`）；server 名 `home` 合法
  （`[a-z0-9_-]{1,64}`）。
- **生效方式**：配置改动**不会自动生效**——重载是显式的 ringing 命令
  `agent.reload_config`（`qaqh-ringing/src/command.rs:88`），或重启 daemon。连接本身是
  lazy 的（`connection.rs::ensure_connected`），重载后第一次调用才会 spawn 子进程。
- **fail-fast 面**（`map_mcp_config`）：`command`/`url` 互斥、`tools` 条目非空、
  `default_timeout_secs ∈ 1..=3600`、`max_concurrent_calls ∈ 1..=16`，越界直接报错；
  但 **TOML 语法错误会让整份 config 回退默认**（连别的 server 一起丢），
  所以注册脚本 `_work/register_home_mcp.py` 落盘前用 `tomllib` 双向校验 + 逐字段比对
  既有 server + 临时文件原子替换 + 备份 `config.toml.bak-home-mcp`。
- `tools` 白名单由 `qaqh-mcp/src/projection.rs:43-47` 在投影时过滤；`stdio → ToolCategory::Exec`。
- **没有新增路由**：`RINGING_V2_HTTP_ROUTES` / `CONTROL_PLANE_ROUTES` 两张表不动。
- `policy.json`（启动时读，非 MCP session）：

  ```json
  {
    "allowlist": {
      "落地扇": {
        "did": "<device-did>", "model": "dmaker.fan.p5c",
        "actions": {
          "off": {"description":"关闭落地扇",
                  "writes":[{"siid":2,"piid":1,"value":false}],
                  "verify":[{"siid":2,"piid":1}]},
          "set_speed": {"description":"无级风速 1-100（不是 1-4 挡位 fan-level）",
                        "writes":[{"siid":8,"piid":1,"from_request":true,
                                    "min":1,"max":100,"integer":true}],
                        "verify":[{"siid":8,"piid":1}]}
        }
      },
      "示例灯": {
        "did": "<device-did>", "model": "xiaomi.switch.w1",
        "quiet_hours": {"utc_offset_secs":28800,"start_minute":1380,"end_minute":480},
        "actions": {"on":  {"writes":[{"siid":2,"piid":1,"value":true}],  "verify":[{"siid":2,"piid":1}]},
                    "off": {"writes":[{"siid":2,"piid":1,"value":false}], "verify":[{"siid":2,"piid":1}]}}
      }
    },
    "temp_sensor": {"did":"<blt-did>","siid_temp":2,"piid_temp":1001,
                    "siid_humidity":2,"piid_humidity":1002,
                    "siid_battery":3,"piid_battery":1003},
    "monitors": {"示例门磁·接触态": {"did":"<blt-did>","siid":2,"piid":2}},
    "auto_actions": [],
    "grant": {"auto_enabled": false},
    "cooldown_secs": 60
  }
  ```

  `did` 只出现在这个本地文件里，模型永远看不到。随仓 `policy.json` 是真机实配
  （`.gitignore` 已排除，模板见 `policy.example.json`）；**空调不在 allowlist 里**，
  即便有人加进去也会被 §4.3 的硬闸拒绝启动。

## 9. 云侧接线（逐行对应 `mijia-core`，已核实 API）

| 设计动作 | `mijia-core` 调用 | 位置 |
|---|---|---|
| 构造 | `MiHome::new(session_path)` 或 `MiHome::with_http(path, Arc<ReqwestSender::new()?>)` | `mi_home.rs:24-40` |
| 会话检查 | `MiHome::is_logged_in()` / `session() -> Result<Session>` | `mi_home.rs:42-52` |
| 读观测值 | `MiHome::get_properties(did, &[(i64,i64)])`：优先 `/miotspec/prop/get` 全通道缓存（带 `updateTime`），失败回落 `/home/rpcv2/<did>` 实时穿透 | `mi_home.rs:110-118`、`device.rs:79-151` |
| 设备清单 | `MiHome::list_devices() -> Result<Vec<Device>>` | `mi_home.rs:103-106` |
| 执行动作 | `MiHome::set_properties(did, &[(i64,i64,Value)])` | `mi_home.rs:120-123`、`device.rs:154-193` |
| 回读确认 | `MiHome::get_properties_realtime(did, props)`（rpcv2 穿透优先，`blt.*` 自动回落缓存读），与期望值比对写进 `AuditRecord.observed_after` | `mi_home.rs:130-144` |

> `/device/batchdevicedatas`：body 形状仍未实测，且**已从 `HomeCloud` trait 里删掉**
> ——留着一条永不使用的 `NotWired` 分支比不写更误导。q2 的取值需求由
> `MiHome::get_properties`（`spec_get_properties` 覆盖 WiFi/Zigbee/BLE mesh/BLE 缓存值）
> 全量满足，真机已验证（§10.3）。

### 9.1 `cloud.rs` 接线边界（本轮 `cargo build` 通过 + 真机冒烟）

**已接线**（`src/cloud.rs`，只调用逐行核实过的 `MiHome` API）：
`MiHome::new(session_path)`、`get_properties`、`get_properties_realtime`、
`set_properties`、`is_logged_in`，以及 `mijia_core::error::Error → CloudError` 的
**穷尽 match**（新增 Error 变体编译期报错）。`PropValue → PropSnapshot` 形状投影不吞
`code != 0`；set 路径的 `Error::Cloud{code}` 单独映射成 `WriteRejected`，与读路径的
`Rejected` 在审计里分得开。

`main.rs` 从 `HOME_CONTROL_SESSION` 构造 `MijiaCloud`；未配置时 `cloud = None`，
工具报 `cloud_not_wired`，不静默放行。**动作编排已接线**（`src/actuate.rs::run_plan`）：
plan → `ActionSpec.writes` → `set_properties` → 实时回读 → 等价比对（§5.1）。

- `PropValue { did, siid: i64, piid: i64, code: i64, value: Value, update_time: Option<i64> }`
  ⇒ **所有 siid/piid 用 i64**（骨架第一版误用 i32，已修）。
- `set_properties` 内部已校验 `result` 与逐项 `ret`（`device.rs:177-191`）；`code!=0` →
  `Error::Cloud{code,message}`。我们不能再吞掉这些错误去「看起来能工作」。
- reqwest 版本：`mijia-core` 用 0.12，rmcp 3.9.0 的 HTTP client 用 ^0.13.2（**二期 HTTP 才
  引入**）。本期 stdio 不拉 rmcp 的 reqwest，无双实例问题；二期需显式处理。

### 9.2 本期实配设备的 spec 坐标（全部实测过，非抄文档）

| 实体/传感器 | 机型 | 坐标与语义 | 验证方式 |
|---|---|---|---|
| 落地扇（可写） | `dmaker.fan.p5c` | `s2/p1` on(bool)、`s2/p2` fan-level 1..4、`s2/p3` mode 0=直吹/1=自然、**`s8/p1` speed-level 1..100 整数**（「66% 风速」映射这个，不是 fan-level） | 档位 A 会话已用 `/miotspec/prop/set` 同发 `s2/p1`+`s8/p1` 并 2.5s 回读确认；本期 `execute_action` 再次写入 `s8/p1=66` 并实时回读 `observed_after=[66]` |
| 温湿度计（只读） | `xiaomi.sensor_ht.mini`（`blt.*`） | 自定义号 `s2/p1001` 温度(float, -30..100, 0.1)、`s2/p1002` 湿度(0..100)、`s3/p1003` 电量；**标准 `s2/p1|p2` 报 -704040003** | `read_environment` 真机返回 27.4℃ / 79% / 100% |
| 示例门磁（只读监视器） | `isa.magnet.dw2hl`（`blt.*`） | `s2/p2` contact-state、`s2/p1` 光照档位、`s3/p1` 电量 | `read_environment` 返回 contact=true（网关 01:27 上报） |
| 夜灯（只读监视器） | `yeelink.light.nl1`（`blt.*`，**spec 里没有可写灯控服务**） | `s4/p1` 照度、`s3/p1` 电量、`s2/p1` 静置分钟 | 只读通；设备当前 offline，读的是网关缓存（靠 `reported_at_ms` 暴露陈旧度） |
| 状态面（只读 monitors） | 同上各机型 | 可控设备的当前状态也登记进 `monitors`（落地扇 `s2/p1` 通断 / `s8/p1` 风速 / `s2/p2` 挡位、示例灯 `s2/p1`），这样「风扇现在还开着吗」走**用户策展的只读面**，而不是给模型开一个能遍历设备的读工具 |
| 示例灯（可写 + 静默时段） | `xiaomi.switch.w1` | `s2/p1` on(bool) | **本期不下发**（深夜 + 未获用户批准）；坐标来自 miot-spec 实例定义 |
| 空调 m4 / 空调伴侣2 | `xiaomi.aircondition.m4` / `lumi.acpartner.mcn02` | — | **不进 allowlist**，且 §4.3 代码级硬闸拒绝它们出现在策略里 |

## 10. 验证方案（不写一堆测试，宪规 1/10/20）

1. **编译**：`cargo check` / `cargo build`（`CARGO_BUILD_JOBS=2`，本机内存紧张）；`--release` 交 CI。
2. **单测（策略不变量 + 假云编排 + 会话生命周期）**：`cargo test` = **35 项**
   （+ `tests/era_auto_client.rs` 真客户端 3 项）。
   领域层：`classify` 的未知实体/未知动作/值形/值域/整数性/静默时段/默认拒绝、
   `may_auto_execute` 双条件、`PlanStore` 的 peek 不吃 + take 一次性 + 句柄带随机盐、
   `Cooldown` 按实体隔离、策略加载的空调硬闸与畸形 `WriteSpec` 拒绝、请求体
   `deny_unknown_fields`、`ToolName` 封闭集合。
   编排层（`actuate.rs`，假云 `FakeCloud{Apply|Frozen|NoReads}`）：写入表取值/取整、
   set→实时回读→比对一致、设备没生效报 `verify_mismatch`、云拒写报 `write_rejected`
   且不记成功、回读为空报 `verify_empty`（不许谎报成功）。
   会话层（`session.rs`，假云加了 401 配额 / 续期成败两种旋钮）：只有凭据形状的错误
   才触发续期（业务码 `Rejected`/`WriteRejected` 不触发）、四条件合取、
   **正常读只发一次云请求**（钉死过一次 `let-else` 把成功路径变成双发的坑）、
   续期成功⇒重发一次且不算 dead、passToken 死⇒每进程只撞一次且**持续**报
   `need_relogin`、开关关⇒`refresh_count()==0`、写路径只续期不重发。
   —— 不为「开风扇」这个功能堆测试，只测不变量。
3. **集成冒烟（真实 session.json，`_work/mcp_smoke.py`）**：stdio 管道喂 newline-delimited
   JSON-RPC（每条请求带 2026-07-28 必填 `_meta`）：
   `server/discover`（只宣告 2026-07-28）→ `tools/list`（恰好 4 个）→
   `read_environment` → `list_capabilities` → 策略面负例 6 条 → `propose_action` →
   未批准 `execute_action`（两次，验证 plan 未被烧）→ 带外批准后 `execute_action`
   → 同句柄二次调用（幂等）→ 冷却 → 审计落盘。结果见 §11。
4. **真客户端集成测试（`tests/era_auto_client.rs`，3 项）**：用 rmcp 的
   `ClientLifecycleMode::Auto{preferred:[V_2026_07_28], legacy:Some(V_2025_11_25)}`
   ——与 `qaqh-mcp/src/adapter.rs::auto_lifecycle()` **逐字同构**——spawn 本 server 的
   stdio 子进程：
   - Auto 客户端零回退连上，`server/discover` 回报 `supported_versions == ["2026-07-28"]`
     且宣告 tools 能力；`tools/list` 恰好 4 个；schema 里没有 did/siid/piid/凭据/`HOME_CONTROL_*`，
     但有 `reason`；
   - `ClientLifecycleMode::Initialize`（legacy-only）客户端**连不上**，报
     `-32022 Unsupported protocol version` ⇒ era 闸真会变化（宪规 18 反证的客户端半边）；
   - 无云配置下策略面行为一致：`read_environment` → `cloud_not_wired`（不编造读数）、
     `reason` 不改变 `risk_class`、超长发 `reason_too_long`、需审批的 plan 落 pending
     且不含 did、多塞 `did` 被拒。
   ⇒ ADR-0024 §后果点名的脆弱点（客户端 preferred 版本与本 server supported 必须有交集）
   从此由测试锁死，不靠注释声明。**仍不等于**跑通真实 daemon（`qaqh-mcp` 侧未接线）。
5. **契约零变化**：`check --against docs/api-surface/v3.0`（确认 `docs/api-surface/*` 未动）。
6. **反证（宪规 18）**：
   - **服务端半边已跑**（§11）：不带 `initialize` 握手直接发 legacy 版
     `initialize(2025-11-25)` ⇒ `-32022 Unsupported protocol version
     {requested:"2025-11-25", supported:["2026-07-28"]}`；
     `tools/list` 的 `_meta.protocolVersion` 填 `2025-11-25` ⇒ 同样 `-32022`；
     `tools/list` **不带** `_meta` ⇒ `-32602 request _meta is missing or has malformed
     required fields: io.modelcontextprotocol/protocolVersion,
     io.modelcontextprotocol/clientCapabilities`。⇒ era 闸与 stateless 每请求元数据
     都是**真在生效**，不是写在注释里的声明。
   - **未跑**：把 `supported_protocol_versions` 临时加回 legacy 后用 `qaqh-mcp` 的
     Auto 客户端连接（需要 `qaqh-backend` 侧的端到端接线，见 §11 未验证项）。
7. **CI 配对检查**：`auto_lifecycle()` 的 `preferred_versions` 与本 server 的
   `supported_protocol_versions` 必须有交集，否则直接红（ADR-0024 §后果脆弱点）——
   §10.4 的 `auto_client_negotiates_modern_with_zero_fallback` 是它的本地等价物。

## 11. 状态 / 未验证项 / 落地清单

**已闭环（骨架轮 11-16 · 接线轮 11-17 02:1x · 审批信息量轮 02:4x · 登录态轮 13:4x，全部实测）**：
- rmcp 3.9.0 server 侧 API 全部核实并编译证实（features、`supported_protocol_versions`
  签名、`discover` 默认实现、`stdio`、`#[tool_handler]` 显式 impl 块 +
  `name/instructions` 属性写法）；精确锁 `=3.9.0`（写 `"3.9.0"` 会被 `^` 解析到 3.10.0）；
- `mijia-core` 的 `MiHome` / `DeviceClient` / `Error` 公开 API 逐行核实；
  `cloud.rs` 五件套（含 `get_properties_realtime`）+ 穷尽错误映射接线完成；
- **动作模型泛化**（§3.3/§4）：`ActionSpec`/`WriteSpec` 策略表落地，
  骨架版的 `CoolingMode`/`EntityBinding.*_power|mode|temp`/`actuation_not_wired`
  全部退役，`/device/batchdevicedatas` 空接口从 trait 删除；
- **§4.3 代码级硬闸**：空调机型/did 进 allowlist ⇒ 启动 `exit(2)`；只读设备只能在
  `monitors`；示例灯带 `quiet_hours`（真机 02:0x 实测报 `quiet_hours`）；
- **§5.2 带外批准通道**：`HOME_CONTROL_APPROVALS` 文件一行一句柄、命中即重写删除、
  重写失败不放行；`policy.json` 的 `approved_plan_ids` 作为内存池同时支持；
- **§3.5 `reason` + §5.4 pending 队列**：模型写给审批人的一句话（≤200 字符、
  不参与风险判定、超限报 `reason_too_long`），随 plan 落 `pending.jsonl`
  （友好名/值/理由，无 did），`execute_action` 一受理就摘掉；审计里也留一份；
- **§9.2 状态面**：可控设备自身的当前状态（落地扇通断/风速/挡位、示例灯通断）
  登记进 `monitors` 走只读通道 ⇒「风扇还开着吗」不需要给模型开一个能遍历设备的读工具；
- **§10.4 真客户端集成测试 3 项全绿**（`tests/era_auto_client.rs`，rmcp dev-deps）：
  与 `qaqh-mcp::auto_lifecycle()` 逐字同构的 Auto 客户端零回退连上 + `discover` 只宣告
  2026-07-28；legacy-only 客户端被 `-32022` 拒；无云时策略面诚实（`cloud_not_wired`）；
- **§7.6 登录态**：`has_session_file()`（改名自带着误导性的 `is_logged_in()`）+
  `probe()`（启动时真拉一次 `list_devices`）+ `refresh()`（`cookies.passToken` →
  `finish_notification` → 新 `serviceToken`，能力本就在 `mijia-core`，本轮接上）；
  续期由 `HOME_CONTROL_ALLOW_REFRESH` 显式打开（**默认关**），且
  `session.rs` 的四条件 + `attempted`/`dead` 两个量把「什么时候该续、续完能不能重发、
  证明死了之后怎么报」全部钉成纯函数单测；
- **daemon 侧已注册**：`~/.qaqh/config.toml` 新增 `[mcp.servers.home]`
  （12 行，diff 只有新增；备份 `config.toml.bak-home-mcp`）。按 daemon 真实 spawn 语义
  实测过一遍（绝对路径 exe + `args=[]` + 配置里那 5 个 env 键 + `stderr=null` +
  **cwd 指到 `C:\Windows`**）：`discover` 只宣告 2026-07-28、`tools/list` 恰好 4 个、
  `read_environment` 真云返回 28.1℃/77%、`propose_action` 仍 `approval_required`。
  env 里**只有路径**，且故意不设 `HOME_CONTROL_ALLOW_REFRESH` ⇒ 这个 server 不会写 session.json；
- **§10.1 单测 35 项 + §10.4 集成 3 项全绿**（`cargo test`），`cargo build` 零 warning；
- **§10.5 服务端反证 4 项全绿**（同一冒烟脚本的 `2b` 段）：legacy `initialize` 与
  每请求 `protocolVersion=2025-11-25` 都得 `-32022`；缺 `_meta` 得 `-32602`；
  `server/discover` 恒只宣告 `["2026-07-28"]`；
- **物理写阶段改成「现取现写」**：`--actuate` 不再硬编码 66，而是用 `read_environment`
  只读面刚取到的 `落地扇·风速` 当目标值（本轮实测现值 100 ⇒ 写 100 ⇒ 回读 `[100]`，
  终态不变）；取不到现值就**跳过物理写**并报错，绝不盲写猜的值；
- **§10.3 真机 stdio 冒烟 50 项断言全绿**（`_work/mcp_smoke.py --actuate`）：
  `server/discover` 只宣告 `2026-07-28`；`tools/list` 恰好 4 个且 schema 不含坐标；
  启动日志 `session=alive has_file=true allow_refresh=false`（真探活生效）；
  `read_environment` 走真实云返回 28.1℃ / 76% / 传感器电量 100% + 10 个只读监视器
  （门磁 contact=true、夜灯走网关缓存靠 `reported_at_ms` 暴露陈旧度、风扇通断/风速现值）；
  8 条策略负例（未知实体/越界值/小数值/缺值/深夜示例灯/`reason` 超长/多塞 `did`/
  未知动作）全部按预期报错，并断言 `reason` 带与不带 `risk_class` 相同、
  pending 队列有该 plan 且不含 did、执行后被摘掉；
  未批准时 `execute_action` 连续两次都回 `approval_required` 且 plan **没被烧掉**；
  带外批准后**真实下发** `落地扇 set_speed=66`（= 当时现值，故物理终态无变化）→
  实时回读 `observed_after=[66]` → `ok:true`；同句柄二次调用 `unknown_or_consumed_plan`；
  60s 内再来一次 `cooldown_active`；审计 JSONL 落盘且不含 did/siid/piid；
  事后独立只读复查风扇 `on=true / speed=100`（= 用户当天 13:18 手动改成的现值，
  本 server 没动它）；静默时段断言已改成**按当前本地时间判定**（早先写死「现在是深夜」，
  白天跑会假失败——13:4x 实测走的是「时段外 ⇒ 可提案但要审批」分支）。

**仍未验证 / 后续（禁止读成已完成）**：
- 与 `qaqh-mcp` 的**端到端连通**：配置已注册且按 spawn 语义实测过，但**还没在跑着的
  daemon 里真正重载过**（重载是显式命令 `agent.reload_config`，或重启 daemon——都不该我替你按）；
  §10.5 契约零变化检查（`check --against
  docs/api-surface/v3.0`）与 §10.6 CI 配对检查仍未跑（要在 `qaqh-backend` 仓里做）；
- QAQH 审批 UI 对接：现在两端都有了（读 `pending.jsonl` 展示 → 写 `approvals.txt`
  放行），但 UI 侧代码未动，本期由冒烟脚本/人工代替；
- **§7.6 的 `refresh()` 真机路径未验证**：`serviceToken` 的**实际有效期**没测过
  （只测到「38 小时后仍 alive」），所以从未真的撞上一次过期；且「MCP 侧静默续期
  会不会把手机 App 的旧 token 踢下线」**未实测**——这正是 `HOME_CONTROL_ALLOW_REFRESH`
  默认关的原因，也建议让本 server 用 `session.json` 的**副本**而不是同一个文件；
- 示例灯 `on`/`off` 从未真机下发过（深夜 + 未获用户批准；坐标仅来自 miot-spec 定义）；
- 冷却窗口仍是**内存态**（进程重启清零）；approvals 文件是持久的，plan 池不是；
- `securityStatus=16` 的人工登录流程未触发（现有 session 仍有效，启动日志
  `session_valid=true`）；
- `rmcp-macros` 在 lock 里解析到 **3.10.0**（rmcp 3.9.0 的依赖区间允许），编译与运行
  均正常，但「与 qaqh-backend 完全同版本」的初衷只做到 `rmcp` 本体一致——二期若引入
  HTTP transport 需重新核对该区间。

**落地清单**：
- [x] ADR-0024（选型决策）
- [x] 本设计文档 + 骨架（`Cargo.toml` / `lib.rs` / `main.rs` / `cloud.rs` / `DESIGN.md`）
- [x] 锁 `rmcp =3.9.0` + `cargo check` 通过、零 warning
- [x] `cloud.rs`：`HomeCloud for MijiaCloud`（§9 映射 + 穷尽错误匹配 + 实时回读）
- [x] `policy.json` 真机实配（allowlist / temp_sensor / monitors / auto_actions=[] /
      grant 关）+ `policy.example.json` 模板 + `.gitignore` 排除实配
- [x] 审计 JSONL append（`HOME_CONTROL_AUDIT`，无 did/坐标）
- [x] `execute_action` 动作编排（`actuate.rs`：写入表 → set → 实时回读 → 等价比对）
- [x] 带外批准 approvals 文件 + 策略加载期硬闸（空调拒绝启动）+ 静默时段
- [x] `reason` → pending 队列（§3.5/§5.4）+ 可控设备状态并入只读面（§9.2）
- [x] §7.6 登录态：真探活 + passToken 续期通道 + 四条件续期策略（默认关）
- [x] 真机 stdio 冒烟 50 项（只读 + 一次「现取现值」的无变化写入）
- [x] rmcp Auto 客户端集成测试 3 项（era 配对的客户端半边 + legacy 反证）
- [x] daemon 侧 `[mcp.servers.home]` 已写进 `~/.qaqh/config.toml`
      （12 行新增 + 备份 + TOML 双向校验 + 按 spawn 语义离线实测）
- [ ] 在跑着的 daemon 里 `agent.reload_config` 重载一次，确认投影出
      `mcp__home__{4 工具}` 且首次调用能 lazy spawn（需用户操作/点头）
- [ ] `qaqh-mcp` 端到端冒烟 + 契约零变化检查 + CI 配对检查
- [ ] 冷却/plan 池的跨进程持久化；审批 UI 对接 approvals 文件
- [ ] 示例灯等其余实体的真机下发验证（需用户在 daytime 明确批准）
- [ ] 二期：`session.json` 迁入 secrets 机制；HTTP/unix-socket stateless
      （`legacy_session_mode=false` + `NeverSessionManager` + 127.0.0.1 + Origin + 认证）
