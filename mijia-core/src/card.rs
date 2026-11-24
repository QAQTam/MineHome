//! # 卡片模板引擎（方案 A：云端模板消费全在 Rust，ArkTS 只渲染归一化 DTO）
//!
//! 数据链路（见 `_work/card_design.md` 第五/六节）：
//! - 米家云端 `getappconfig` 下发 `card_control_miotspec_config`（75 条 miotspec 模板）、
//!   `card_control_config`（老卡配置）、`card_default_txt`（多语言文案）。
//! - 本模块负责：**解析 → 按 `spec_type` 类别匹配 `types` / 过滤 `disable_type` →
//!   `prop_key` 解析成 siid/piid → 合并 zh_CN 文案/单位/枚举 → `disable_status`
//!   归一化为条件并按当前属性值求值 `enabled` → 模板未命中时 spec 驱动兜底**。
//! - 抓取与磁盘缓存在 [`crate::card_net`]；napi 薄壳透出 [`CardItemVm`] 的 DTO。
//!
//! 设计原则：本模块**零网络、零鸿蒙依赖**，可用 `_work/card_config_*.json`
//! 做夹具在 PC 上 `cargo test` 秒级回归。

use crate::device::PropValue;
use crate::error::{Error, Result};
use serde_json::Value;
use std::collections::HashMap;

// ------------------------------------------------------------------ DTO ----

/// 组件类型（cardType → 归一化 kind）。本期起二级弹层控制类**可交互下发**
/// （用户显式授权）；首页卡片仍按只读预览渲染。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CardKind {
    /// 电源开关 Toggle（cardType 1）
    Toggle,
    /// 动作/复合按钮（cardType 2）
    Action,
    /// 枚举分段（cardType 3 / 12）
    EnumChoice,
    /// 数值步进（cardType 4）
    Stepper,
    /// 滑条（cardType 5 / 11）
    SeekBar,
    /// 固件版本等文本（cardType 6）
    FirmwareText,
    /// 观察数值（cardType 7）
    ObserveNumber,
    /// 状态文本（cardType 8）
    StatusText,
    /// 传感器状态文本（cardType 18）
    SensorStatus,
    /// 挡位分段（cardType 12）
    Gear,
    /// 备用/隐藏项（cardType 101）——不进卡片
    Hidden,
}

impl CardKind {
    pub fn as_str(self) -> &'static str {
        match self {
            CardKind::Toggle => "toggle",
            CardKind::Action => "action",
            CardKind::EnumChoice => "enum_choice",
            CardKind::Stepper => "stepper",
            CardKind::SeekBar => "seek_bar",
            CardKind::FirmwareText => "firmware_text",
            CardKind::ObserveNumber => "observe_number",
            CardKind::StatusText => "status_text",
            CardKind::SensorStatus => "sensor_status",
            CardKind::Gear => "gear",
            CardKind::Hidden => "hidden",
        }
    }

    /// 控制类组件：二级弹层可交互下发；enabled 表示联动禁用态。
    pub fn is_control(self) -> bool {
        matches!(
            self,
            CardKind::Toggle
                | CardKind::Action
                | CardKind::EnumChoice
                | CardKind::Stepper
                | CardKind::SeekBar
                | CardKind::Gear
        )
    }
}

/// cardType 编码 → CardKind（从模板内容反推，见 card_design.md 第五节）。
pub fn kind_of(card_type: i64) -> CardKind {
    match card_type {
        1 => CardKind::Toggle,
        2 => CardKind::Action,
        3 => CardKind::EnumChoice,
        4 => CardKind::Stepper,
        5 | 11 => CardKind::SeekBar,
        6 => CardKind::FirmwareText,
        7 => CardKind::ObserveNumber,
        8 | 17 | 20 => CardKind::StatusText,
        12 => CardKind::Gear,
        18 => CardKind::SensorStatus,
        101 => CardKind::Hidden,
        _ => CardKind::StatusText,
    }
}

/// 枚举/挡位选项。`en` 为 spec value-list 的英文 description（供 desc_sort 排序匹配），
/// `label` 为中文文案（comment 或 card_default_txt 的 zh_CN）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct CardChoice {
    pub value: Value,
    pub label: String,
    pub en: String,
}

/// 联动禁用条件：`disable_status` 归一化后，按 (siid,piid) 的当前值与 `expect` 比较。
#[derive(Debug, Clone, serde::Serialize)]
pub struct CardCond {
    pub siid: i64,
    pub piid: i64,
    /// 归一化后的期望值（见 [`norm_value`]）
    pub expect: String,
}

/// 归一化卡片项（ArkTS 唯一消费的 DTO）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct CardItemVm {
    pub kind: CardKind,
    /// `prop_key` 已解析成地址（pi: 直寻址 / p: 经 spec 查 iid）
    pub siid: i64,
    pub piid: i64,
    pub label: String,
    pub unit: String,
    /// 显示格式（"%.1f" / "%.0f" 或 spec format）
    pub format: String,
    /// 数值缩放（如大气压 ratio 0.001）
    pub ratio: f64,
    pub choices: Vec<CardChoice>,
    pub disabled_when: Vec<CardCond>,
    /// 按当前属性值求值后的联动启用态
    pub enabled: bool,
    /// 当前读值（[`CardEngine::evaluate`] 回填）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    /// 单项读取 code（code!=0 隐藏/标记失败）
    pub code: i64,
    /// 观察值是否忽略 label（模板 `prop_name_ignore`）
    pub name_ignore: bool,
    /// 控制值域下界（spec `value-range[0]`，二级滑条/步进用；首页不消费）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    /// 控制值域上界（spec `value-range[1]`）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    /// 控制值域步长（spec `value-range[2]`）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step: Option<f64>,
    /// 模板步进增量（`param_delta`，如空调目标温度 1）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub param_delta: Option<f64>,
    /// true = (siid,piid) 是**动作**地址（siid=服务 iid、piid=动作 iid），
    /// 下发走 action 通道而非 setProperty；属性项恒为 false。
    pub is_action: bool,
    /// 读值来源："local" = 本地 BLE 广播直收（比云端缓存新鲜，蓝牙收数落库）；
    /// None = 云端通道。由 card_net 的本地优先合并层回填，ArkTS 渲染来源徽标。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub src: Option<String>,
}

