//! CardEngine 集成单测（方案 A）。夹具全部在 `mijia-core/fixtures/`（可进公开仓库）：
//! - `templates/card_config_*.json`：云端 getappconfig 三份模板配置（无签名 GET 的公开数据）
//! - `specs_by_model.json`：**按 model 索引**的 spec 分类（由 `tools/make_card_fixtures.py`
//!   从真实账号产物去账号化生成——不含 did、不含用户自取设备名，只留 `from_classify`
//!   真正消费的 `stats.*`）
//! - `specs/spec_aircondition_m4.json`：miot-spec.org instance 真实样本
//!
//! 全部只读、纯 PC，不发任何网络请求、不下发任何控制、不需要任何账号。
//! 别把夹具指回 `../_work`：那目录被 .gitignore 忽略，且里面是真实账号数据。

use mijia_core::card::{CardEngine, CardKind, CardSpec};
use mijia_core::device::PropValue;
use serde_json::{json, Value};
use std::path::PathBuf;

fn fixture(name: &str) -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(name);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("读夹具 {name} 失败: {e}"))
}

fn engine() -> CardEngine {
    CardEngine::from_json(
        &fixture("templates/card_config_card_control_miotspec_config.json"),
        &fixture("templates/card_config_card_default_txt.json"),
        &fixture("templates/card_config_card_control_config.json"),
    )
    .expect("解析三份配置")
}

fn classify() -> Value {
    serde_json::from_str(&fixture("specs_by_model.json")).expect("specs_by_model")
}

/// 按 **model** 取 spec（不再是真实 did）：夹具键就是型号，外部贡献者也能只凭型号跑。
fn spec_of(model: &str) -> CardSpec {
    let root = classify();
    CardSpec::from_classify(
        &root[model],
    )
}

/// 16 台设备 model → 官方模板类别（第 4 段）。无模板类别的走 spec 兜底。
fn category(model: &str) -> &'static str {
    match model {
        "xiaomi.aircondition.m4" | "lumi.acpartner.mcn02" => "air-conditioner",
        "dmaker.fan.p5c" => "fan",
        "isa.camera.hlc7" => "camera",
        "xiaomi.switch.w1" => "switch",
        "cuco.plug.cp1" => "outlet",
        "xiaomi.sensor_ht.mini" => "temperature-humidity-sensor",
        "isa.magnet.dw2hl" => "magnet-sensor",
        "yeelink.light.nl1" => "light",
        "lumi.gateway.mgl03" => "gateway",
        "linp.sensor_occupy.hb01" => "occupancy-sensor",
        "xiaomi.router.ra80" => "router",
        "soocare.magic_touch.m14" => "massager", // 模板 types 无 massager → 兜底
        _ => "unknown-device-type",               // 无线开关/充电器/手环 → 兜底
    }
}

fn pv(siid: i64, piid: i64, value: Value) -> PropValue {
    PropValue {
        did: "test".to_string(),
        siid,
        piid,
        code: 0,
        value,
        update_time: None,
    }
}

fn has_kind(items: &[mijia_core::card::CardItemVm], k: CardKind) -> bool {
    items.iter().any(|i| i.kind == k)
}

// ------------------------------------------------------------------ 基础 ----

#[test]
fn parses_75_templates() {
    assert_eq!(engine().template_count(), 75);
}

#[test]
fn instance_spec_resolves_like_classify() {
    let inst: Value = serde_json::from_str(&fixture("specs/spec_aircondition_m4.json"))
        .expect("instance spec");
    let spec = CardSpec::from_instance(&inst);
    assert_eq!(spec.resolve("p:air-conditioner:on"), Some((2, 1)));
    assert_eq!(spec.resolve("p:air-conditioner:mode"), Some((2, 2)));
    assert_eq!(spec.resolve("p:air-conditioner:target-temperature"), Some((2, 4)));
    assert_eq!(spec.resolve("p:environment:temperature"), Some((4, 7)));
    assert_eq!(spec.resolve("p:fan-control:fan-level"), Some((3, 2)));
    // instance comment 是中文，用作 label
    let on = spec.prop(2, 1).unwrap();
    assert_eq!(on.comment, "开关");
}

// ------------------------------------------------------- 官方模板命中设备 ----

