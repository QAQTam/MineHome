//! home-control-mcp 领域契约与策略层（ADR-0024 / DESIGN.md）。
//!
//! 根模块**不依赖 rmcp**（MCP 接线隔离在 `main.rs`），云侧 SDK 依赖隔离在
//! [`cloud`]。它是纯 Rust 可编译、可单测的部分，把三件事钉死：
//!   1. 模型能看到什么（封闭工具集、strict-input 入参、观测值带 source+ts）；
//!   2. 谁判风险（策略层封闭白名单 + 每会话授权开关 + 静默时段，不是模型、不是 prompt）；
//!   3. 设备坐标只在进程内的 `policy.json` 里，`did`/`siid`/`piid` 绝不出现在工具结果里。
//!
//! 动作模型是**策略表驱动**的（DESIGN.md §3.3/§4）：`(友好实体, 友好动作)` →
//! 一组属性写入 + 一组回读坐标。骨架版那套「空调专用 power/mode/temp 三元组」
//! 已退役（本期真实可控设备是落地扇 `dmaker.fan.p5c`）。封闭性没有放松：动作名与
//! 值域仍由策略数据封闭枚举，未知名字/越界值一律结构化报错。
//!
//! 子模块：[`actuate`]（执行编排，假云可测）、[`cloud`]（`MiHome` 适配）、
//! [`session`]（凭据过期与静默续期的策略）。设计规格见同目录 `DESIGN.md`。

pub mod actuate;
pub mod cloud;
pub mod session;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

/// 观测值来源标签（q2：小米云 get_properties）。
pub const CLOUD_SOURCE: &str = "mijia-cloud";

/// 代码级硬闸：这些机型/did 永远不允许进入 allowlist（DESIGN.md §4.3）。
/// 家庭敏感设备（空调/空调伴侣这类舒适控制）本 server **永不控制**——即使有人
/// 误改 `policy.json`，启动时也会 fail-closed 退出，而不是静默放行。
/// 机型前缀是主闸；`FORBIDDEN_DIDS` 里保留占位哨兵，让启动自检用例始终有得测——
/// fork 后请把你自己家敏感设备的真实 did 填进这里（did 级双保险）。
pub const FORBIDDEN_MODEL_PREFIXES: &[&str] = &["xiaomi.aircondition", "lumi.acpartner"];
pub const FORBIDDEN_DIDS: &[&str] = &["example-forbidden-did-1", "example-forbidden-did-2"];

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_millis() as u64
}

pub fn now_secs() -> i64 {
    (now_ms() / 1000) as i64
}

/// 工具封闭集合：MCP 侧只暴露这四个名字（ADR-0024 决策 5）。
/// 加工具必须改这里并写进 ADR，不允许暴露通用 `raw_rpc` / `call_miio`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolName {
    ReadEnvironment,
    ListCapabilities,
    ProposeAction,
    ExecuteAction,
}

impl ToolName {
    /// 与 `main.rs` 的 tool_router 对拍用（漏写/多写工具 = 启动即失败）。
    pub const ALL: [ToolName; 4] = [
        ToolName::ReadEnvironment,
        ToolName::ListCapabilities,
        ToolName::ProposeAction,
        ToolName::ExecuteAction,
    ];
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ReadEnvironment => "read_environment",
            Self::ListCapabilities => "list_capabilities",
            Self::ProposeAction => "propose_action",
            Self::ExecuteAction => "execute_action",
        }
    }
}

// ───────────────────────────── 观测值（事实） ─────────────────────────────

/// 单个观测读数的出处：值 + 云端上报时刻（BLE 设备走网关缓存，
/// `reported_at_ms` 让模型能区分「现在 26℃」和「网关 3 小时前缓存 26℃」）。
/// 读失败时 `value=None` 且 `error` 非空——不填猜测值（宪规 5）。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Reading {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reported_at_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<&'static str>,
}

/// 云端 `updateTime` 的单位不总是毫秒（秒级时间戳也见过）；统一到 ms。
pub fn to_epoch_ms(t: i64) -> u64 {
    if t < 100_000_000_000 {
        (t as u64).saturating_mul(1000)
    } else {
        t as u64
    }
}

/// 环境观测（事实）。「有点热」是用户意图，不是事实；模型不得凭空填这个结构。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Environment {
    /// `f64` 而非 `f32`：`27.4f32` 序列化会吐成 `27.399999618530273`，
    /// 模型不需要看见 float 表示噪声。
    pub temperature_c: Option<f64>,
    pub humidity_pct: Option<f64>,
    pub battery_pct: Option<f64>,
    /// 传感器云缓存的上报时刻（温湿度计是 BLE 设备，靠网关上报）。
    pub reported_at_ms: Option<u64>,
    /// 只读监视器（门磁接触态、夜灯光照、可控设备的当前状态等）：
    /// 只读通道，永不进 allowlist（DESIGN.md §4.3/§9.2）。
    pub monitors: Vec<Reading>,
    pub source: &'static str,
    pub observed_at_ms: u64,
}

// ───────────────────────────── 策略数据结构 ─────────────────────────────

/// 属性坐标（policy.json 内部用，绝不出现在工具输出里）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PropRef {
    pub siid: i64,
    pub piid: i64,
}

