//! 卡片安全边界（2026-11-17 决策：从 ArkTS `Index.ets` 下沉到 Rust）。
//!
//! 为什么必须在这一层：一键圆钮的放行判定原先活在 ArkTS 里，而 ArkTS 用例要么跑在
//! 设备/模拟器上、要么根本没测（`app/entry/src/ohosTest` 至今不存在）。开放外部 PR 后，
//! 「贡献者改坏安全边界」这件事必须有纯 CPU 就能红的测试兜住，所以判定逻辑与它的
//! 排除名单都搬到这里，profile 里的 `expect.quick_switch` / `expect.forbidden`
//! 才有落地对象（设计文档：`_work/卡片上线标准设计.md`）。
//!
//! 口径沿用上一轮真机验收过的行为，**没有放宽**：宁缺勿滥。

use crate::card::{CardItemVm, CardKind, CardSpec};

/// 基础设施类设备：它们模板里的可写项是网络配置/布防，误触代价远高于便利。
/// （SESSION_STATE 明确「路由器可写属性全是网络配置，危险勿碰」）
const INFRA_KEYWORDS: [&str; 3] = ["router", "gateway", "micloud"];

/// 无 spec 时按中文 label 兜底放行的词。
const POWER_WORDS: [&str; 2] = ["开关", "电源"];

/// 无 spec 时即使含电源词也必须拦掉的危险词（布防/网络/童锁/指示灯…）。
const DANGER_WORDS: [&str; 7] = ["布防", "撤防", "访客", "网络", "wifi", "童锁", "指示"];

/// 圆钮放行结论 + 可解释理由（理由要能进 DTO，出问题时用户和 PR reviewer 都能看懂）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchReason {
    /// 模板主 toggle 与 spec 规范 `on` 属性同址：米家语义的电源开关
    AllowedSpecOn,
    /// spec 缺失，按中文 label 兜底命中电源词
    AllowedLabelFallback,
    /// 基础设施设备黑名单，一律不给
    BlockedInfra,
    /// 模板主 toggle 与 spec 规范 `on` 不同址（指示灯/子开关/非电源 toggle）
    BlockedSpecMismatch,
    /// label 命中危险词
    BlockedDangerLabel,
    /// label 里没有电源词，语义不明
    BlockedNoPowerWord,
    /// 模板里根本没有 toggle 项
    BlockedNoToggle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwitchDecision {
    pub allowed: bool,
    pub reason: SwitchReason,
}

impl SwitchReason {
    /// 稳定的机器可读码：进 DTO、profile 断言、日志都用它。
    /// 刻意不用 Debug 名（那是中文标识符，改名就会破坏断言）。
    pub fn code(self) -> &'static str {
        match self {
            SwitchReason::AllowedSpecOn => "allowed_spec_on",
            SwitchReason::AllowedLabelFallback => "allowed_label_fallback",
            SwitchReason::BlockedInfra => "blocked_infra",
            SwitchReason::BlockedSpecMismatch => "blocked_spec_mismatch",
            SwitchReason::BlockedDangerLabel => "blocked_danger_label",
            SwitchReason::BlockedNoPowerWord => "blocked_no_power_word",
            SwitchReason::BlockedNoToggle => "blocked_no_toggle",
        }
    }

    /// 给用户看的一句话理由（放行时也说清依据，方便"为什么有/没有圆钮"可解释）。
    pub fn hint(self) -> &'static str {
        match self {
            SwitchReason::AllowedSpecOn => "模板主开关就是设备的电源属性，可一键",
            SwitchReason::AllowedLabelFallback => "无规范电源属性，按开关语义放行",
            SwitchReason::BlockedInfra => "路由器/网关等基础设施设备，不提供一键开关",
            SwitchReason::BlockedSpecMismatch => "模板主开关不是该设备的电源属性，只进二级",
            SwitchReason::BlockedDangerLabel => "该项语义属布防/网络/童锁等危险操作，不提供一键",
            SwitchReason::BlockedNoPowerWord => "无法确认该项是电源开关，只进二级",
            SwitchReason::BlockedNoToggle => "该设备模板里没有开关项",
        }
    }
}