#[test]
fn airconditioner_template_items_and_linkage() {
    let e = engine();
    let spec = spec_of("xiaomi.aircondition.m4");
    let urn = "urn:miot-spec-v2:device:air-conditioner:0000A004:xiaomi-m4:6";
    let mut items = e.items_for(&spec, urn);
    assert!(has_kind(&items, CardKind::Toggle), "应含电源开关");
    assert!(has_kind(&items, CardKind::EnumChoice), "应含 mode 枚举");
    assert!(has_kind(&items, CardKind::Stepper), "应含温度步进");

    let on = items.iter().find(|i| i.kind == CardKind::Toggle).unwrap();
    assert_eq!((on.siid, on.piid), (2, 1));
    let mode = items.iter().find(|i| i.kind == CardKind::EnumChoice).unwrap();
    assert_eq!((mode.siid, mode.piid), (2, 2));
    // mode 的 desc_sort 中文合并：制冷/制热/送风/除湿
    let labels: Vec<&str> = mode.choices.iter().map(|c| c.label.as_str()).collect();
    assert!(labels.iter().any(|l| l.contains("制冷")), "mode 应含制冷: {labels:?}");
    assert!(labels.iter().any(|l| l.contains("制热")), "mode 应含制热: {labels:?}");
    // 关机联动禁用：关机时 mode/温度 enabled=false，开机时 true
    assert!(mode.disabled_when.iter().any(|c| c.expect == "false"), "mode 应有 on=false 条件");
    e.evaluate(&mut items, &[pv(2, 1, json!(false))]);
    let mode_off = items.iter().find(|i| i.kind == CardKind::EnumChoice).unwrap();
    assert!(!mode_off.enabled, "关机时 mode 应联动禁用");
    let temp_off = items.iter().find(|i| i.kind == CardKind::Stepper).unwrap();
    assert!(!temp_off.enabled, "关机时温度步进应联动禁用");

    e.evaluate(&mut items, &[pv(2, 1, json!(true))]);
    let mode_on = items.iter().find(|i| i.kind == CardKind::EnumChoice).unwrap();
    assert!(mode_on.enabled, "开机时 mode 应启用");
    let temp = items.iter().find(|i| i.kind == CardKind::Stepper).unwrap();
    assert_eq!((temp.siid, temp.piid), (2, 4));
    assert_eq!(temp.format, "%.1f");
    assert_eq!(temp.unit, "℃");
}

#[test]
fn fan_template_pi_keys_and_string_false_cond() {
    let e = engine();
    let spec = spec_of("dmaker.fan.p5c");
    let items = e.items_for(&spec, "fan");
    assert!(has_kind(&items, CardKind::Toggle));
    // pi:2:4 / pi:2:2 直寻址
    assert!(items.iter().any(|i| (i.siid, i.piid) == (2, 4)));
    assert!(items.iter().any(|i| (i.siid, i.piid) == (2, 2)));
    // fan 模板 disable_status 用字符串 "false"，归一化后应为 "false"
    let level = items.iter().find(|i| (i.siid, i.piid) == (2, 2)).unwrap();
    assert!(
        level.disabled_when.iter().any(|c| c.expect == "false"),
        "字符串 \"false\" 应被归一化: {:?}",
        level.disabled_when
    );
}

#[test]
fn temperature_humidity_sensor_observe_only() {
    let e = engine();
    let mut spec = spec_of("xiaomi.sensor_ht.mini");
    // 该夹具只收录温/湿度/电量，未收录 atmospheric-pressure：
    // 模板第 3 项 prop_key 无法解析 → 正确跳过（设备不支持该属性）
    let items2 = e.items_for(&spec, "temperature-humidity-sensor");
    assert_eq!(items2.len(), 2, "设备无气压属性时只渲染温湿度 2 项");
    // 注入气压属性后，模板 3 项全部命中，且 ratio 0.001 合并
    let svc = spec.services.iter_mut().find(|s| s.iid == 2).unwrap();
    svc.props.push(mijia_core::card::CardProp {
        iid: 1003,
        name: "atmospheric-pressure".into(),
        desc: String::new(),
        comment: String::new(),
        format: "float".into(),
        unit: String::new(),
        access: vec!["read".into()],
        value_list: vec![],
        range: None,
        is_action: false,
        has_in: false,
    });
    let items = e.items_for(&spec, "temperature-humidity-sensor");
    assert_eq!(items.len(), 3, "带气压时 3 个观察值");
    assert!(items.iter().all(|i| i.kind == CardKind::ObserveNumber));
    assert!(items.iter().all(|i| !i.kind.is_control()), "不得含任何控制组件");
    assert!(items.iter().any(|i| i.piid == 1001 && i.unit == "℃"));
    assert!(items.iter().any(|i| i.piid == 1002));
    assert!(items.iter().any(|i| i.ratio == 0.001), "大气压应有 ratio 0.001");
}