/// 一次写入：要么策略里给死值，要么取模型给的 `value`（`from_request`）。
/// 值域写在策略里，与 miot-spec 的 `value-range` 对齐（本期实测：p5c
/// speed-level = [1,100,1]，见 DESIGN.md §9.2）。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WriteSpec {
    pub siid: i64,
    pub piid: i64,
    #[serde(default)]
    pub value: Option<Value>,
    #[serde(default)]
    pub from_request: bool,
    #[serde(default)]
    pub min: Option<f64>,
    #[serde(default)]
    pub max: Option<f64>,
    /// 该属性只接受整数（如 speed-level 1..100）；为 true 时 `value` 必须是整数值。
    #[serde(default)]
    pub integer: bool,
}

/// 一个友好动作 = 一组属性写入 + 一组回读坐标。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionSpec {
    /// 给模型看的说明（`list_capabilities` 会原样回显，因此**不得**写 did/坐标）。
    #[serde(default)]
    pub description: Option<String>,
    pub writes: Vec<WriteSpec>,
    #[serde(default)]
    pub verify: Vec<PropRef>,
}

impl ActionSpec {
    pub fn takes_request_value(&self) -> bool {
        self.writes.iter().any(|w| w.from_request)
    }
    /// 结构自检（策略加载时调用，返回人类可读的毛病描述）。
    fn validate(&self) -> Result<(), String> {
        if self.writes.is_empty() {
            return Err("writes 为空".into());
        }
        for w in &self.writes {
            match (w.value.as_ref(), w.from_request) {
                (Some(_), false) => {}
                (None, true) => {
                    if w.min.is_none() || w.max.is_none() {
                        return Err(format!("s{}/p{} from_request 缺值域（min/max）", w.siid, w.piid));
                    }
                    if w.min > w.max {
                        return Err(format!("s{}/p{} min>max", w.siid, w.piid));
                    }
                }
                (Some(_), true) => {
                    return Err(format!("s{}/p{} 同时给了 value 与 from_request", w.siid, w.piid));
                }
                (None, false) => {
                    return Err(format!("s{}/p{} 既无 value 也未标 from_request", w.siid, w.piid));
                }
            }
        }
        Ok(())
    }
}

/// 友好名 → 设备坐标 + 该设备允许的动作集合。
/// **只实现 Deserialize**：`did` 只在进程内，绝不出现在工具输出里。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntityBinding {
    pub did: String,
    /// 机型（如 `dmaker.fan.p5c`），用于 §4.3 的硬闸校验。
    pub model: String,
    pub actions: HashMap<String, ActionSpec>,
    /// 静默时段（用户守则：深夜不操作示例灯等物理设备）。策略数据，非模型判断。
    #[serde(default)]
    pub quiet_hours: Option<QuietWindow>,
}

/// `[start_minute, end_minute)` 的本地时间窗口（跨零点允许 start>end）。
/// 本地时间 = UTC 秒 + `utc_offset_secs`，不引时区 crate。
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuietWindow {
    pub utc_offset_secs: i64,
    pub start_minute: u32,
    pub end_minute: u32,
}

impl QuietWindow {
    pub fn contains(&self, now_secs: i64) -> bool {
        let local_min = {
            let secs = now_secs + self.utc_offset_secs;
            let day = secs.rem_euclid(86_400);
            (day / 60) as u32
        };
        if self.start_minute <= self.end_minute {
            local_min >= self.start_minute && local_min < self.end_minute
        } else {
            local_min >= self.start_minute || local_min < self.end_minute
        }
    }
}

#[derive(Debug, Default)]
pub struct Allowlist(pub HashMap<String, EntityBinding>);

impl Allowlist {
    pub fn resolve(&self, friendly: &str) -> Result<&EntityBinding, PolicyError> {
        self.0
            .get(friendly)
            .ok_or_else(|| PolicyError::UnknownEntity(friendly.to_owned()))
    }
    /// 确定性顺序（2026-07-28 建议稳定输出以利 prompt cache）。
    pub fn names(&self) -> Vec<&String> {
        let mut v: Vec<&String> = self.0.keys().collect();
        v.sort();
        v
    }
}

/// 温湿度传感器坐标（`read_environment` 的主读值源）。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TempSensor {
    pub did: String,
    pub siid_temp: i64,
    pub piid_temp: i64,
    #[serde(default)]
    pub siid_humidity: Option<i64>,
    #[serde(default)]
    pub piid_humidity: Option<i64>,
    #[serde(default)]
    pub siid_battery: Option<i64>,
    #[serde(default)]
    pub piid_battery: Option<i64>,
}

impl TempSensor {
    /// 一次批量读的全部坐标（顺序 = 解析顺序）。
    pub fn props(&self) -> Vec<PropRef> {
        let mut v = vec![PropRef { siid: self.siid_temp, piid: self.piid_temp }];
        if let (Some(s), Some(p)) = (self.siid_humidity, self.piid_humidity) {
            v.push(PropRef { siid: s, piid: p });
        }
        if let (Some(s), Some(p)) = (self.siid_battery, self.piid_battery) {
            v.push(PropRef { siid: s, piid: p });
        }
        v
    }
}

/// 只读监视器（门磁 / 夜灯光照 / 可控设备的当前状态）。
/// 永远只出现在 `read_environment` 里，不给模型一个能遍历设备的读工具。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MonitorRef {
    pub did: String,
    pub siid: i64,
    pub piid: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RiskClass {
    Auto,
    Approval,
}