impl SwitchDecision {
    fn allow(reason: SwitchReason) -> Self {
        Self { allowed: true, reason }
    }
    fn deny(reason: SwitchReason) -> Self {
        Self { allowed: false, reason }
    }
}

/// 是否基础设施设备（路由器/网关/micloud 云账号虚拟设备）。
/// `model` 与 `spec_type` 都查：网关的 model 是 `lumi.gateway.mgl03`，
/// 而有些设备的 URN 里才带 router。
pub fn is_infra(model: &str, spec_type: &str) -> bool {
    let id = format!("{model} {spec_type}").to_lowercase();
    INFRA_KEYWORDS.iter().any(|k| id.contains(k))
}

/// spec 规范 `on` 属性地址：先要 `bool` + 可写，退一步只要求可写。
/// 与 ArkTS `SpecKit.findOnProp` 同口径（那段代码本轮保留，等首页改读 DTO 后再删）。
pub fn spec_on_prop(spec: &CardSpec) -> Option<(i64, i64)> {
    for svc in &spec.services {
        for p in &svc.props {
            if p.name == "on" && p.format == "bool" && p.access.iter().any(|a| a == "write") {
                return Some((svc.iid, p.iid));
            }
        }
    }
    for svc in &spec.services {
        for p in &svc.props {
            if p.name == "on" && p.access.iter().any(|a| a == "write") {
                return Some((svc.iid, p.iid));
            }
        }
    }
    None
}

/// 模板主 toggle：第一个 `Toggle` 项（与 ArkTS 取法一致）。
pub fn main_toggle(items: &[CardItemVm]) -> Option<&CardItemVm> {
    items.iter().find(|i| i.kind == CardKind::Toggle)
}

/// 判定所需的最小主开关信息。
///
/// 单独定义是为了让 napi 层**不必为了问一句"能不能给圆钮"而构造整个 `CardItemVm`**
/// （那玩意儿有 18 个字段、9 个构造点）。渲染层拿到的模板主 toggle 只需把
/// `(siid, piid, label)` 交进来。
#[derive(Debug, Clone, Copy)]
pub struct ToggleRef<'a> {
    pub siid: i64,
    pub piid: i64,
    pub label: &'a str,
}

/// 一键快控放行判定。三重口径（顺序即优先级）：
/// ① 基础设施黑名单直接拒；
/// ② 能解析到 spec 规范 `on` 属性时，必须模板主 toggle 与它同址；
/// ③ 只有 spec 缺失（没有 `on` 属性）才按中文 label 兜底。
///
/// 注意：`enabled`（联动灰显）**不参与**本判定——地址语义是电源就该给圆钮，
/// 灰显与点击反馈属渲染层与 CardEngine 联动求值的职责。
pub fn quick_switch_ref(
    model: &str,
    spec_type: &str,
    spec: &CardSpec,
    toggle: Option<ToggleRef<'_>>,
) -> SwitchDecision {
    if is_infra(model, spec_type) {
        return SwitchDecision::deny(SwitchReason::BlockedInfra);
    }
    let Some(tg) = toggle else {
        return SwitchDecision::deny(SwitchReason::BlockedNoToggle);
    };
    if let Some((oi, op)) = spec_on_prop(spec) {
        return if tg.siid == oi && tg.piid == op {
            SwitchDecision::allow(SwitchReason::AllowedSpecOn)
        } else {
            SwitchDecision::deny(SwitchReason::BlockedSpecMismatch)
        };
    }
    let lower = tg.label.to_lowercase();
    if DANGER_WORDS.iter().any(|k| lower.contains(k)) {
        return SwitchDecision::deny(SwitchReason::BlockedDangerLabel);
    }
    if POWER_WORDS.iter().any(|k| tg.label.contains(k)) {
        return SwitchDecision::allow(SwitchReason::AllowedLabelFallback);
    }
    SwitchDecision::deny(SwitchReason::BlockedNoPowerWord)
}