// ------------------------------------------------------------- CardSpec ----

/// spec 索引：把 miot-spec 服务/属性的 URN 名映射到 siid/piid，并带渲染元数据。
/// 两种构造：`from_instance`（真机，miot-spec.org instance JSON）、
/// `from_classify`（PC 单测夹具 `_work/spec_classify.json` 单台形态）。
#[derive(Debug, Clone, Default)]
pub struct CardSpec {
    pub services: Vec<CardService>,
}

#[derive(Debug, Clone)]
pub struct CardService {
    pub iid: i64,
    /// urnName(type)，如 "air-conditioner"
    pub name: String,
    pub props: Vec<CardProp>,
}

#[derive(Debug, Clone)]
pub struct CardProp {
    pub iid: i64,
    /// urnName(type)，如 "on"
    pub name: String,
    /// 英文 description
    pub desc: String,
    /// 中文 comment（miot-spec instance 带，优先作 label）
    pub comment: String,
    pub format: String,
    pub unit: String,
    pub access: Vec<String>,
    pub value_list: Vec<CardChoice>,
    pub range: Option<(f64, f64, f64)>,
    /// true = 该条目来自 spec 的 actions 数组（非属性），二级动作按钮用
    pub is_action: bool,
    /// 动作是否带 in 参数（带参动作本期不下发，不导出）
    pub has_in: bool,
}

impl CardSpec {
    /// 从 miot-spec.org instance JSON 构造（SpecKit 同款 URL 的响应）。
    pub fn from_instance(v: &Value) -> Self {
        let mut services = Vec::new();
        let empty: Vec<Value> = Vec::new();
        for s in v.get("services").and_then(|x| x.as_array()).unwrap_or(&empty) {
            let iid = s.get("iid").and_then(|x| x.as_i64()).unwrap_or(0);
            let name = urn_name(s.get("type").and_then(|x| x.as_str()).unwrap_or(""));
            let mut props = Vec::new();
            for p in s.get("properties").and_then(|x| x.as_array()).unwrap_or(&empty) {
                props.push(parse_instance_prop(p));
            }
            // 动作（a:svc:action）也纳入索引：iid 当 piid 用，仅用于 resolve（控制组件只读，不轮询）
            for a in s.get("actions").and_then(|x| x.as_array()).unwrap_or(&empty) {
                let mut ap = parse_instance_prop(a);
                ap.is_action = true;
                props.push(ap);
            }
            services.push(CardService { iid, name, props });
        }
        CardSpec { services }
    }

    /// 从 `_work/spec_classify.json` 的单台设备对象构造（PC 单测夹具）。
    pub fn from_classify(v: &Value) -> Self {
        let stats = v.get("stats").cloned().unwrap_or(Value::Null);
        let empty: Vec<Value> = Vec::new();
        let mut map: HashMap<i64, CardService> = HashMap::new();
        for (cat, access) in [
            ("observe", vec!["read"]),
            ("control", vec!["read", "write"]),
            ("meta", vec!["read"]),
            ("notify_only", vec!["notify"]),
        ] {
            let acc: Vec<String> = access.iter().map(|s| s.to_string()).collect();
            for p in stats.get(cat).and_then(|x| x.as_array()).unwrap_or(&empty) {
                let full = p.get("name").and_then(|x| x.as_str()).unwrap_or("");
                let (svc_name, prop_name) = match full.split_once('.') {
                    Some((a, b)) => (a.to_string(), b.to_string()),
                    None => (full.to_string(), full.to_string()),
                };
                let siid = p.get("siid").and_then(|x| x.as_i64()).unwrap_or(0);
                let piid = p.get("piid").and_then(|x| x.as_i64()).unwrap_or(0);
                let prop = CardProp {
                    iid: piid,
                    name: prop_name,
                    desc: p.get("desc").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                    comment: String::new(),
                    format: p.get("format").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                    unit: p.get("unit").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                    access: acc.clone(),
                    value_list: parse_classify_list(p.get("list")),
                    range: None,
                    is_action: false,
                    has_in: false,
                };
                map.entry(siid)
                    .or_insert_with(|| CardService { iid: siid, name: svc_name, props: Vec::new() })
                    .props
                    .push(prop);
            }
        }
        let mut services: Vec<CardService> = map.into_values().collect();
        services.sort_by_key(|s| s.iid);
        CardSpec { services }
    }

    /// `prop_key` → (siid, piid)。逗号分隔复合键取首个可解析段。
    /// `pi:s:p` 直寻址；`p/a/e:svc:prop` 经服务名+属性名查 iid。
    pub fn resolve(&self, prop_key: &str) -> Option<(i64, i64)> {
        for seg in prop_key.split(',') {
            let seg = seg.trim();
            let parts: Vec<&str> = seg.split(':').collect();
            match parts.first().copied().unwrap_or("") {
                "pi" if parts.len() >= 3 => {
                    if let (Ok(s), Ok(p)) = (parts[1].parse::<i64>(), parts[2].parse::<i64>()) {
                        return Some((s, p));
                    }
                }
                "p" | "a" | "e" if parts.len() >= 3 => {
                    if let Some(r) = self.resolve_named(parts[1], parts[2], false) {
                        return Some(r);
                    }
                }
                _ => {}
            }
        }
        None
    }