/// 每会话用户授权开关，**默认关**（q3 严格收口：低风险自动执行的前提）。
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionGrant {
    pub auto_enabled: bool,
}

/// 「低风险」的封闭枚举白名单：策略数据，不是模型判断。
/// - `auto_actions`：只有显式列出的 `(entity, action)` 才可能免审批；
/// - `approved_plan_ids`：带外批准的一次性句柄（进程内池；文件通道见 §5.2）；
/// - `cooldown_secs`：同一实体的最小执行间隔。
#[derive(Debug, Default)]
pub struct AutoPolicy {
    pub auto_actions: HashSet<(String, String)>,
    pub approved_plan_ids: HashSet<String>,
    pub cooldown_secs: u64,
}

impl AutoPolicy {
    /// 带外批准句柄：命中即消费，返回 true（等价于「已批准」）。
    pub fn consume_approval(&mut self, plan_id: &str) -> bool {
        self.approved_plan_ids.remove(plan_id)
    }
}

/// `policy.json` 的加载形状（DESIGN.md §8）。
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyFile {
    pub allowlist: HashMap<String, EntityBinding>,
    pub temp_sensor: Option<TempSensor>,
    #[serde(default)]
    pub monitors: HashMap<String, MonitorRef>,
    #[serde(default)]
    pub auto_actions: Vec<(String, String)>,
    #[serde(default)]
    pub grant: SessionGrant,
    pub cooldown_secs: Option<u64>,
}

/// 加载完成后的运行期策略包（`main.rs` 拆字段用）。
#[derive(Debug)]
pub struct Policy {
    pub allow: Allowlist,
    pub temp_sensor: Option<TempSensor>,
    pub monitors: Vec<(String, MonitorRef)>,
    pub auto: AutoPolicy,
    pub grant: SessionGrant,
    pub cooldown: Cooldown,
}

impl PolicyFile {
    /// 结构自检 + 硬闸。任何毛病都让进程**启动失败**，不静默降级（宪规 5）。
    pub fn validate(&self) -> Result<(), String> {
        for (name, b) in &self.allowlist {
            if FORBIDDEN_DIDS.contains(&b.did.as_str())
                || FORBIDDEN_MODEL_PREFIXES.iter().any(|p| b.model.starts_with(p))
            {
                return Err(format!(
                    "实体「{name}」= 机型 {} 属于永不控制清单（DESIGN.md §4.3）",
                    b.model
                ));
            }
            if b.actions.is_empty() {
                return Err(format!("实体「{name}」没有可用动作，应从 allowlist 移除"));
            }
            for (aname, spec) in &b.actions {
                spec.validate()
                    .map_err(|e| format!("实体「{name}」动作「{aname}」: {e}"))?;
            }
        }
        for a in &self.auto_actions {
            if !self.allowlist.contains_key(&a.0) {
                return Err(format!("auto_actions 引用未登记实体「{}」", a.0));
            }
            let b = &self.allowlist[&a.0];
            if !b.actions.contains_key(&a.1) {
                return Err(format!(
                    "auto_actions 引用未登记动作「{}」.「{}」",
                    a.0, a.1
                ));
            }
        }
        if let Some(ts) = &self.temp_sensor {
            for (a, b, what) in [
                (ts.siid_humidity, ts.piid_humidity, "湿度"),
                (ts.siid_battery, ts.piid_battery, "电量"),
            ] {
                if a.is_some() != b.is_some() {
                    return Err(format!("temp_sensor {what} 坐标不完整（siid/piid 需同时给）"));
                }
            }
        }
        Ok(())
    }