/// 卡片 DTO 版便捷入口：自己挑模板主 toggle。
pub fn quick_switch(
    model: &str,
    spec_type: &str,
    spec: &CardSpec,
    items: &[CardItemVm],
) -> SwitchDecision {
    quick_switch_ref(
        model,
        spec_type,
        spec,
        main_toggle(items).map(|t| ToggleRef {
            siid: t.siid,
            piid: t.piid,
            label: &t.label,
        }),
    )
}

// -------------------------------------------------- 控制区排除名单（搬自 card.rs）----

/// 高级/调参/元数据类服务：二级不展示其可写属性与动作。
/// 名单从 `card.rs` 移到这里，成为**唯一事实源**：`card.rs` 渲染时用它，
/// profile 测试也直接断言它（`expect.forbidden` 不得出现在任何控制项）。
pub fn skip_control_service(name: &str) -> bool {
    if name == "device-information" {
        return true;
    }
    const SKIP: [&str; 11] = [
        "dev-mode",
        "system-parm",
        "machine-state",
        "flag-bit",
        "favorite-type-data",
        "mosquito-repellent",
        "enhance",
        "single-smart-scene",
        "maintenance",
        "properties",
        // 网关布防服务：22 项调参（delay/volume/duration/customize-mode…）
        // 属设计文档 §4 高级折叠区，二级不展开
        "arming",
    ];
    SKIP.iter().any(|k| name.contains(k))
}

/// 二级不展示的属性名关键词（设计文档 §4：调参类默认折叠/不消费）。
pub fn skip_prop_keyword(name: &str) -> bool {
    const SKIP: [&str; 9] = [
        "custom",
        "threshold",
        "sensitivity",
        "delay",
        "parm",
        "config",
        "factory",
        "duration",
        "volume",
    ];
    SKIP.iter().any(|k| name.contains(k))
}

// -------------------------------------------------------------- 文案 token 过滤 ----