#[test]
fn router_only_observe_number() {
    let e = engine();
    let spec = spec_of("xiaomi.router.ra80");
    let items = e.items_for(&spec, "router");
    assert_eq!(items.len(), 1, "路由器官方模板只 1 项");
    assert_eq!(items[0].kind, CardKind::ObserveNumber);
    assert_eq!(items[0].piid, 3, "connected-device-number = (2,3)");
    assert!(!items.iter().any(|i| i.kind.is_control()), "危险可写属性一个都不进卡");
}

#[test]
fn magnet_and_occupancy_sensor_status() {
    let e = engine();
    let magnet = e.items_for(&spec_of("isa.magnet.dw2hl"), "magnet-sensor");
    assert!(!magnet.is_empty() && magnet.iter().all(|i| i.kind == CardKind::SensorStatus));
    let occ = e.items_for(&spec_of("linp.sensor_occupy.hb01"), "occupancy-sensor");
    assert!(!occ.is_empty() && has_kind(&occ, CardKind::SensorStatus));
}

#[test]
fn camera_firmware_and_mask_cond() {
    let e = engine();
    let spec = spec_of("isa.camera.hlc7");
    let urn = "urn:miot-spec-v2:device:camera:0000A01C:isa-hlc7:2";
    let items = e.items_for(&spec, urn);
    assert!(has_kind(&items, CardKind::FirmwareText));
    let toggle = items.iter().find(|i| i.kind == CardKind::Toggle);
    assert!(toggle.is_some(), "应含 camera-control:on");
    // 固件文本在关机(on=false)时禁用：disable_status 的布尔 false 应归一化成 "false"
    let fw = items.iter().find(|i| i.kind == CardKind::FirmwareText).unwrap();
    assert!(
        fw.disabled_when.iter().any(|c| c.expect == "false"),
        "固件文本关机禁用条件应归一化: {:?}",
        fw.disabled_when
    );
    // physical-mask 属性本设备 spec 未收录 → 该条件正确跳过，不误判
}

#[test]
fn gateway_and_outlet_and_switch_have_controls() {
    let e = engine();
    let mut gw_spec = spec_of("lumi.gateway.mgl03");
    // 网关夹具只有 indicator-light.on，未收录 brightness：注入后模板亮度滑条命中，
    // 同时验证「逻辑服务名 light → 实例服务名 indicator-light」的后缀匹配。
    let svc = gw_spec.services.iter_mut().find(|s| s.iid == 6).unwrap();
    svc.props.push(mijia_core::card::CardProp {
        iid: 2,
        name: "brightness".into(),
        desc: String::new(),
        comment: String::new(),
        format: "uint8".into(),
        unit: String::new(),
        access: vec!["read".into(), "write".into()],
        value_list: vec![],
        range: None,
        is_action: false,
        has_in: false,
    });
    let gw = e.items_for(&gw_spec, "gateway");
    assert!(has_kind(&gw, CardKind::Toggle), "网关应含 light:on 开关");
    assert!(has_kind(&gw, CardKind::SeekBar), "亮度滑条(cardType 5)");
    let outlet = e.items_for(&spec_of("cuco.plug.cp1"), "outlet");
    assert!(has_kind(&outlet, CardKind::Toggle));
    let sw = e.items_for(&spec_of("xiaomi.switch.w1"), "switch");
    assert!(has_kind(&sw, CardKind::Toggle));
}

// ------------------------------------------------------------ 兜底/过滤 ----

#[test]
fn disable_type_exclusion_falls_back() {
    let e = engine();
    let spec = spec_of("xiaomi.router.ra80");
    // 该 URN 在 router 模板 disable_type 名单内 → 应走 spec 兜底
    let urn = "urn:miot-spec-v2:device:router:0000A036:xiaomi-ra80:1";
    let items = e.items_for(&spec, urn);
    assert!(!items.is_empty());
    assert!(
        items.iter().any(|i| i.kind == CardKind::ObserveNumber),
        "兜底应仍产出观察值"
    );
    assert!(
        items.iter().any(|i| i.piid == 3),
        "兜底优先 connected-device-number(2,3)"
    );
}