    pub fn into_policy(self) -> Result<Policy, String> {
        self.validate()?;
        let cooldown_secs = self.cooldown_secs.unwrap_or(60);
        let auto = AutoPolicy {
            auto_actions: self.auto_actions.into_iter().collect(),
            approved_plan_ids: HashSet::new(),
            cooldown_secs,
        };
        let mut monitors: Vec<(String, MonitorRef)> = self.monitors.into_iter().collect();
        monitors.sort_by(|a, b| a.0.cmp(&b.0)); // 确定性顺序（prompt cache）
        Ok(Policy {
            allow: Allowlist(self.allowlist),
            temp_sensor: self.temp_sensor,
            monitors,
            auto,
            grant: self.grant,
            cooldown: Cooldown::new(cooldown_secs),
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    #[error("unknown entity: {0}")]
    UnknownEntity(String),
    #[error("unknown action for entity {0}: {1}")]
    UnknownAction(String, String),
    #[error("action requires a numeric value")]
    ValueRequired,
    #[error("action takes no value")]
    UnexpectedValue,
    #[error("value out of range [{min}, {max}]: {value}")]
    ValueOutOfRange { value: f64, min: f64, max: f64 },
    #[error("value must be an integer: {0}")]
    ValueNotInteger(f64),
    #[error("entity is in its quiet-hours window")]
    QuietHours,
    #[error("reason exceeds {0} chars")]
    ReasonTooLong(usize),
    #[error("unknown or consumed plan_id: {0}")]
    UnknownPlan(String),
    #[error("approval required for plan_id: {0}")]
    ApprovalRequired(String),
    #[error("cooldown active for entity: {0}")]
    CooldownActive(String),
    #[error("no temp_sensor configured")]
    NoTempSensor,
    #[error("sensor read failed: {0}")]
    SensorReadFailed(String),
    #[error("cloud: {0}")]
    Cloud(#[from] CloudError),
}

/// 工具级错误码（可达模型；分类见 DESIGN.md §6）。
pub fn policy_error_code(e: &PolicyError) -> &'static str {
    match e {
        PolicyError::UnknownEntity(_) => "unknown_entity",
        PolicyError::UnknownAction(..) => "unknown_action",
        PolicyError::ValueRequired => "value_required",
        PolicyError::UnexpectedValue => "unexpected_value",
        PolicyError::ValueOutOfRange { .. } => "value_out_of_range",
        PolicyError::ValueNotInteger(_) => "value_not_integer",
        PolicyError::QuietHours => "quiet_hours",
        PolicyError::ReasonTooLong(_) => "reason_too_long",
        PolicyError::UnknownPlan(_) => "unknown_or_consumed_plan",
        PolicyError::ApprovalRequired(_) => "approval_required",
        PolicyError::CooldownActive(_) => "cooldown_active",
        PolicyError::NoTempSensor => "no_temp_sensor",
        PolicyError::SensorReadFailed(_) => "sensor_read_failed",
        PolicyError::Cloud(c) => c.code(),
    }
}

/// 工具入参：closed-set + `deny_unknown_fields`（ADR-0004 `strict-input`）。
/// 模型只给友好名 + 友好动作 + 可选数值，永远不给 did/siid/piid；
/// 多一个字段即报错，不静默接受。
///
/// `reason` 是**给审批人看的一句话**（DESIGN.md §3.5）：只进 pending 文件与审计，
/// **完全不参与 `classify` 的风险判定**——授权不能来自模型的措辞。长度封顶，
/// 免得带外通道被写成模型的控制台。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActionRequest {
    pub entity: String,
    pub action: String,
    pub value: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// `reason` 的硬上限（字符数，按 `chars()` 计）。
pub const REASON_MAX_CHARS: usize = 200;

/// 策略层判风险：先校验 allowlist / 动作名 / 值形 / 值域 / 静默时段，
/// 再查封闭白名单。模型不参与此判定。`now_secs` 注入以便单测（生产传 [`now_secs`]）。
pub fn classify(
    req: &ActionRequest,
    allow: &Allowlist,
    policy: &AutoPolicy,
    now_secs: i64,
) -> Result<RiskClass, PolicyError> {
    // 唯一使用 `reason` 的地方：长度上限。它**不**参与风险判定（DESIGN.md §3.5/§4.1）。
    if let Some(r) = &req.reason {
        if r.chars().count() > REASON_MAX_CHARS {
            return Err(PolicyError::ReasonTooLong(REASON_MAX_CHARS));
        }
    }
    let binding = allow.resolve(&req.entity)?;
    let spec = binding
        .actions
        .get(&req.action)
        .ok_or_else(|| PolicyError::UnknownAction(req.entity.clone(), req.action.clone()))?;

    match (spec.takes_request_value(), req.value) {
        (true, None) => return Err(PolicyError::ValueRequired),
        (false, Some(_)) => return Err(PolicyError::UnexpectedValue),
        (true, Some(v)) => {
            for w in spec.writes.iter().filter(|w| w.from_request) {
                let (min, max) = (w.min.unwrap_or(f64::MIN), w.max.unwrap_or(f64::MAX));
                if !(min..=max).contains(&v) {
                    return Err(PolicyError::ValueOutOfRange { value: v, min, max });
                }
                if w.integer && v.fract() != 0.0 {
                    return Err(PolicyError::ValueNotInteger(v));
                }
            }
        }
        (false, None) => {}
    }

    if let Some(q) = &binding.quiet_hours {
        if q.contains(now_secs) {
            return Err(PolicyError::QuietHours);
        }
    }

    Ok(
        if policy
            .auto_actions
            .contains(&(req.entity.clone(), req.action.clone()))
        {
            RiskClass::Auto
        } else {
            RiskClass::Approval
        },
    )
}

/// 是否免审批自动执行：策略判为 Auto **且** 本会话用户已打开授权开关。
pub fn may_auto_execute(class: RiskClass, grant: SessionGrant) -> bool {
    class == RiskClass::Auto && grant.auto_enabled
}

/// 冷却：同一实体在 `min_interval_ms` 内不允许第二次物理动作（防设备抖动）。
#[derive(Debug)]
pub struct Cooldown {
    min_interval_ms: u64,
    last: HashMap<String, u64>,
}

impl Default for Cooldown {
    fn default() -> Self {
        Self::new(60)
    }
}

impl Cooldown {
    pub fn new(secs: u64) -> Self {
        Self {
            min_interval_ms: secs.saturating_mul(1000),
            last: HashMap::new(),
        }
    }

    /// 只看窗口，不标记（批准消费前先用它挡一道，避免冷却把带外批准烧掉）。
    pub fn check(&self, entity: &str) -> Result<(), PolicyError> {
        match self.last.get(entity) {
            Some(prev) if now_ms().saturating_sub(*prev) < self.min_interval_ms => {
                Err(PolicyError::CooldownActive(entity.to_owned()))
            }
            _ => Ok(()),
        }
    }

    /// 记一次物理动作时刻（与 `PlanStore::take` 同批做，保证「已消费 = 已进冷却」）。
    pub fn mark(&mut self, entity: &str) {
        self.last.insert(entity.to_owned(), now_ms());
    }

    pub fn check_and_mark(&mut self, entity: &str) -> Result<(), PolicyError> {
        self.check(entity)?;
        self.mark(entity);
        Ok(())
    }
}

/// server-minted handle（SEP-2567）：2026-07-28 无 session，跨调用状态只能
/// 作为普通工具参数传递，`plan_id` 就是这个句柄。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub plan_id: String,
    pub entity: String,
    pub action: String,
    pub value: Option<f64>,
    /// 模型给审批人的理由（原样透传，不做风险判定）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub approved: bool,
    pub created_at_ms: u64,
}

#[derive(Debug, Default)]
pub struct PlanStore(HashMap<String, Plan>);

impl PlanStore {
    pub fn mint(&mut self, req: &ActionRequest, approved: bool) -> Plan {
        let plan = Plan {
            plan_id: mint_plan_id(),
            entity: req.entity.clone(),
            action: req.action.clone(),
            value: req.value,
            reason: req.reason.clone(),
            approved,
            created_at_ms: now_ms(),
        };
        self.0.insert(plan.plan_id.clone(), plan.clone());
        plan
    }

    /// 只看不吃：批准/冷却失败时不能把 plan 烧掉（DESIGN.md §5.1 执行顺序）。
    pub fn peek(&self, plan_id: &str) -> Option<&Plan> {
        self.0.get(plan_id)
    }

    /// 取出并消费（一次性），防止同一个物理动作被重复执行。
    pub fn take(&mut self, plan_id: &str) -> Result<Plan, PolicyError> {
        self.0
            .remove(plan_id)
            .ok_or_else(|| PolicyError::UnknownPlan(plan_id.to_owned()))
    }
}

/// 句柄不可预测：时间戳 + 进程内单调计数 + 进程随机盐（`RandomState` 的
/// hasher 每进程随机播种）。plan_id 不是机密，但「模型自己猜出别人的批准句柄」
/// 必须是碰运气而不是可推导——真正的授权来自带外 approvals 文件（§5.2）。
fn mint_plan_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    let salt = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish();
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("p-{:x}-{:x}-{:x}", now_ms(), seq, salt & 0xff_ffff_ffff)
}

/// 审计记录。**不含 did/siid/piid**（友好名 + plan_id + 观测值）。
/// 2026-07-28 移除 SSE 重传 ⇒ 断流会丢在途请求，幂等键（`plan_id` 一次性消费）
/// 与冷却必须自实现，不依赖传输重发。
#[derive(Debug, Clone, Serialize)]
pub struct AuditRecord {
    pub plan_id: String,
    pub entity: String,
    pub action: String,
    pub value: Option<f64>,
    /// 模型申报的理由（审计里保留，供人工回看「当时它是怎么说的」）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub at_ms: u64,
    pub ok: bool,
    /// 失败/告警的错误码（ok=false 时非空）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<&'static str>,
    /// `set_properties` 后按 `verify` 顺序回读到的观测值
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_after: Option<Vec<Value>>,
}

// ───────────────────────────── 云侧边界 ─────────────────────────────

/// 云侧结构化错误。**不用 `String` 吞掉错误**：`cloud.rs` 对
/// `mijia_core::Error` 做穷尽 match 映射到这些变体（DESIGN.md §6）。
#[derive(Debug, Clone, thiserror::Error)]
pub enum CloudError {
    #[error("cloud backend not wired (DESIGN.md §11 落地清单)")]
    NotWired,
    #[error("no session / need relogin: {0}")]
    NeedRelogin(String),
    #[error("cloud rejected code={0}")]
    Rejected(i64),
    #[error("credentials invalid or risk-controlled")]
    CredentialInvalid,
    #[error("response shape invalid: {0}")]
    ShapeInvalid(String),
    #[error("transport error: {0}")]
    Transport(String),
    #[error("device rejected the write (code={0})")]
    WriteRejected(i64),
    #[error("readback mismatch after write")]
    VerifyMismatch,
    #[error("readback returned no value")]
    VerifyEmpty,
}

impl CloudError {
    /// 工具级错误码（可达模型，但不泄露凭据/URL 以外的任何内容）。
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotWired => "cloud_not_wired",
            Self::NeedRelogin(_) => "need_relogin",
            Self::Rejected(_) => "cloud_rejected",
            Self::CredentialInvalid => "credential_invalid",
            Self::ShapeInvalid(_) => "shape_invalid",
            Self::Transport(_) => "cloud_transport",
            Self::WriteRejected(_) => "write_rejected",
            Self::VerifyMismatch => "verify_mismatch",
            Self::VerifyEmpty => "verify_empty",
        }
    }
}

