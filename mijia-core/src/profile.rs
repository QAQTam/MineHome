//! 自定义卡片的**可共享档案**（profile）：把用户在本机拼出来的显示方案，
//! 变成一份能公开、能被别人复用、且**可校验**的数据。
//!
//! 为什么必须有指纹这一层：档案里存的是 `(siid, piid)` 地址。固件升级或官方
//! spec 改版后，同一个 `(2,6)` 完全可能从「灯开关」变成别的属性（极端情况是
//! 网关布防）。如果只按地址恢复，就会**静默地把用户当年勾的"灯开关"重绑到一个
//! 要命的属性上**。所以档案必须同时记录该地址当时的语义指纹，加载时逐项校验，
//! 对不上就**拒绝渲染那个控件**（而不是"地址还在就继续用"）。
//!
//! 指纹字段刻意只用**公开 spec 语义**（urn 属性名、access、format、unit），
//! 不含 did、不含用户自取设备名——这两类是隐私，不能进共享档案。

use crate::card::{CardItemVm, CardSpec};
use crate::layout::{self, CardLayout};
use serde_json::{json, Value};

pub const PROFILE_VERSION: i64 = 1;

/// 单个地址的语义指纹。
#[derive(Debug, Clone, serde::Serialize)]
pub struct Fingerprint {
    pub key: String,
    /// spec urnName，如 `on` / `target-temperature`（服务名.属性名 的服务部分另存）
    pub service: String,
    pub prop: String,
    /// `read` / `write` 排序后的集合文本，如 `"read,write"`
    pub access: String,
    pub format: String,
    pub unit: String,
}

/// 生成档案。`did`/`name` **不写入**，只写 model 与公开 spec 语义。
///
/// `scrub` 固定为 `"synthetic"`：本模块产出的档案天然不含任何账号数据，
/// 调用方不需要再做二次脱敏，也不允许伪标成 `"verified"`（那个标记留给
/// 未来"从真账号快照生成"的路径，且必须走强制脱敏函数）。
pub fn build(
    model: &str,
    spec_type: &str,
    spec: &CardSpec,
    items: &[CardItemVm],
    layout: &CardLayout,
) -> Value {
    let by_key: Vec<(String, &CardItemVm)> = items.iter().map(|i| (layout::key_of(i), i)).collect();
    let mut fps = Vec::new();
    for key in &layout.visible {
        if key == layout::HIDE_MARK {
            continue;
        }
        if let Some((_k, it)) = by_key.iter().find(|(k, _)| k == key) {
            fps.push(fingerprint_of(it, spec));
        }
    }
    // known 也要留指纹：新项判定靠它，改版后 known 失真会误判"用户没见过"
    let mut known_fps: Vec<Fingerprint> = Vec::new();
    for key in &layout.known {
        if let Some((_k, it)) = by_key.iter().find(|(k, _)| k == key) {
            known_fps.push(fingerprint_of(it, spec));
        }
    }
    json!({
        "version": PROFILE_VERSION,
        "model": model,
        "spec_type": spec_type,
        "scrub": "synthetic",
        "visible": layout.visible,
        "known": layout.known,
        "fingerprints": fps,
        "known_fingerprints": known_fps,
    })
}

/// 取一个 DTO 项在当前 spec 里的语义指纹。
///
/// 权威源一律是 `CardSpec`，不是 DTO：DTO 的 `format` 可能是模板给的显示格式
/// （`"%.1f"`），而 `unit` 可能是本地化后的符号，两者都会随模板文案变化，
/// 拿它们当指纹会造成"模板改了字 → 用户控件被判定改版"的假警报。
fn fingerprint_of(it: &CardItemVm, spec: &CardSpec) -> Fingerprint {
    let service = spec
        .services
        .iter()
        .find(|s| s.iid == it.siid)
        .map(|s| s.name.clone())
        .unwrap_or_default();
    let (prop, access, format, unit) = spec
        .prop(it.siid, it.piid)
        .map(|p| {
            (
                p.name.clone(),
                sorted_join(&p.access.iter().map(|s| s.as_str()).collect::<Vec<&str>>()),
                p.format.clone(),
                p.unit.clone(),
            )
        })
        .unwrap_or_default();
    Fingerprint {
        key: layout::key_of(it),
        service,
        prop,
        access,
        format,
        unit,
    }
}