    /// `prop_key` 中首个 **`a:svc:action` 段** → (服务 iid, 动作 iid)。
    /// 二级动作按钮用（下发走 action 通道，不走 setProperty）。
    pub fn resolve_action(&self, prop_key: &str) -> Option<(i64, i64)> {
        for seg in prop_key.split(',') {
            let seg = seg.trim();
            let parts: Vec<&str> = seg.split(':').collect();
            if parts.first().copied() == Some("a") && parts.len() >= 3 {
                if let Some(r) = self.resolve_named(parts[1], parts[2], true) {
                    return Some(r);
                }
            }
        }
        None
    }

    /// 按服务名/属性名查 iid。`want_action=true` 只匹配动作条目，
    /// false 只匹配属性条目（动作也存进 props，靠 is_action 区分，防 iid 撞号）。
    fn resolve_named(&self, svc: &str, prop: &str, want_action: bool) -> Option<(i64, i64)> {
        // 1) 精确服务名（air-conditioner / fan / router / camera-control ...）
        for s in &self.services {
            if s.name == svc {
                for p in &s.props {
                    if p.name == prop && p.is_action == want_action {
                        return Some((s.iid, p.iid));
                    }
                }
            }
        }
        // 2) 服务名后缀/前缀匹配：模板常用逻辑名（light），实例名带前缀
        //    （indicator-light / night-light）。仅在精确失败时兜底，避免误命中。
        let suffix = format!("-{svc}");
        let prefix = format!("{svc}-");
        for s in &self.services {
            if s.name.ends_with(&suffix) || s.name.starts_with(&prefix) {
                for p in &s.props {
                    if p.name == prop && p.is_action == want_action {
                        return Some((s.iid, p.iid));
                    }
                }
            }
        }
        None
    }

    pub fn prop(&self, siid: i64, piid: i64) -> Option<&CardProp> {
        self.services
            .iter()
            .find(|s| s.iid == siid)
            .and_then(|s| s.props.iter().find(|p| p.iid == piid && !p.is_action))
    }

    /// 动作条目查询（is_action=true）
    pub fn action(&self, siid: i64, iid: i64) -> Option<&CardProp> {
        self.services
            .iter()
            .find(|s| s.iid == siid)
            .and_then(|s| s.props.iter().find(|p| p.iid == iid && p.is_action))
    }

    /// spec 驱动兜底时挑观察属性（跳过 device-information 与 notify-only），
    /// 返回 (siid, prop) 便于直接生成 DTO 地址。
    pub fn observe_props(&self) -> Vec<(i64, &CardProp)> {
        let preferred = [
            "connected-device-number",
            "temperature",
            "relative-humidity",
            "contact-state",
            "occupancy-status",
            "illumination",
            "battery-level",
            "atmospheric-pressure",
            "download-speed",
            "status",
        ];
        let mut out: Vec<(i64, &CardProp)> = Vec::new();
        for s in &self.services {
            if s.name == "device-information" {
                continue;
            }
            for p in &s.props {
                if p.access.iter().any(|a| a == "read") {
                    out.push((s.iid, p));
                }
            }
        }
        out.sort_by_key(|(_, p)| {
            preferred
                .iter()
                .position(|w| *w == p.name)
                .unwrap_or(preferred.len())
        });
        out
    }

    /// spec 驱动兜底时的主控制：首个可写 bool on 属性。
    pub fn main_switch(&self) -> Option<(i64, i64)> {
        for s in &self.services {
            for p in &s.props {
                if p.name == "on" && p.format == "bool" && p.access.iter().any(|a| a == "write") {
                    return Some((s.iid, p.iid));
                }
            }
        }
        None
    }

    /// 二级"全量控制项"：可写、非高级/元数据服务的属性，
    /// 按 format 归类为 bool/枚举/区间/数值（string/object 下发语义不明，排除）。
    /// 排序：主开关(on) > 其它 bool > 枚举 > 区间数值 > 普通数值。
    pub fn control_props(&self) -> Vec<(i64, &CardProp)> {
        let mut out: Vec<(i64, &CardProp)> = Vec::new();
        for s in &self.services {
            if skip_control_service(&s.name) {
                continue;
            }
            for p in &s.props {
                if p.is_action || !p.access.iter().any(|a| a == "write") {
                    continue;
                }
                if !is_numeric_format(&p.format) && p.format != "bool" {
                    continue;
                }
                // 调参/vendor 类属性名排除（阈值/灵敏度/延时/custom 模式等）
                if skip_prop_keyword(&p.name) {
                    continue;
                }
                out.push((s.iid, p));
            }
        }
        out.sort_by_key(|(_, p)| control_priority(p));
        out
    }

    /// 二级动作按钮：spec services[].actions 中**无 in 参数**的动作
    /// （带参动作本期不下发；高级服务同样排除）。
    pub fn action_props(&self) -> Vec<(i64, &CardProp)> {
        let mut out = Vec::new();
        for s in &self.services {
            if skip_control_service(&s.name) {
                continue;
            }
            for p in &s.props {
                if p.is_action && !p.has_in {
                    out.push((s.iid, p));
                }
            }
        }
        out
    }
}

// 控制区排除名单的**唯一事实源在 `crate::safety`**（2026-11-17 决策：安全边界可被
// 纯 CPU 测试拦住）。这里只留薄壳委托，避免渲染层与测试层各拿一份名单、
// 改一处漏一处。
fn skip_control_service(name: &str) -> bool {
    crate::safety::skip_control_service(name)
}

fn skip_prop_keyword(name: &str) -> bool {
    crate::safety::skip_prop_keyword(name)
}

fn is_numeric_format(f: &str) -> bool {
    matches!(
        f,
        "uint8" | "uint16" | "uint32" | "int8" | "int16" | "int32" | "int64" | "float" | "double"
    )
}

fn control_priority(p: &CardProp) -> u8 {
    if p.name == "on" && p.format == "bool" {
        0
    } else if p.format == "bool" {
        1
    } else if !p.value_list.is_empty() {
        2
    } else if p.range.is_some() {
        3
    } else {
        4
    }
}