#[test]
fn scoped_conditions_dropped_for_other_urn() {
    let e = engine();
    let spec = spec_of("xiaomi.aircondition.m4");
    let urn = "urn:miot-spec-v2:device:air-conditioner:0000A004:xiaomi-m4:6";
    let items = e.items_for(&spec, urn);
    // 带 enable_type/disable_type URN 名单的 mode=Auto/Fan/Dry 条件不适用于 m4:6，
    // 只保留无条件 on=false 与 mode=off，故温度步进条件数应很小
    let temp = items.iter().find(|i| i.kind == CardKind::Stepper).unwrap();
    assert!(
        temp.disabled_when.len() <= 2,
        "URN 范围条件应被丢弃: {:?}",
        temp.disabled_when
    );
    assert!(temp.disabled_when.iter().any(|c| c.expect == "false"));
}

#[test]
fn fallback_for_unmatched_devices() {
    let e = engine();
    let root = classify();
    let mut checked = 0;
    for (_did, dev) in root.as_object().unwrap() {
        let model = dev["model"].as_str().unwrap_or("");
        let cat = category(model);
        let spec = CardSpec::from_classify(dev);
        let items = e.items_for(&spec, cat);
        assert!(!items.is_empty(), "{model} 应有兜底/模板 items");
        for it in &items {
            // 所有 item 地址有效（除占位的 0,0）
            assert!(it.siid >= 0 && it.piid >= 0);
            // 控制类必须仍是只读（enabled 仅联动灰显，不改变 kind）
            if it.kind.is_control() {
                assert!(!matches!(it.kind, CardKind::Hidden));
            }
        }
        checked += 1;
    }
    assert_eq!(checked, 16, "应覆盖 16 台设备");
}

#[test]
fn fallback_devices_are_spec_driven() {
    let e = engine();
    // 无线开关 0 可读 0 可写 → 兜底至少 1 个占位/状态项，绝不崩
    let sw = e.items_for(&spec_of("lumi.sensor_switch.v2"), "unknown-device-type");
    assert!(!sw.is_empty());
    // 按摩仪有 1 控制 1 观察 → 兜底含开关 + 观察值
    let mass = e.items_for(&spec_of("soocare.magic_touch.m14"), "massager");
    assert!(has_kind(&mass, CardKind::Toggle), "按摩仪兜底应含主开关");
}

#[test]
fn unit_percentage_falls_back_to_en_symbol() {
    // card_default_txt.units.percentage 只有 en："%"，无 zh_CN；
    // 归一化应回退成 "%"，不能把 "percentage" 原样透到卡片文案。
    let e = engine();
    let items = e.items_for(&spec_of("soocare.magic_touch.m14"), "massager");
    let units: Vec<&str> = items.iter().map(|i| i.unit.as_str()).collect();
    assert!(
        !units.iter().any(|u| u.contains("percentage")),
        "percentage 应本地化，实际 units={units:?}"
    );
    assert!(units.contains(&"%"), "应出现 % 单位，实际 units={units:?}");
}

// ------------------------------------------------------------------ 求值 ----

#[test]
fn evaluate_fills_value_and_code() {
    let e = engine();
    let spec = spec_of("xiaomi.sensor_ht.mini");
    let mut items = e.items_for(&spec, "temperature-humidity-sensor");
    e.evaluate(&mut items, &[pv(2, 1001, json!(26.5))]);
    let t = items.iter().find(|i| i.piid == 1001).unwrap();
    assert_eq!(t.value, Some(json!(26.5)));
    assert!(t.enabled);
}

#[test]
fn evaluate_string_false_matches_bool_false() {
    // 风扇模板条件 "false"(str) 与读值 false(bool) 归一化后相等
    let e = engine();
    let spec = spec_of("dmaker.fan.p5c");
    let mut items = e.items_for(&spec, "fan");
    let before = items.iter().filter(|i| i.enabled).count();
    e.evaluate(&mut items, &[pv(2, 1, json!(false))]); // 关机
    let after = items.iter().filter(|i| i.enabled).count();
    assert!(after < before, "关机应联动禁用若干控件");
}

// ------------------------------------------------------------- 二级 detail ----