/// 加载档案：先校验指纹，**不匹配的地址一律剔除**，返回可用于 `layout::apply` 的名单。
/// 返回 (layout, 被剔除的项说明)。剔除项要能在 UI 上说清原因，不能静默。
pub fn restore(profile: &Value, spec: &CardSpec) -> (CardLayout, Vec<String>) {
    let mut out = CardLayout::default();
    let mut dropped: Vec<String> = Vec::new();
    let empty: Vec<Value> = Vec::new();

    let vis = profile.get("visible").and_then(|v| v.as_array()).unwrap_or(&empty);
    let fps = profile.get("fingerprints").and_then(|v| v.as_array()).unwrap_or(&empty);

    for v in vis {
        let Some(key) = v.as_str() else { continue };
        if key == layout::HIDE_MARK {
            out.visible.push(key.to_string());
            continue;
        }
        let Some(fp) = fps.iter().find(|f| f.get("key").and_then(|x| x.as_str()) == Some(key))
        else {
            // 档案里没有这个地址的指纹：无法证明它还是当年那个属性 → 拒绝
            dropped.push(format!("{key} 缺语义指纹，已拒绝恢复"));
            continue;
        };
        match check_fp(fp, spec) {
            Ok(()) => out.visible.push(key.to_string()),
            Err(why) => dropped.push(format!("{key} {why}")),
        }
    }

    let knows = profile.get("known").and_then(|v| v.as_array()).unwrap_or(&empty);
    let knows_fps = profile
        .get("known_fingerprints")
        .and_then(|v| v.as_array())
        .unwrap_or(&empty);
    for v in knows {
        let Some(key) = v.as_str() else { continue };
        let Some(fp) = knows_fps
            .iter()
            .find(|f| f.get("key").and_then(|x| x.as_str()) == Some(key))
        else {
            // 没有指纹也不能当成"用户没见过的新项"——那样会把用户明确取消勾选的
            // 控件悄悄放回首屏。保守处理：仍记为 known（继续隐藏），只报一条警告。
            out.known.push(key.to_string());
            dropped.push(format!("{key} 缺 known 指纹，保持隐藏不回收"));
            continue;
        };
        // 同上：known 条目即使指纹失效也**继续留在 known**（= 继续隐藏）。
        // 让"改版"变成"重新暴露控件"是绝对不可接受的副作用。
        out.known.push(key.to_string());
        if let Err(why) = check_fp(fp, spec) {
            dropped.push(format!("{key} {why}（该项本就未显示，维持隐藏）"));
        }
    }
    (out, dropped)
}

/// 从**档案文本**恢复（ArkTS/napi 走这条：档案对上层是不透明文本，
/// 不让它自己解析 JSON，否则指纹语义会分裂成两处实现）。
///
/// 解析失败时返回空布局 + 一条原因：空布局 = 不启用自定义 = 按模板原样渲染。
/// 这是有意的降级方向——**坏档案最多让卡片退回默认样子，绝不能按地址猜着恢复控件**。
pub fn restore_text(profile: &str, spec: &CardSpec) -> (CardLayout, Vec<String>) {
    match serde_json::from_str::<Value>(profile) {
        Ok(v) => restore(&v, spec),
        Err(e) => (
            CardLayout::default(),
            vec![format!("档案解析失败，已退回默认显示：{e}")],
        ),
    }
}