fn parse_instance_prop(p: &Value) -> CardProp {
    CardProp {
        iid: p.get("iid").and_then(|x| x.as_i64()).unwrap_or(0),
        name: urn_name(p.get("type").and_then(|x| x.as_str()).unwrap_or("")),
        desc: p.get("description").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        comment: p.get("comment").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        format: p.get("format").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        unit: p.get("unit").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        access: p
            .get("access")
            .and_then(|x| x.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default(),
        value_list: parse_instance_list(p.get("value-list")),
        range: p.get("value-range").and_then(|x| x.as_array()).and_then(|a| {
            if a.len() >= 3 {
                Some((num(&a[0]), num(&a[1]), num(&a[2])))
            } else {
                None
            }
        }),
        is_action: false,
        has_in: p
            .get("in")
            .and_then(|x| x.as_array())
            .map(|a| !a.is_empty())
            .unwrap_or(false),
    }
}

fn parse_instance_list(v: Option<&Value>) -> Vec<CardChoice> {
    let empty: Vec<Value> = Vec::new();
    v.and_then(|x| x.as_array())
        .unwrap_or(&empty)
        .iter()
        .map(|it| CardChoice {
            value: it.get("value").cloned().unwrap_or(Value::Null),
            en: it.get("description").and_then(|x| x.as_str()).unwrap_or("").to_string(),
            label: it.get("comment").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        })
        .collect()
}

fn parse_classify_list(v: Option<&Value>) -> Vec<CardChoice> {
    let empty: Vec<Value> = Vec::new();
    v.and_then(|x| x.as_array())
        .unwrap_or(&empty)
        .iter()
        .map(|it| CardChoice {
            value: it.get("value").cloned().unwrap_or(Value::Null),
            en: it.get("description").and_then(|x| x.as_str()).unwrap_or("").to_string(),
            label: it.get("comment").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        })
        .collect()
}

/// URN 的类别/名称段（第 4 段，index 3）：
/// `urn:miot-spec-v2:device:air-conditioner:0000A004:xiaomi-m4:6` → "air-conditioner"。
pub fn urn_name(urn: &str) -> String {
    let parts: Vec<&str> = urn.split(':').collect();
    if parts.len() >= 4 {
        parts[3].to_string()
    } else {
        urn.to_string()
    }
}

/// 二级弹层 DTO 条目上限（防高级/调参属性刷屏）
const DETAIL_MAX_ITEMS: usize = 24;

/// 追加去重：同通道（is_action）+ 同 (siid,piid) 视为重复；超上限丢弃。
fn push_unique(items: &mut Vec<CardItemVm>, vm: CardItemVm) {
    if items.len() >= DETAIL_MAX_ITEMS {
        return;
    }
    let dup = items
        .iter()
        .any(|i| i.is_action == vm.is_action && i.siid == vm.siid && i.piid == vm.piid);
    if !dup {
        items.push(vm);
    }
}

fn num(v: &Value) -> f64 {
    v.as_f64().unwrap_or(0.0)
}

/// 属性值归一化成可比较字符串，兼容 `false`(bool) / "false"(str) / 数字 / 字符串。
pub fn norm_value(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
    .to_lowercase()
}

// ----------------------------------------------------------- CardEngine ----

/// 云端卡片模板引擎（纯逻辑）。
pub struct CardEngine {
    /// 75 条 miotspec 模板（保留原始 Value 便于异构导航）
    templates: Vec<Value>,
    /// card_default_txt.names：key → zh_CN 文案
    names: HashMap<String, String>,
    /// card_default_txt.short_name：枚举短名 → zh_CN（Turbo→超强）
    short_name: HashMap<String, String>,
    /// card_default_txt.units：单位 key → zh_CN
    units: HashMap<String, String>,
    /// 老卡配置的 prop_name 中文表（补充 label）
    legacy_labels: HashMap<String, String>,
}

impl CardEngine {
    /// 吃三份 `getappconfig` **外层 JSON 文本**（result.content 为模板/文案字符串）。
    pub fn from_json(miotspec: &str, default_txt: &str, legacy: &str) -> Result<Self> {
        let tpl_content = outer_content(miotspec)?;
        let txt_content = outer_content(default_txt)?;
        let leg_content = outer_content(legacy).unwrap_or_default();
        Self::from_content(&tpl_content, &txt_content, &leg_content)
    }

    /// 吃三份 `result.content` 内容字符串（单测可直接传）。
    pub fn from_content(tpl: &str, txt: &str, leg: &str) -> Result<Self> {
        let templates: Vec<Value> = serde_json::from_str(tpl)
            .map_err(|e| Error::Protocol(format!("miotspec 模板解析失败: {e}")))?;
        let txt: Value = if txt.trim().is_empty() {
            Value::Null
        } else {
            serde_json::from_str(txt)?
        };
        let names = zh_map(txt.get("names"));
        let short_name = zh_map(txt.get("short_name"));
        let units = zh_unit_map(txt.get("units"));
        let legacy_labels = parse_legacy_labels(leg);
        Ok(CardEngine { templates, names, short_name, units, legacy_labels })
    }

    pub fn template_count(&self) -> usize {
        self.templates.len()
    }

    /// zh_CN 文案（names/short_name）；无则用英文 en 键兜底返回 None。
    fn zh(&self, key: &str) -> Option<String> {
        self.names
            .get(key)
            .or_else(|| self.short_name.get(key))
            .cloned()
            .filter(|s| !s.is_empty())
    }

    /// 首页精简视图：匹配模板 → 归一化；未命中 / 全被过滤 → spec 驱动兜底。
    pub fn items_for(&self, spec: &CardSpec, instance_urn: &str) -> Vec<CardItemVm> {
        let items = self.build_items(spec, instance_urn, false);
        if items.is_empty() {
            return self.fallback(spec);
        }
        items
    }

    /// 二级"全量"视图（本期弹层消费）：
    /// 1. 模板 card_items（与首页同源，但 `cardType:2` 动作项按 `a:svc:action`
    ///    解析成动作地址，并透出 `param_delta` / `value-range`）；
    /// 2. 追加 spec 驱动的全量可写属性（高级/元数据/notify-only 除外）；
    /// 3. 追加 spec 动作（无 in 参数）。
    /// 模板未命中 → [`Self::fallback_full`]（全量可写 + 更多观察值）。
    pub fn detail_items(&self, spec: &CardSpec, instance_urn: &str) -> Vec<CardItemVm> {
        let mut items = self.build_items(spec, instance_urn, true);
        if items.is_empty() {
            return self.fallback_full(spec);
        }
        for (siid, p) in spec.control_props() {
            push_unique(&mut items, self.control_vm(siid, p));
        }
        for (siid, a) in spec.action_props() {
            push_unique(&mut items, self.action_vm(siid, a));
        }
        self.dedup_labels(&mut items, spec);
        items
    }

    /// 控制类重名 label 追加服务名后缀（同一设备多个「开关」→「开关·指示灯」等），
    /// 避免二级弹层同质化，也保证可读性。
    fn dedup_labels(&self, items: &mut [CardItemVm], spec: &CardSpec) {
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for it in items.iter_mut() {
            if !it.kind.is_control() || it.label.is_empty() {
                continue;
            }
            if seen.contains(&it.label) {
                let svc = spec
                    .services
                    .iter()
                    .find(|s| s.iid == it.siid)
                    .map(|s| s.name.clone())
                    .unwrap_or_default();
                let zh = self.zh(&title_case(&svc)).unwrap_or_else(|| svc.clone());
                if !zh.is_empty() {
                    it.label = format!("{}·{}", it.label, zh);
                }
            } else {
                seen.insert(it.label.clone());
            }
        }
    }

    /// 模板匹配 + card_items 归一化（不含兜底）；`detail` 决定动作项寻址方式。
    fn build_items(&self, spec: &CardSpec, instance_urn: &str, detail: bool) -> Vec<CardItemVm> {
        let category = urn_name(instance_urn);
        let empty: Vec<Value> = Vec::new();
        let tpl = self.templates.iter().find(|t| {
            let types = t.get("types").and_then(|x| x.as_array()).unwrap_or(&empty);
            let matches_cat = types.iter().any(|v| v.as_str() == Some(category.as_str()));
            let disabled = t
                .get("disable_type")
                .and_then(|x| x.as_array())
                .map(|a| a.iter().any(|v| v.as_str() == Some(instance_urn)))
                .unwrap_or(false);
            matches_cat && !disabled
        });
        let mut items = Vec::new();
        if let Some(tpl) = tpl {
            for it in tpl.get("card_items").and_then(|x| x.as_array()).unwrap_or(&empty) {
                if let Some(vm) = self.build_item(it, spec, instance_urn, detail) {
                    items.push(vm);
                }
            }
        }
        items
    }

    fn build_item(
        &self,
        it: &Value,
        spec: &CardSpec,
        instance_urn: &str,
        detail: bool,
    ) -> Option<CardItemVm> {
        let card_type = it.get("cardType").and_then(|x| x.as_i64()).unwrap_or(-1);
        let kind = kind_of(card_type);
        if kind == CardKind::Hidden {
            return None; // 101 备用/隐藏项
        }
        let prop_key = it.get("prop_key").and_then(|x| x.as_str()).unwrap_or("");
        // 二级动作项：优先按 `a:服务:动作` 解析成动作地址（下发走 action 通道）；
        // 首页仍按属性寻址（只读预览）。
        let (siid, piid, is_action) = if detail && kind == CardKind::Action {
            match spec.resolve_action(prop_key) {
                Some((s, i)) => (s, i, true),
                None => {
                    let (s, p) = spec.resolve(prop_key)?;
                    (s, p, false)
                }
            }
        } else {
            let (s, p) = spec.resolve(prop_key)?;
            (s, p, false)
        };
        let sp = if is_action {
            spec.action(siid, piid)
        } else {
            spec.prop(siid, piid)
        };

        let name_ignore = it.get("prop_name_ignore").and_then(|x| x.as_bool()).unwrap_or(false);
        let prop_obj = it.get("prop");

        // 文案：中文 comment > names > 英文 description > prop 名
        let prop_name = prop_key.rsplit(':').next().unwrap_or(prop_key).trim().to_string();
        let label = self.label_for(&prop_name, sp, it);
        let (unit, format, ratio) = self.display_for(prop_obj, sp);
        let choices = self.choices_for(it, sp);
        let disabled_when = self.conds_for(it, spec, instance_urn);
        let (min, max, step) = sp
            .and_then(|x| x.range)
            .map(|(a, b, c)| (Some(a), Some(b), Some(c)))
            .unwrap_or((None, None, None));
        let param_delta = it.get("param_delta").and_then(|x| x.as_f64());

        // cardType 18 布尔传感状态「讲人话」特化：官方模板该组件只渲染状态词，
        // 而 names 表把属性名（如 Contact State）整个译成「关闭」进了 label，
        // ArkTS 再拼 bool 值就出现「关闭 开」这种自相矛盾文案。
        // 归一化：label=属性中文语义名、name_ignore=true（状态行只显示结论词）、
        // choices 给出 false/true 两个人话值词。
        // contact-state 语义（2026-11-17 dw2hl 真机实测，用户开门时属性=false）：
        // false=磁体分离=门开，true=吸合=门关；与 BLE MiBeacon 门事件
        // 0=开/1=关 一致。miot-spec 文档字面假设（false=吸合）与实测相反，以实测为准。
        let (label, name_ignore, choices) = if kind == CardKind::SensorStatus
            && sp.map(|x| x.format == "bool").unwrap_or(false)
        {
            let sp_name = sp.map(|x| x.name.as_str()).unwrap_or("");
            let is_contact = sp_name == "contact-state";
            let (label_true, label_false, en_true, en_false) = if is_contact {
                ("门已关", "门已开", "Closed", "Open")
            } else {
                ("已打开", "已关闭", "Open", "Closed")
            };
            let mut ch = vec![
                CardChoice { value: Value::Bool(false), label: label_false.to_string(), en: en_false.to_string() },
                CardChoice { value: Value::Bool(true), label: label_true.to_string(), en: en_true.to_string() },
            ];
            let mut label = label;
            if is_contact {
                label = "门磁状态".to_string();
            }
            // 值词若已有中文（罕见：spec value-list 带 bool），以引擎人话为准覆盖
            ch.iter_mut().for_each(|c| {
                if c.label.is_empty() {
                    c.label = c.en.clone();
                }
            });
            (label, true, ch)
        } else {
            (label, name_ignore, choices)
        };

        Some(CardItemVm {
            kind,
            siid,
            piid,
            label,
            unit,
            format,
            ratio,
            choices,
            disabled_when,
            enabled: true,
            value: None,
            code: 0,
            name_ignore,
            min,
            max,
            step,
            param_delta,
            is_action,
            src: None,
        })
    }

    fn label_for(&self, prop_name: &str, sp: Option<&CardProp>, it: &Value) -> String {
        // 模板显式 prop_desc / service_desc（英文）→ names 翻译
        for field in ["prop_desc", "service_desc"] {
            if let Some(arr) = it.get(field).and_then(|x| x.as_array()) {
                for v in arr {
                    if let Some(en) = v.as_str() {
                        if let Some(zh) = self.zh(en) {
                            return zh;
                        }
                    }
                }
            }
        }
        // spec 中文 comment（instance 带）
        if let Some(sp) = sp {
            if !sp.comment.is_empty() {
                return sp.comment.clone();
            }
            if let Some(zh) = self.zh(&title_case(prop_name)).or_else(|| self.zh(&sp.desc)) {
                return zh;
            }
            if !sp.desc.is_empty() {
                return sp.desc.clone();
            }
        }
        if let Some(zh) = self.zh(&title_case(prop_name)) {
            return zh;
        }
        self.legacy_labels
            .get(prop_name)
            .cloned()
            .unwrap_or_else(|| prop_name.to_string())
    }

    fn display_for(&self, prop_obj: Option<&Value>, sp: Option<&CardProp>) -> (String, String, f64) {
        let fmt = prop_obj
            .and_then(|p| p.get("format"))
            .and_then(|x| x.as_str())
            .map(String::from)
            .or_else(|| sp.map(|s| s.format.clone()))
            .unwrap_or_default();
        let unit = prop_obj
            .and_then(|p| p.get("prop_unit"))
            .and_then(|x| x.as_str())
            .map(String::from)
            .or_else(|| sp.map(|s| s.unit.clone()).filter(|s| !s.is_empty()))
            .unwrap_or_default();
        // units 表按 key 翻译（如 seconds→秒）；prop_unit 常已是中文/符号，直接用
        let unit = self.units.get(&unit).cloned().unwrap_or(unit);
        let ratio = prop_obj
            .and_then(|p| p.get("ratio"))
            .and_then(|x| x.as_f64())
            .unwrap_or(1.0);
        (unit, fmt, ratio)
    }

    fn choices_for(&self, it: &Value, sp: Option<&CardProp>) -> Vec<CardChoice> {
        let mut base: Vec<CardChoice> = sp.map(|s| s.value_list.clone()).unwrap_or_default();
        // 中文文案补全：comment > names[en].zh_CN > en
        for c in &mut base {
            if c.label.is_empty() {
                c.label = self.zh(&c.en).unwrap_or_else(|| c.en.clone());
            }
        }
        // desc_sort：按英文词顺序重排（模板给的顺序优先），未匹配的追加
        if let Some(sort) = it.get("desc_sort").and_then(|x| x.as_array()) {
            let order: Vec<String> = sort.iter().filter_map(|v| v.as_str().map(String::from)).collect();
            if !order.is_empty() && !base.is_empty() {
                let mut sorted: Vec<CardChoice> = Vec::new();
                let mut used = vec![false; base.len()];
                for w in &order {
                    for (i, c) in base.iter().enumerate() {
                        if !used[i] && c.en.eq_ignore_ascii_case(w) {
                            used[i] = true;
                            sorted.push(c.clone());
                            break;
                        }
                    }
                }
                for (i, c) in base.iter().enumerate() {
                    if !used[i] {
                        sorted.push(c.clone());
                    }
                }
                return sorted;
            }
        }
        base
    }

    /// `operation.disable_status` → 归一化条件。
    /// 带 `disable_type`/`enable_type` URN 列表的条件按「仅列出的型号适用」处理，
    /// 当前设备 URN 不在列表则丢弃（默认型号走无条件规则）。
    fn conds_for(&self, it: &Value, spec: &CardSpec, instance_urn: &str) -> Vec<CardCond> {
        let mut out = Vec::new();
        let empty: Vec<Value> = Vec::new();
        for op in it.get("operation").and_then(|x| x.as_array()).unwrap_or(&empty) {
            for ds in op.get("disable_status").and_then(|x| x.as_array()).unwrap_or(&empty) {
                let in_scope = |field: &str| -> Option<bool> {
                    ds.get(field).and_then(|x| x.as_array()).map(|a| {
                        a.iter().any(|v| v.as_str() == Some(instance_urn))
                    })
                };
                let scoped = in_scope("enable_type").or_else(|| in_scope("disable_type"));
                if scoped == Some(false) {
                    continue;
                }
                let key = ds.get("key").and_then(|x| x.as_str()).unwrap_or("");
                let (siid, piid) = match spec.resolve(key) {
                    Some(r) => r,
                    None => continue,
                };
                let expect = norm_value(ds.get("value").unwrap_or(&Value::Null));
                out.push(CardCond { siid, piid, expect });
            }
        }
        out
    }

    /// 模板未命中时 spec 驱动兜底（card_design.md 第四节 L1）：
    /// 1 个主控制 + 若干观察值；全控制类仍只读渲染。
    pub fn fallback(&self, spec: &CardSpec) -> Vec<CardItemVm> {
        let mut items = Vec::new();
        if let Some((siid, piid)) = spec.main_switch() {
            items.push(CardItemVm {
                kind: CardKind::Toggle,
                siid,
                piid,
                label: spec
                    .prop(siid, piid)
                    .map(|p| p.comment.clone())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| self.zh("Power").unwrap_or_else(|| "开关".to_string())),
                unit: String::new(),
                format: "bool".to_string(),
                ratio: 1.0,
                choices: Vec::new(),
                disabled_when: Vec::new(),
                enabled: true,
                value: None,
                code: 0,
                name_ignore: false,
                min: None,
                max: None,
                step: None,
                param_delta: None,
                is_action: false,
                src: None,
            });
        }
        for (siid, p) in spec.observe_props().into_iter().take(3) {
            items.push(self.observe_vm(siid, p));
        }
        if items.is_empty() {
            items.push(CardItemVm {
                kind: CardKind::StatusText,
                siid: 0,
                piid: 0,
                label: "设备".to_string(),
                unit: String::new(),
                format: String::new(),
                ratio: 1.0,
                choices: Vec::new(),
                disabled_when: Vec::new(),
                enabled: true,
                value: None,
                code: 0,
                name_ignore: false,
                min: None,
                max: None,
                step: None,
                param_delta: None,
                is_action: false,
                src: None,
            });
        }
        items
    }

    /// 二级兜底（模板未命中）：全量可写控制项 + 更多观察值（card_design.md §4 L2）。
    pub fn fallback_full(&self, spec: &CardSpec) -> Vec<CardItemVm> {
        let mut items: Vec<CardItemVm> = Vec::new();
        for (siid, p) in spec.control_props() {
            push_unique(&mut items, self.control_vm(siid, p));
        }
        for (siid, p) in spec.observe_props().into_iter().take(6) {
            push_unique(&mut items, self.observe_vm(siid, p));
        }
        for (siid, a) in spec.action_props() {
            push_unique(&mut items, self.action_vm(siid, a));
        }
        if items.is_empty() {
            return self.fallback(spec);
        }
        self.dedup_labels(&mut items, spec);
        items
    }

    /// 观察/状态项 VM（fallback 与 fallback_full 共用）
    fn observe_vm(&self, siid: i64, p: &CardProp) -> CardItemVm {
        let numeric = is_numeric_format(&p.format);
        let label = if !p.comment.is_empty() {
            p.comment.clone()
        } else {
            self.zh(&title_case(&p.name)).unwrap_or_else(|| p.desc.clone())
        };
        let mut choices = p.value_list.clone();
        for c in &mut choices {
            if c.label.is_empty() {
                c.label = self.zh(&c.en).unwrap_or_else(|| c.en.clone());
            }
        }
        CardItemVm {
            kind: if numeric { CardKind::ObserveNumber } else { CardKind::StatusText },
            siid,
            piid: p.iid,
            label,
            unit: self.units.get(&p.unit).cloned().unwrap_or_else(|| p.unit.clone()),
            format: p.format.clone(),
            ratio: 1.0,
            choices,
            disabled_when: Vec::new(),
            enabled: true,
            value: None,
            code: 0,
            name_ignore: false,
            min: None,
            max: None,
            step: None,
            param_delta: None,
            is_action: false,
            src: None,
        }
    }

    /// spec 驱动控制项 VM：bool→Toggle，value-list→EnumChoice，
    /// value-range→SeekBar，其余数值→Stepper。
    fn control_vm(&self, siid: i64, p: &CardProp) -> CardItemVm {
        let kind = if p.format == "bool" {
            CardKind::Toggle
        } else if !p.value_list.is_empty() {
            CardKind::EnumChoice
        } else if p.range.is_some() {
            CardKind::SeekBar
        } else {
            CardKind::Stepper
        };
        let label = if !p.comment.is_empty() {
            p.comment.clone()
        } else if let Some(zh) = self.zh(&title_case(&p.name)) {
            zh
        } else if !p.desc.is_empty() {
            p.desc.clone()
        } else {
            p.name.clone()
        };
        let (min, max, step) = p
            .range
            .map(|(a, b, c)| (Some(a), Some(b), Some(c)))
            .unwrap_or((None, None, None));
        let mut choices = p.value_list.clone();
        for c in &mut choices {
            if c.label.is_empty() {
                c.label = self.zh(&c.en).unwrap_or_else(|| c.en.clone());
            }
        }
        CardItemVm {
            kind,
            siid,
            piid: p.iid,
            label,
            unit: self.units.get(&p.unit).cloned().unwrap_or_else(|| p.unit.clone()),
            format: p.format.clone(),
            ratio: 1.0,
            choices,
            disabled_when: Vec::new(),
            enabled: true,
            value: None,
            code: 0,
            name_ignore: false,
            min,
            max,
            step,
            param_delta: None,
            is_action: false,
            src: None,
        }
    }

    /// spec 动作按钮 VM（无 in 参数；is_action=true，siid/piid 为服务/动作 iid）
    fn action_vm(&self, siid: i64, a: &CardProp) -> CardItemVm {
        let label = if !a.comment.is_empty() {
            a.comment.clone()
        } else if !a.desc.is_empty() {
            a.desc.clone()
        } else {
            a.name.clone()
        };
        CardItemVm {
            kind: CardKind::Action,
            siid,
            piid: a.iid,
            label,
            unit: String::new(),
            format: String::new(),
            ratio: 1.0,
            choices: Vec::new(),
            disabled_when: Vec::new(),
            enabled: true,
            value: None,
            code: 0,
            name_ignore: false,
            min: None,
            max: None,
            step: None,
            param_delta: None,
            is_action: true,
            src: None,
        }
    }

    /// 按当前属性值回填 value + 求 enabled：任一 `disabled_when` 命中即禁用。
    pub fn evaluate(&self, items: &mut [CardItemVm], props: &[PropValue]) {
        let mut cur: HashMap<(i64, i64), (Value, i64)> = HashMap::new();
        for p in props {
            cur.insert((p.siid, p.piid), (p.value.clone(), p.code));
        }
        for it in items.iter_mut() {
            if let Some((v, code)) = cur.get(&(it.siid, it.piid)) {
                it.value = Some(v.clone());
                it.code = *code;
            }
            let disabled = it.disabled_when.iter().any(|c| {
                cur.get(&(c.siid, c.piid))
                    .map(|(v, _)| norm_value(v) == c.expect)
                    .unwrap_or(false)
            });
            it.enabled = !disabled;
        }
    }
}