/// 空调二级：模板项透出 value-range/param_delta，并追加 spec 全量可写与动作。
#[test]
fn detail_aircondition_range_delta_and_extras() {
    let e = engine();
    let inst: Value = serde_json::from_str(&fixture("specs/spec_aircondition_m4.json"))
        .expect("instance spec");
    let spec = CardSpec::from_instance(&inst);
    let urn = "urn:miot-spec-v2:device:air-conditioner:0000A004:xiaomi-m4:6";

    let home = e.items_for(&spec, urn);
    let detail = e.detail_items(&spec, urn);
    assert!(
        detail.len() > home.len(),
        "二级应比首页导出更多控制项: home={} detail={}",
        home.len(),
        detail.len()
    );

    // 模板步进项（cardType 4 设定温度）：range 16..31 step 0.5 + param_delta 1
    let temp = detail
        .iter()
        .find(|i| (i.siid, i.piid) == (2, 4) && !i.is_action)
        .expect("detail 应含设定温度");
    assert_eq!(temp.kind, CardKind::Stepper);
    assert_eq!(temp.min, Some(16.0));
    assert_eq!(temp.max, Some(31.0));
    assert_eq!(temp.step, Some(0.5));
    assert_eq!(temp.param_delta, Some(1.0));

    // spec 追加控制：ECO 节能(siid2 piid7 bool) → Toggle，设定湿度(siid2 piid14) 带 range
    let eco = detail
        .iter()
        .find(|i| (i.siid, i.piid) == (2, 7))
        .expect("detail 应追加 eco 开关");
    assert!(eco.kind.is_control() && !eco.is_action, "eco 应为可写属性项");
    let hum = detail
        .iter()
        .find(|i| (i.siid, i.piid) == (2, 14))
        .expect("detail 应追加设定湿度");
    assert_eq!((hum.min, hum.max, hum.step), (Some(0.0), Some(100.0), Some(1.0)));

    // 动作：filter.reset-filter-life → is_action，地址为 (服务 iid, 动作 iid)
    let act = detail
        .iter()
        .find(|i| i.is_action)
        .expect("detail 应含无参动作 reset-filter-life");
    assert_eq!((act.siid, act.piid), (21, 1));
    assert_eq!(act.kind, CardKind::Action);

    // 高级服务不进二级：dev-mode 动作与 system-parm 字符串属性排除
    assert!(
        !detail.iter().any(|i| i.is_action && i.siid == 25),
        "dev-mode 动作（恢复默认设置）不应导出"
    );
    assert!(
        !detail.iter().any(|i| i.siid == 39),
        "system-parm 服务不应进二级控制列表"
    );

    // 联动求值对 detail 同样生效：关机时模板 mode 灰显
    let mut d = detail;
    e.evaluate(&mut d, &[pv(2, 1, json!(false))]);
    let mode = d
        .iter()
        .find(|i| (i.siid, i.piid) == (2, 2) && i.kind == CardKind::EnumChoice)
        .unwrap();
    assert!(!mode.enabled, "关机时 mode 应联动禁用");

    // 控制区 label 追加服务后缀后应互不重复（m4 有多个 comment=「开关」的属性）
    let mut labels: Vec<String> = d
        .iter()
        .filter(|i| i.kind.is_control())
        .map(|i| i.label.clone())
        .collect();
    assert!(
        labels.iter().any(|l| l.contains("·")),
        "重名控制项应追加服务名后缀: {labels:?}"
    );
    let n = labels.len();
    labels.sort();
    labels.dedup();
    assert_eq!(labels.len(), n, "控制项 label 应唯一: {labels:?}");
}

/// 模板动作项（cardType 2 `p:..,a:..` 复合键）：二级解析成动作地址，首页仍属性寻址。
#[test]
fn detail_resolves_template_action_keys() {
    let e = engine();
    // 构造最小 vacuum spec（instance 形态：status 属性 + start-sweep 动作）
    let inst = json!({
        "services": [{
            "iid": 2,
            "type": "urn:miot-spec-v2:service:vacuum:00007804:xiaomi-vac:1",
            "properties": [{
                "iid": 1,
                "type": "urn:miot-spec-v2:property:status:0000000C:xiaomi-vac:1",
                "description": "Status",
                "format": "uint8",
                "access": ["read"],
                "value-list": [{"value": 5, "description": "Sweeping", "comment": "清扫中"}]
            }],
            "actions": [{
                "iid": 2,
                "type": "urn:miot-spec-v2:action:start-sweep:00002801:xiaomi-vac:1",
                "description": "Start Sweep",
                "in": [],
                "out": []
            }]
        }]
    });
    let spec = CardSpec::from_instance(&inst);
    let urn = "urn:miot-spec-v2:device:vacuum:0000A01B:xiaomi-vac:1";

    let home = e.items_for(&spec, urn);
    let act_home = home
        .iter()
        .find(|i| i.kind == CardKind::Action)
        .expect("首页 vacuum action 项存在");
    assert!(!act_home.is_action, "首页仍按属性(status)寻址");
    assert_eq!((act_home.siid, act_home.piid), (2, 1));

    let detail = e.detail_items(&spec, urn);
    let act = detail
        .iter()
        .find(|i| i.kind == CardKind::Action && i.is_action)
        .expect("二级 action 应解析到 a:vacuum:start-sweep");
    assert_eq!((act.siid, act.piid), (2, 2), "动作地址 = (服务 iid, 动作 iid)");
}