pub type CloudResult<T> = Result<T, CloudError>;

/// `mijia_core::PropValue` 的领域化投影（`device.rs:31-42`），避免领域层依赖 SDK。
#[derive(Debug, Clone)]
pub struct PropSnapshot {
    pub did: String,
    pub siid: i64,
    pub piid: i64,
    pub code: i64,
    pub value: serde_json::Value,
    /// 云端缓存上报时刻（`/miotspec/prop/get` 的 updateTime；实时读为 None）。
    pub update_time: Option<i64>,
}

/// 云侧边界：由 `cloud::MijiaCloud` 实现（`MiHome` 门面）。
/// trait 隔离让领域层与策略单测不绑定具体 SDK；`async` 是必须的（云端是 tokio 异步）。
#[async_trait::async_trait]
pub trait HomeCloud: Send + Sync {
    /// `/miotspec/prop/get`（主，全通道缓存）→ 失败回落 `/home/rpcv2/<did>`。
    async fn get_properties(&self, did: &str, props: &[PropRef]) -> CloudResult<Vec<PropSnapshot>>;
    /// 控制下发后的**实时**回读（rpcv2 穿透，避开云缓存滞后）。
    async fn get_properties_realtime(
        &self,
        did: &str,
        props: &[PropRef],
    ) -> CloudResult<Vec<PropSnapshot>>;
    /// `/home/rpcv2/<did>` `set_properties`（`blt.*` 走 `/miotspec/prop/set`）
    async fn set_properties(&self, did: &str, props: &[(i64, i64, Value)]) -> CloudResult<()>;
    /// **文件里有会话** ≠ 会话有效。实现方不许拿这个当健康判据
    /// （`mijia_core::MiHome::is_logged_in()` 就是这个语义，名字有误导性，§7.6）。
    async fn has_session_file(&self) -> bool;
    /// 真探活：拉一次账号 API（`list_devices`）。错误形状供 §7.6 决定是否续期。
    async fn probe(&self) -> CloudResult<()>;
    /// 用 `passToken` 静默换一次新会话（账号域调用，会**重写凭据文件**）。
    /// 返回值 = 是否真换到了新会话。**该不该调**由 `session::may_refresh_session`
    /// 决定（默认关），实现方不许自己找理由调（§7.6）。
    async fn refresh(&self) -> CloudResult<bool>;
}