// -------------------------------------------------------------- helpers ----

/// getappconfig 外层 → result.content 字符串
fn outer_content(raw: &str) -> Result<String> {
    let v: Value = serde_json::from_str(raw)?;
    Ok(v.pointer("/result/content")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string())
}

/// names/short_name：{key: {zh_CN|zh|...}} → {key: zh_CN}
fn zh_map(v: Option<&Value>) -> HashMap<String, String> {
    let mut out = HashMap::new();
    if let Some(obj) = v.and_then(|x| x.as_object()) {
        for (k, langs) in obj {
            if let Some(m) = langs.as_object() {
                let zh = m
                    .get("zh_CN")
                    .or_else(|| m.get("zh"))
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                if !zh.is_empty() {
                    out.insert(k.clone(), zh);
                }
            }
        }
    }
    out
}

/// units：{key: {name: {多语言}}} → {key: zh_CN}。
/// 无中文时回退英文符号（percentage→% / μg/m³ / celsius→°C 等），
/// 否则 spec 的 unit="percentage" 会原样显示在卡片上。
fn zh_unit_map(v: Option<&Value>) -> HashMap<String, String> {
    let mut out = HashMap::new();
    if let Some(obj) = v.and_then(|x| x.as_object()) {
        for (k, val) in obj {
            let name = val.get("name").unwrap_or(val);
            let zh = name
                .get("zh_CN")
                .or_else(|| name.get("zh"))
                .or_else(|| name.get("en"))
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            if !zh.is_empty() {
                out.insert(k.clone(), zh);
            }
        }
    }
    out
}