/// 模板未命中设备（按摩仪）：二级走 fallback_full，控制项比首页兜底更全。
#[test]
fn detail_fallback_full_supersets_home_fallback() {
    let e = engine();
    let spec = spec_of("soocare.magic_touch.m14");
    let home = e.items_for(&spec, "massager");
    let detail = e.detail_items(&spec, "massager");
    assert!(has_kind(&detail, CardKind::Toggle), "兜底二级应含主开关");
    // 首页兜底会把 on 同时当主开关与观察值（重复地址）；按去重地址比较覆盖关系
    let uniq = |items: &[mijia_core::card::CardItemVm]| -> std::collections::HashSet<(i64, i64)> {
        items.iter().map(|i| (i.siid, i.piid)).collect()
    };
    let hu = uniq(&home);
    let du = uniq(&detail);
    assert!(
        hu.is_subset(&du) && du.len() >= hu.len(),
        "二级地址集应覆盖首页: home={hu:?} detail={du:?}"
    );
    // 二级不含 device-information / 不可写项
    for it in &detail {
        if it.kind.is_control() {
            assert!(!it.is_action || it.siid > 0, "动作项地址必须有效");
        }
    }
}

/// 网关二级：布防调参区（arming 服务/customize-mode/delay/volume）不展开，
/// 模板主控制保留。
#[test]
fn detail_gateway_skips_tuning_props() {
    let e = engine();
    let spec = spec_of("lumi.gateway.mgl03");
    let detail = e.detail_items(&spec, "gateway");
    for it in &detail {
        let l = it.label.as_str();
        assert!(
            !l.contains("自定义警戒模式") && !l.contains("customize"),
            "customize-mode 调参应排除: {l}"
        );
        assert!(
            !l.contains("delay") && !l.contains("volume") && !l.contains("duration"),
            "delay/volume/duration 调参应排除: {l}"
        );
        assert!(!l.contains("arming"), "arming 服务应整体排除: {l}");
    }
    assert!(has_kind(&detail, CardKind::Toggle), "模板主开关仍在");
}

/// 门磁 contact-state「讲人话」：label=门磁状态、name_ignore、
/// **choices false=门已开 / true=门已关**。
///
/// 方向以真机实测为准（`card.rs:755` 注释：2026-11-17 dw2hl 实测「用户开门时属性=false」），
/// 与 miot-spec 标准一致（contact-state 的 1=Contact=磁体吸合=门关，0=No Contact=门开）。
/// 本用例原先写的是反的（false=门关），是修复前留下的旧信念；因为集成测试长期没被跑，
/// 直到 2026-11-17 做 `cargo test`（不带 --lib）才暴露。别再改回
/// 去，除非有新的真机测量。
/// 另：官方 names 表把 Contact State 译成「关闭」，与 bool 值拼接会出「关闭 开」矛盾文案，
/// 所以才要 name_ignore 只显示值词。
#[test]
fn magnet_contact_state_plain_words() {
    let e = engine();
    let spec = spec_of("isa.magnet.dw2hl");
    let mut items = e.items_for(&spec, "urn:miot-spec-v2:device:magnet-sensor:0000A016:isa-dw2hl:1");
    let it = items
        .iter()
        .find(|i| i.kind == CardKind::SensorStatus && (i.siid, i.piid) == (2, 2))
        .expect("门磁应含 contact-state 状态项");
    assert_eq!(it.label, "门磁状态");
    assert!(it.name_ignore, "状态行应只显示值词，不拼 label");
    assert_eq!(it.choices.len(), 2);
    assert_eq!(it.choices[0].value, json!(false));
    assert_eq!(it.choices[0].label, "门已开", "false=No Contact=开门");
    assert_eq!(it.choices[1].value, json!(true));
    assert_eq!(it.choices[1].label, "门已关", "true=Contact=吸合关门");
    // evaluate 回填后 value=false（开门），渲染层按 choices 取「门已开」
    e.evaluate(&mut items, &[pv(2, 2, json!(false))]);
    let door = items.iter().find(|i| i.piid == 2).unwrap();
    assert_eq!(door.value, Some(json!(false)));
    assert_eq!(door.code, 0);
}