/// 单条指纹校验：地址必须能在当前 spec 里找到，且 urn 名/access/format 全等。
fn check_fp(fp: &Value, spec: &CardSpec) -> Result<(), String> {
    let siid = fp
        .get("key")
        .and_then(|k| k.as_str())
        .and_then(|s| s.split(':').nth(1))
        .and_then(|s| s.parse::<i64>().ok())
        .ok_or_else(|| "指纹里的 key 无法解析".to_string())?;
    let piid = fp
        .get("key")
        .and_then(|k| k.as_str())
        .and_then(|s| s.split(':').nth(2))
        .and_then(|s| s.parse::<i64>().ok())
        .ok_or_else(|| "指纹里的 key 无法解析".to_string())?;

    let want_service = fp.get("service").and_then(|v| v.as_str()).unwrap_or("");
    let want_prop = fp.get("prop").and_then(|v| v.as_str()).unwrap_or("");
    if want_service.is_empty() || want_prop.is_empty() {
        return Err("指纹缺 urn 属性名，无法证明语义未变".to_string());
    }
    let p = spec
        .prop(siid, piid)
        .ok_or_else(|| format!("当前 spec 已无 ({siid},{piid}) 属性"))?;
    let svc = spec
        .services
        .iter()
        .find(|s| s.iid == siid)
        .ok_or_else(|| format!("当前 spec 已无服务 {siid}"))?;
    if p.name != want_prop || svc.name != want_service {
        return Err(format!(
            "属性已改版：{}:{} → {}:{}",
            want_service, want_prop, svc.name, p.name
        ));
    }
    let got_access: Vec<&str> = p.access.iter().map(|s| s.as_str()).collect();
    let want_access = fp.get("access").and_then(|v| v.as_str()).unwrap_or("");
    let want: Vec<&str> = want_access.split(',').filter(|s| !s.is_empty()).collect();
    if sorted_ref(&got_access) != sorted_ref(&want) {
        return Err(format!(
            "读写权限已变：{} → {}",
            want_access,
            sorted_join(&got_access)
        ));
    }
    let want_format = fp.get("format").and_then(|v| v.as_str()).unwrap_or("");
    if !want_format.is_empty() && !p.format.is_empty() && want_format != p.format {
        return Err(format!("值形态已变：{want_format} → {}", p.format));
    }
    Ok(())
}

fn sorted_ref(v: &[&str]) -> Vec<String> {
    let mut o: Vec<String> = v.iter().map(|s| s.to_string()).collect();
    o.sort();
    o
}

fn sorted_join(v: &[&str]) -> String {
    sorted_ref(v).join(",")
}

// ---------------------------------------------------------------------- 测试 ----

#[cfg(test)]
mod tests {
    use super::*;
    use crate::card::{CardKind, CardProp, CardService};

    fn spec_with(siid: i64, svc: &str, piid: i64, prop: &str, access: &[&str], format: &str) -> CardSpec {
        let mut s = CardSpec::default();
        s.services.push(CardService {
            iid: siid,
            name: svc.to_string(),
            props: vec![CardProp {
                iid: piid,
                name: prop.to_string(),
                desc: String::new(),
                comment: "开关".to_string(),
                format: format.to_string(),
                unit: String::new(),
                access: access.iter().map(|a| a.to_string()).collect(),
                value_list: vec![],
                range: None,
                is_action: false,
                has_in: false,
            }],
        });
        s
    }

    fn profile_of(spec: &CardSpec) -> Value {
        // 用 spec 反推出指纹，模拟"当年生成档案时"的样子
        let svc = &spec.services[0];
        let p = &svc.props[0];
        json!({
            "version": PROFILE_VERSION,
            "model": "xiaomi.switch.w1",
            "spec_type": "urn:miot-spec-v2:device:switch:0000A004:xiaomi-w1:1",
            "scrub": "synthetic",
            "visible": ["toggle:2:1"],
            "known": ["toggle:2:1"],
            "fingerprints": [{
                "key": "toggle:2:1", "service": svc.name, "prop": p.name,
                "access": sorted_join(&p.access.iter().map(|s| s.as_str()).collect::<Vec<&str>>()),
                "format": p.format, "unit": p.unit
            }],
            "known_fingerprints": [{
                "key": "toggle:2:1", "service": svc.name, "prop": p.name,
                "access": sorted_join(&p.access.iter().map(|s| s.as_str()).collect::<Vec<&str>>()),
                "format": p.format, "unit": p.unit
            }]
        })
    }