// ───────────────────────────── 策略不变量单测 ─────────────────────────────
// 只为策略不变量写测试（DESIGN.md §10.2）：值域、封闭白名单、默认拒绝、
// 一次性句柄、冷却、静默时段。不为「开风扇」这个功能堆测试。

#[cfg(test)]
mod tests {
    use super::*;

    fn fan() -> serde_json::Value {
        serde_json::json!({
            "did": "900000010",
            "model": "dmaker.fan.p5c",
            "actions": {
                "on":  {"writes":[{"siid":2,"piid":1,"value":true}],"verify":[{"siid":2,"piid":1}]},
                "off": {"writes":[{"siid":2,"piid":1,"value":false}],"verify":[{"siid":2,"piid":1}]},
                "set_speed": {
                    "writes":[{"siid":8,"piid":1,"from_request":true,"min":1.0,"max":100.0,"integer":true}],
                    "verify":[{"siid":8,"piid":1}]
                }
            }
        })
    }

    fn policy_json(allow_extra: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "allowlist": { "落地扇": allow_extra },
            "temp_sensor": {"did":"sensor-did","siid_temp":2,"piid_temp":1001,
                             "siid_humidity":2,"piid_humidity":1002},
            "auto_actions": [["落地扇","off"]],
            "grant": {"auto_enabled": true},
            "cooldown_secs": 60
        })
    }

    fn load(v: serde_json::Value) -> Policy {
        serde_json::from_value::<PolicyFile>(v)
            .expect("policy deserializes")
            .into_policy()
            .expect("policy validates")
    }

    fn req(entity: &str, action: &str, value: Option<f64>) -> ActionRequest {
        ActionRequest {
            entity: entity.into(),
            action: action.into(),
            value,
            reason: None,
        }
    }

    /// 2026-11-17 02:00 +08:00（= 1794852000 UTC）—— 落在示例灯的静默时段内。
    const NIGHT: i64 = 1_794_852_000;
    /// 同一天的 14:00 +08:00 —— 静默时段之外。
    const DAY: i64 = NIGHT + 12 * 3600;

    #[test]
    fn unknown_entity_and_action_are_rejected() {
        let p = load(policy_json(fan()));
        let e = classify(&req("电暖风", "off", None), &p.allow, &p.auto, DAY).unwrap_err();
        assert_eq!(policy_error_code(&e), "unknown_entity");
        let e = classify(&req("落地扇", "boost", None), &p.allow, &p.auto, DAY).unwrap_err();
        assert_eq!(policy_error_code(&e), "unknown_action");
    }

    #[test]
    fn value_field_is_strictly_required_or_rejected() {
        let p = load(policy_json(fan()));
        let e = classify(&req("落地扇", "set_speed", None), &p.allow, &p.auto, DAY).unwrap_err();
        assert_eq!(policy_error_code(&e), "value_required");
        let e = classify(&req("落地扇", "off", Some(50.0)), &p.allow, &p.auto, DAY).unwrap_err();
        assert_eq!(policy_error_code(&e), "unexpected_value");
        let e = classify(&req("落地扇", "set_speed", Some(101.0)), &p.allow, &p.auto, DAY)
            .unwrap_err();
        assert_eq!(policy_error_code(&e), "value_out_of_range");
        let e = classify(&req("落地扇", "set_speed", Some(66.5)), &p.allow, &p.auto, DAY)
            .unwrap_err();
        assert_eq!(policy_error_code(&e), "value_not_integer");
        assert_eq!(
            classify(&req("落地扇", "set_speed", Some(66.0)), &p.allow, &p.auto, DAY).unwrap(),
            RiskClass::Approval,
            "set_speed 不在 auto_actions 里 ⇒ 默认要审批"
        );
    }

    #[test]
    fn deny_unknown_request_fields() {
        // 模型多塞 did/siid 必须直接报错，不静默接受（ADR-0004 strict-input）
        let bad = serde_json::json!({"entity":"落地扇","action":"off","did":"1"});
        assert!(serde_json::from_value::<ActionRequest>(bad).is_err());
    }

    #[test]
    fn auto_requires_both_whitelist_and_grant() {
        let p = load(policy_json(fan()));
        let on = SessionGrant { auto_enabled: true };
        let off = SessionGrant { auto_enabled: false };
        let c = classify(&req("落地扇", "off", None), &p.allow, &p.auto, DAY).unwrap();
        assert_eq!(c, RiskClass::Auto);
        assert!(may_auto_execute(c, on));
        assert!(!may_auto_execute(c, off), "会话开关关 ⇒ 白名单也不放行");
        let c = classify(&req("落地扇", "on", None), &p.allow, &p.auto, DAY).unwrap();
        assert_eq!(c, RiskClass::Approval);
        assert!(!may_auto_execute(c, on), "不在白名单 ⇒ 开关开着也不放行");
    }

    #[test]
    fn quiet_hours_blocks_the_entity_only_during_the_window() {
        let mut f = fan();
        f["quiet_hours"] =
            serde_json::json!({"utc_offset_secs": 28800, "start_minute": 1380, "end_minute": 480});
        let p = load(policy_json(f));
        assert_eq!(
            policy_error_code(
                &classify(&req("落地扇", "off", None), &p.allow, &p.auto, NIGHT).unwrap_err()
            ),
            "quiet_hours"
        );
        assert!(classify(&req("落地扇", "off", None), &p.allow, &p.auto, DAY).is_ok());
    }

    #[test]
    fn quiet_window_wraps_midnight() {
        let q = QuietWindow { utc_offset_secs: 28800, start_minute: 1380, end_minute: 480 };
        assert!(q.contains(NIGHT), "02:00");
        assert!(!q.contains(DAY), "14:00");
        assert!(q.contains(NIGHT - 3 * 3600), "23:00");
    }

    #[test]
    fn plan_handle_is_single_use_and_unpredictable() {
        let mut store = PlanStore::default();
        let a = store.mint(&req("落地扇", "off", None), false);
        let b = store.mint(&req("落地扇", "off", None), false);
        assert_ne!(a.plan_id, b.plan_id, "同一毫秒内 mint 也必须拿到不同句柄");
        assert_eq!(a.plan_id.split('-').count(), 4, "句柄必须带进程随机盐段");
        assert!(store.peek(&a.plan_id).is_some(), "peek 不吃句柄");
        assert!(store.take(&a.plan_id).is_ok());
        let e = store.take(&a.plan_id).unwrap_err();
        assert_eq!(policy_error_code(&e), "unknown_or_consumed_plan");
    }

    #[test]
    fn approval_handle_is_consumed_once() {
        let mut p = AutoPolicy::default();
        p.approved_plan_ids.insert("p-x".into());
        assert!(p.consume_approval("p-x"));
        assert!(!p.consume_approval("p-x"));
    }

    #[test]
    fn cooldown_serializes_the_same_entity() {
        let mut c = Cooldown::new(3600);
        assert!(c.check("落地扇").is_ok(), "未标记前不该有冷却");
        assert!(c.check_and_mark("落地扇").is_ok());
        let e = c.check_and_mark("落地扇").unwrap_err();
        assert_eq!(policy_error_code(&e), "cooldown_active");
        assert!(c.check_and_mark("示例灯").is_ok(), "冷却按实体隔离");
    }

    #[test]
    fn air_conditioner_is_hard_blocked_in_policy_loading() {
        // 守则：空调永不控制。即便有人把空调写进 policy.json，也必须启动失败。
        let mut f = fan();
        f["model"] = serde_json::json!("xiaomi.aircondition.m4");
        let err = serde_json::from_value::<PolicyFile>(policy_json(f))
            .unwrap()
            .into_policy()
            .unwrap_err();
        assert!(err.contains("永不控制"), "{err}");

        // did 维度的硬拦：**引用常量而不是写死真实 did**。
        // 一则是真实 did 不该散落在源码里（这份代码要能公开），二则写死之后
        // 改 `FORBIDDEN_DIDS` 就会让这个用例假绿/假红——本轮就踩过一次：
        // 把测试里的 did 换成合成值后，这条断言立刻失效，说明它测的其实是
        // "那个字面量在不在名单里"，而不是"名单生效没有"。
        let mut g = fan();
        g["did"] = serde_json::json!(FORBIDDEN_DIDS[0]);
        assert!(serde_json::from_value::<PolicyFile>(policy_json(g))
            .unwrap()
            .into_policy()
            .is_err());
        // 反向确认：名单外的 did 仍能通过（否则上面那条 is_err 会因为"全都拒"而恒真）
        let mut ok = fan();
        ok["did"] = serde_json::json!("not-in-forbidden-list");
        assert!(
            serde_json::from_value::<PolicyFile>(policy_json(ok))
                .unwrap()
                .into_policy()
                .is_ok(),
            "非名单 did 应可通过，否则这条测试没有判别力"
        );

        // 空 actions 的可控实体也不许登记（防止「先进清单再手工塞动作」）
        let mut h = fan();
        h["actions"] = serde_json::json!({});
        assert!(serde_json::from_value::<PolicyFile>(policy_json(h))
            .unwrap()
            .into_policy()
            .is_err());
    }

    #[test]
    fn malformed_write_specs_are_rejected_at_load() {
        for (label, w) in [
            (
                "既无 value 也未标 from_request",
                serde_json::json!({"siid":2,"piid":1}),
            ),
            (
                "from_request 缺值域",
                serde_json::json!({"siid":8,"piid":1,"from_request":true}),
            ),
            (
                "同时给了 value 与 from_request",
                serde_json::json!({"siid":2,"piid":1,"value":true,"from_request":true}),
            ),
        ] {
            let mut f = fan();
            f["actions"]["on"]["writes"] = serde_json::json!([w]);
            let err = serde_json::from_value::<PolicyFile>(policy_json(f))
                .unwrap()
                .into_policy()
                .unwrap_err();
            assert!(err.contains(label), "{label} 应被拒绝，实际: {err}");
        }
        // 空 writes
        let mut f = fan();
        f["actions"]["on"]["writes"] = serde_json::json!([]);
        assert!(serde_json::from_value::<PolicyFile>(policy_json(f))
            .unwrap()
            .into_policy()
            .is_err());
    }

    #[test]
    fn auto_actions_must_reference_registered_entities_and_actions() {
        let mut v = policy_json(fan());
        v["auto_actions"] = serde_json::json!([["幽灵设备", "off"]]);
        let err = serde_json::from_value::<PolicyFile>(v)
            .unwrap()
            .into_policy()
            .unwrap_err();
        assert!(err.contains("未登记实体"), "{err}");

        let mut w = policy_json(fan());
        w["auto_actions"] = serde_json::json!([["落地扇", "自造动作"]]);
        let err = serde_json::from_value::<PolicyFile>(w)
            .unwrap()
            .into_policy()
            .unwrap_err();
        assert!(err.contains("未登记动作"), "{err}");
    }

    #[test]
    fn policy_rejects_unknown_fields() {
        // 有人想从配置面塞凭据/坐标进工具面 ⇒ 加载即失败
        let mut v = policy_json(fan());
        v["allowlist"]["落地扇"]["token"] = serde_json::json!("secret");
        assert!(serde_json::from_value::<PolicyFile>(v).is_err());
    }

    #[test]
    fn grant_defaults_to_off_when_absent() {
        let mut v = policy_json(fan());
        v.as_object_mut().unwrap().remove("grant");
        let p = load(v);
        assert!(!p.grant.auto_enabled, "缺省必须默认关（q3 收口）");
    }

    #[test]
    fn reason_never_changes_risk_but_is_length_capped() {
        let p = load(policy_json(fan()));
        // 同一动作：带不带理由，风险等级必须完全一致
        let mut with = req("落地扇", "off", None);
        with.reason = Some("太热了".into());
        assert_eq!(
            classify(&with, &p.allow, &p.auto, DAY).unwrap(),
            classify(&req("落地扇", "off", None), &p.allow, &p.auto, DAY).unwrap(),
        );
        let mut long = req("落地扇", "off", None);
        long.reason = Some("x".repeat(REASON_MAX_CHARS + 1));
        let e = classify(&long, &p.allow, &p.auto, DAY).unwrap_err();
        assert_eq!(policy_error_code(&e), "reason_too_long");
        // 越界值 + 理由：仍然先报值域（理由不能替越界开脱）
        let mut bad = req("落地扇", "set_speed", Some(999.0));
        bad.reason = Some("很热".into());
        assert_eq!(
            policy_error_code(&classify(&bad, &p.allow, &p.auto, DAY).unwrap_err()),
            "value_out_of_range"
        );
    }

    #[test]
    fn reason_is_carried_into_plan_handle() {
        let mut store = PlanStore::default();
        let mut r = req("落地扇", "set_speed", Some(66.0));
        r.reason = Some("室温 27.4℃".into());
        let plan = store.mint(&r, false);
        assert_eq!(plan.reason.as_deref(), Some("室温 27.4℃"));
        assert_eq!(serde_json::to_value(&plan).unwrap()["reason"], "室温 27.4℃");
        // 没给理由就不该在句柄里冒空字段
        let bare = store.mint(&req("落地扇", "set_speed", Some(66.0)), false);
        assert!(serde_json::to_value(&bare).unwrap().get("reason").is_none());
    }

    #[test]
    fn temp_sensor_props_are_batched_and_pairs_required() {
        let ts: TempSensor = serde_json::from_value(serde_json::json!(
            {"did":"x","siid_temp":2,"piid_temp":1001,
             "siid_humidity":2,"piid_humidity":1002,"siid_battery":3,"piid_battery":1003}
        ))
        .unwrap();
        assert_eq!(ts.props().len(), 3, "一次批量读三个坐标");

        let broken = serde_json::json!(
            {"did":"x","siid_temp":2,"piid_temp":1001,"siid_humidity":2}
        );
        let mut v = policy_json(fan());
        v["temp_sensor"] = broken;
        let err = serde_json::from_value::<PolicyFile>(v)
            .unwrap()
            .into_policy()
            .unwrap_err();
        assert!(err.contains("湿度"), "{err}");
    }

    #[test]
    fn tool_set_is_closed() {
        let names: Vec<&str> = ToolName::ALL.iter().map(|t| t.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "read_environment",
                "list_capabilities",
                "propose_action",
                "execute_action"
            ]
        );
    }
}