/// 老卡配置：props[].prop_name.{zh_CN} → 以 prop_key 末段作补充 label 表
fn parse_legacy_labels(leg: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    if leg.trim().is_empty() {
        return out;
    }
    let v: Value = match serde_json::from_str(leg) {
        Ok(v) => v,
        Err(_) => return out,
    };
    let empty: Vec<Value> = Vec::new();
    let arr = v.as_array().unwrap_or(&empty);
    for block in arr {
        for p in block.get("props").and_then(|x| x.as_array()).unwrap_or(&empty) {
            let key = p
                .get("prop_key")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .rsplit('.')
                .next()
                .unwrap_or("")
                .to_string();
            if let Some(zh) = p
                .get("prop_name")
                .and_then(|n| n.get("zh_CN"))
                .and_then(|x| x.as_str())
            {
                if !zh.is_empty() && !key.is_empty() {
                    out.insert(key, zh.to_string());
                }
            }
        }
    }
    out
}

/// "target-temperature" → "Target Temperature"，"on" → "On"（names 用 Title Case 键）
fn title_case(s: &str) -> String {
    s.split('-')
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urn_name_category() {
        assert_eq!(
            urn_name("urn:miot-spec-v2:device:air-conditioner:0000A004:xiaomi-m4:6"),
            "air-conditioner"
        );
        assert_eq!(urn_name("fan"), "fan");
    }

    #[test]
    fn card_type_mapping() {
        assert_eq!(kind_of(1), CardKind::Toggle);
        assert_eq!(kind_of(7), CardKind::ObserveNumber);
        assert_eq!(kind_of(18), CardKind::SensorStatus);
        assert_eq!(kind_of(101), CardKind::Hidden);
        assert!(CardKind::Toggle.is_control());
        assert!(!CardKind::ObserveNumber.is_control());
    }

    #[test]
    fn norm_value_mixed_types() {
        assert_eq!(norm_value(&serde_json::json!(false)), "false");
        assert_eq!(norm_value(&serde_json::json!("false")), "false");
        assert_eq!(norm_value(&serde_json::json!(true)), "true");
        assert_eq!(norm_value(&serde_json::json!("Natural Wind")), "natural wind");
        assert_eq!(norm_value(&serde_json::json!(2)), "2");
    }

    #[test]
    fn resolve_prop_key() {
        let spec = CardSpec {
            services: vec![CardService {
                iid: 2,
                name: "air-conditioner".to_string(),
                props: vec![
                    CardProp {
                        iid: 1,
                        name: "on".to_string(),
                        desc: String::new(),
                        comment: String::new(),
                        format: "bool".to_string(),
                        unit: String::new(),
                        access: vec!["write".to_string()],
                        value_list: vec![],
                        range: None,
                        is_action: false,
                        has_in: false,
                    },
                    CardProp {
                        iid: 2,
                        name: "mode".to_string(),
                        desc: String::new(),
                        comment: String::new(),
                        format: "uint8".to_string(),
                        unit: String::new(),
                        access: vec![],
                        value_list: vec![],
                        range: None,
                        is_action: false,
                        has_in: false,
                    },
                ],
            }],
        };
        assert_eq!(spec.resolve("p:air-conditioner:on"), Some((2, 1)));
        assert_eq!(spec.resolve("p:air-conditioner:mode"), Some((2, 2)));
        assert_eq!(spec.resolve("pi:3:7"), Some((3, 7)));
        // 复合键：跳过不可解析段，取首个可解析段
        assert_eq!(spec.resolve("p:air-conditioner:uv,p:air-conditioner:on"), Some((2, 1)));
        assert_eq!(spec.resolve("p:nope:x"), None);
    }
}