    #[test]
    fn 指纹全等时正常恢复() {
        let spec = spec_with(2, "switch", 1, "on", &["read", "write"], "bool");
        let (lay, dropped) = restore(&profile_of(&spec), &spec);
        assert_eq!(lay.visible, vec!["toggle:2:1".to_string()]);
        assert_eq!(lay.known, vec!["toggle:2:1".to_string()]);
        assert!(dropped.is_empty(), "不该有剔除: {dropped:?}");
    }

    #[test]
    fn known条目指纹失效也不得被放回首页() {
        // 这是本模块最重要的安全性质：改版不能把用户取消勾选的控件悄悄放回来。
        let made = spec_with(2, "switch", 1, "on", &["read", "write"], "bool");
        let mut p = profile_of(&made);
        // 构造"用户当时见过但没勾"的情形：known 里有、visible 里没有
        p["visible"] = json!([] as [String; 0]);
        p["known"] = json!(["toggle:2:1"]);
        let now = spec_with(2, "switch", 1, "indicator-light", &["read", "write"], "bool");
        let (lay, dropped) = restore(&p, &now);
        assert!(lay.visible.is_empty(), "不该凭空多出显示项");
        assert_eq!(lay.known, vec!["toggle:2:1".to_string()], "指纹失效仍须记为 known");
        // 用这个 layout 去 apply：该项不会被当成新项追加
        let it = CardItemVm {
            kind: CardKind::Toggle,
            siid: 2,
            piid: 1,
            label: "指示灯".into(),
            unit: String::new(),
            format: "bool".into(),
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
        };
        // visible 为空 = 未自定义（layout::apply 会原样返回），所以这里给一个
        // 非空 visible 才能验证 known 的抑制作用：用另一项占位，确保本项不被追加
        let lay2 = CardLayout {
            visible: vec!["other:9:9".to_string()],
            known: lay.known.clone(),
        };
        let (out, appended) = layout::apply(vec![it], Some(&lay2));
        assert!(out.is_empty(), "指纹失效的 known 项被当新项放回: {out:?}");
        assert_eq!(appended, 0);
        assert!(!dropped.is_empty(), "应给出可解释的警告");
    }

    #[test]
    fn 地址还在但属性改名了必须拒绝() {
        let made = spec_with(2, "switch", 1, "on", &["read", "write"], "bool");
        let p = profile_of(&made);
        // 固件改版：(2,1) 现在是别的属性
        let now = spec_with(2, "switch", 1, "indicator-light", &["read", "write"], "bool");
        let (lay, dropped) = restore(&p, &now);
        assert!(lay.visible.is_empty(), "改过名的地址绝不能恢复");
        assert_eq!(dropped.len(), 2, "visible 与 known 各报一次: {dropped:?}");
        assert!(dropped[0].contains("属性已改版"), "{dropped:?}");
    }

    #[test]
    fn 读写权限变了必须拒绝() {
        let made = spec_with(2, "switch", 1, "on", &["read", "write"], "bool");
        let p = profile_of(&made);
        let now = spec_with(2, "switch", 1, "on", &["read"], "bool");
        let (lay, dropped) = restore(&p, &now);
        assert!(lay.visible.is_empty());
        assert!(dropped[0].contains("读写权限已变"), "{dropped:?}");
    }