/// 云端枚举缺中文文案时，spec 原文（`basic_arming` / `idle` 等）会被当状态吐给用户，
/// 卡片上就是乱码。搬自 ArkTS `looksLikeSpecToken`，判定完全一致：
/// 以字母开头、其后全是拉丁/数字/`_`/`-`/`.` 且总长 ≥2 → 视为协议 token。
/// 「26.7℃」「15%」「制冷」「门已开」「RGB指示灯。设置为0…」都不命中。
pub fn looks_like_spec_token(text: &str) -> bool {
    let t = text.trim();
    let mut it = t.chars();
    let Some(first) = it.next() else {
        return false;
    };
    if !first.is_ascii_alphabetic() {
        return false;
    }
    if t.chars().count() < 2 {
        return false;
    }
    it.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

// ---------------------------------------------------------------------- 测试 ----

#[cfg(test)]
mod tests {
    use super::*;
    use crate::card::{CardProp, CardService};

    fn prop(iid: i64, name: &str, format: &str, access: &[&str]) -> CardProp {
        CardProp {
            iid,
            name: name.into(),
            desc: String::new(),
            comment: String::new(),
            format: format.into(),
            unit: String::new(),
            access: access.iter().map(|s| s.to_string()).collect(),
            value_list: vec![],
            range: None,
            is_action: false,
            has_in: false,
        }
    }

    /// 组一个最小 spec：services 里放若干属性
    fn spec_with(svc_iid: i64, svc_name: &str, props: Vec<CardProp>) -> CardSpec {
        let mut s = CardSpec::default();
        s.services.push(CardService {
            iid: svc_iid,
            name: svc_name.into(),
            props,
        });
        s
    }

    fn item(kind: CardKind, siid: i64, piid: i64, label: &str) -> CardItemVm {
        CardItemVm {
            kind,
            siid,
            piid,
            label: label.into(),
            unit: String::new(),
            format: String::new(),
            ratio: 1.0,
            choices: vec![],
            disabled_when: vec![],
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

    #[test]
    fn 基础设施设备一律拒绝() {
        let spec = spec_with(2, "switch", vec![prop(1, "on", "bool", &["read", "write"])]);
        let items = vec![item(CardKind::Toggle, 2, 1, "开关")];
        // 路由器：即使 spec on 同址、label 就是「开关」，也不给
        let d = quick_switch("xiaomi.router.ra80", "", &spec, &items);
        assert_eq!(d, SwitchDecision::deny(SwitchReason::BlockedInfra));
        // URN 里带 router 同样拦住
        assert!(is_infra(
            "",
            "urn:miot-spec-v2:device:router:0000A036:xiaomi-ra80:1"
        ));
        assert!(is_infra("lumi.gateway.mgl03", ""));
        assert!(!is_infra("xiaomi.aircondition.m4", ""));
    }

    #[test]
    fn spec_on_同址才放行() {
        let spec = spec_with(2, "air-conditioner", vec![prop(1, "on", "bool", &["read", "write"])]);
        let ok = quick_switch(
            "xiaomi.aircondition.m4",
            "urn:miot-spec-v2:device:air-conditioner:0000A004:xiaomi-m4:6",
            &spec,
            &[item(CardKind::Toggle, 2, 1, "开关")],
        );
        assert!(ok.allowed, "实际理由 {:?}", ok.reason);
        assert_eq!(ok.reason, SwitchReason::AllowedSpecOn);

        // 模板主 toggle 是指示灯（2,6）而规范 on 是 (2,1) → 拒
        let bad = quick_switch(
            "dmaker.fan.p5c",
            "",
            &spec,
            &[item(CardKind::Toggle, 2, 6, "开关")],
        );
        assert_eq!(bad, SwitchDecision::deny(SwitchReason::BlockedSpecMismatch));
    }

    #[test]
    fn spec_缺失时按_label_兜底且危险词优先拦() {
        // 没有任何 on 属性 → 走 label 兜底
        let spec = spec_with(2, "massager", vec![prop(2, "power", "bool", &["read", "write"])]);
        let allow = quick_switch(
            "yeelink.light.nl1",
            "",
            &spec,
            &[item(CardKind::Toggle, 2, 1, "电源开关")],
        );
        assert!(allow.allowed);
        assert_eq!(allow.reason, SwitchReason::AllowedLabelFallback);

        // 含「开关」但带危险词 → 仍拒（布防开关/网络开关/指示灯开关…）
        for label in ["布防开关", "网络开关", "童锁开关", "指示灯开关", "访客开关", "撤防开关"] {
            let d = quick_switch("foo.bar", "", &spec, &[item(CardKind::Toggle, 2, 1, label)]);
            assert_eq!(
                d,
                SwitchDecision::deny(SwitchReason::BlockedDangerLabel),
                "label={label} 应拦住"
            );
        }
        // 语义不明（label 里既没电源词也没危险词）→ 不放行
        let unclear = quick_switch(
            "foo.bar",
            "",
            &spec,
            &[item(CardKind::Toggle, 2, 1, "运行")],
        );
        assert_eq!(unclear, SwitchDecision::deny(SwitchReason::BlockedNoPowerWord));
    }

    #[test]
    fn 没有_toggle_就没有圆钮() {
        let spec = spec_with(2, "sensor", vec![prop(1, "temperature", "float", &["read"])]);
        let d = quick_switch(
            "xiaomi.sensor_ht.mini",
            "",
            &spec,
            &[item(CardKind::ObserveNumber, 2, 1, "温度")],
        );
        assert_eq!(d, SwitchDecision::deny(SwitchReason::BlockedNoToggle));
    }

    #[test]
    fn 规范_on_允许非_bool_形态但必须可写() {
        // 0-1 型（uint8 + value-list）也可写：兜底第二轮命中
        let spec = spec_with(
            2,
            "switch",
            vec![prop(1, "on", "uint8", &["read", "write"])],
        );
        assert_eq!(spec_on_prop(&spec), Some((2, 1)));
        // 只读 on 不算规范电源属性
        let ro = spec_with(2, "switch", vec![prop(1, "on", "bool", &["read"])]);
        assert_eq!(spec_on_prop(&ro), None);
    }

    #[test]
    fn 控制区排除名单是唯一事实源() {
        for n in [
            "device-information",
            "urn:miot-spec-v2:service:system-parm:xiaomi-1",
            "arming",
            "dev-mode",
        ] {
            assert!(skip_control_service(n), "{n} 应排除");
        }
        assert!(!skip_control_service("air-conditioner"));
        for k in ["customize-mode", "delay", "volume", "threshold"] {
            assert!(skip_prop_keyword(k), "{k} 应排除");
        }
        assert!(!skip_prop_keyword("target-temperature"));
    }

    #[test]
    fn 协议_token_文案过滤不误伤中文与数值() {
        for t in ["basic_arming", "idle", "auto-clean", "Door.Open", "off"] {
            assert!(looks_like_spec_token(t), "{t} 应判为 token");
        }
        for t in [
            "26.7℃", "15%", "制冷", "门已关", "已开启", "3挡", "开",
            "RGB指示灯。设置为0，关蓝灯常亮", "风机档位 3挡", "", "28.4", "_private",
        ] {
            assert!(!looks_like_spec_token(t), "{t} 不该被判为 token");
        }
    }

    #[test]
    fn 联动禁用项不给圆钮_但由上层灰显不在这层管() {
        // 本层只管"是不是电源开关"；enabled（联动灰显）仍由 CardEngine 负责。
        // 这里断言两件事不打架：主 toggle 被联动禁用时，判定结果仍按电源语义给。
        let spec = spec_with(2, "fan", vec![prop(1, "on", "bool", &["read", "write"])]);
        let mut it = item(CardKind::Toggle, 2, 1, "开关");
        it.enabled = false;
        let d = quick_switch("dmaker.fan.p5c", "", &spec, &[it]);
        assert!(d.allowed, "地址语义是电源，灰显交给渲染层");
        assert_eq!(d.reason, SwitchReason::AllowedSpecOn);
    }

    #[test]
    fn 理由码全局唯一且都有人话说明() {
        use SwitchReason::*;
        let all = [
            AllowedSpecOn,
            AllowedLabelFallback,
            BlockedInfra,
            BlockedSpecMismatch,
            BlockedDangerLabel,
            BlockedNoPowerWord,
            BlockedNoToggle,
        ];
        let mut codes: Vec<&str> = all.iter().map(|r| r.code()).collect();
        assert_eq!(codes.len(), 7, "理由必须穷尽（新增变体就要补 code）");
        codes.sort();
        let n = codes.len();
        codes.dedup();
        assert_eq!(codes.len(), n, "code 不得重复: {codes:?}");
        for r in &all {
            assert!(!r.hint().is_empty(), "{r:?} 缺人话说明");
            assert!(r.hint().chars().count() >= 8, "{r:?} 说明太短");
        }
        // 不变量：code 前缀必须与 allowed 布尔一致（上层按前缀分语气，按布尔分行为）
        for r in &all {
            let allowed_variant = matches!(r, AllowedSpecOn | AllowedLabelFallback);
            assert_eq!(
                r.code().starts_with("allowed_"),
                allowed_variant,
                "{r:?} 的 code 前缀与放行语义不一致: {}",
                r.code()
            );
            assert!(!r.hint().contains("TODO"));
        }
    }
}