    #[test]
    fn 地址在当前spec里消失了必须拒绝() {
        let made = spec_with(2, "switch", 1, "on", &["read", "write"], "bool");
        let p = profile_of(&made);
        let now = spec_with(2, "switch", 5, "power", &["read", "write"], "bool");
        let (lay, dropped) = restore(&p, &now);
        assert!(lay.visible.is_empty());
        assert!(dropped[0].contains("已无"), "{dropped:?}");
    }

    #[test]
    fn 缺指纹一律拒绝而不是默认放行() {
        let spec = spec_with(2, "switch", 1, "on", &["read", "write"], "bool");
        let p = json!({ "visible": ["toggle:2:1"], "known": ["toggle:2:1"] });
        let (lay, dropped) = restore(&p, &spec);
        assert!(lay.visible.is_empty());
        assert_eq!(dropped.len(), 2, "{dropped:?}");
    }

    #[test]
    fn 隐藏哨兵不参与指纹校验() {
        let spec = spec_with(2, "switch", 1, "on", &["read", "write"], "bool");
        let p = json!({ "visible": [layout::HIDE_MARK], "known": [] });
        let (lay, dropped) = restore(&p, &spec);
        assert!(lay.is_hidden());
        assert!(dropped.is_empty());
    }

    #[test]
    fn build_产物能被_restore_原样恢复() {
        let spec = spec_with(2, "switch", 1, "on", &["read", "write"], "bool");
        let it = CardItemVm {
            kind: CardKind::Toggle,
            siid: 2,
            piid: 1,
            label: "开关".into(),
            unit: String::new(),
            // DTO 上是模板显示格式，spec 上是 bool：指纹必须取 spec 那份
            format: "%.1f".into(),
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
        };
        let lay = CardLayout {
            visible: vec!["toggle:2:1".to_string()],
            known: vec!["toggle:2:1".to_string()],
        };
        let p = build("xiaomi.switch.w1", "urn:x", &spec, &[it], &lay);
        let (back, dropped) = restore(&p, &spec);
        assert_eq!(back.visible, vec!["toggle:2:1".to_string()]);
        assert!(dropped.is_empty(), "{dropped:?}");
        // 指纹记的是 spec 的 bool，不是 DTO 的 %.1f
        assert_eq!(p["fingerprints"][0]["format"], json!("bool"));
    }

    #[test]
    fn 坏档案退回默认显示而不是按地址猜() {
        let spec = spec_with(2, "switch", 1, "on", &["read", "write"], "bool");
        let (lay, warn) = restore_text("{ 这不是合法 JSON", &spec);
        assert!(lay.visible.is_empty() && lay.known.is_empty());
        assert_eq!(warn.len(), 1, "{warn:?}");
        assert!(warn[0].contains("退回默认显示"), "{warn:?}");
        // 空 layout 交给 apply：全量原样，等于没自定义
        let it = CardItemVm {
            kind: CardKind::Toggle,
            siid: 2,
            piid: 1,
            label: "开关".into(),
            unit: String::new(),
            format: "bool".into(),
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
        };
        let (out, _) = layout::apply(vec![it], Some(&lay));
        assert_eq!(out.len(), 1, "坏档案不该把卡片弄空");
    }

    #[test]
    fn 档案里不得出现账号字段() {
        let spec = spec_with(2, "switch", 1, "on", &["read", "write"], "bool");
        let it = CardItemVm {
            kind: CardKind::Toggle,
            siid: 2,
            piid: 1,
            label: "开关".into(),
            unit: String::new(),
            format: "bool".into(),
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
        };
        let lay = CardLayout {
            visible: vec![layout::key_of(&it)],
            known: vec![layout::key_of(&it)],
        };
        let text = build("xiaomi.switch.w1", "urn:x", &spec, &[it], &lay).to_string();
        for bad in ["did", "userId", "token", "书桌灯", "房间名", "address"] {
            assert!(!text.contains(bad), "档案含敏感字段 {bad}: {text}");
        }
        assert!(text.contains("\"scrub\":\"synthetic\""), "缺脱敏标记: {text}");
    }
}
