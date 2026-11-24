//! 生成 `mijia-core/profiles/<model>.json`（表驱动回归测试的输入）。
//!
//! 用法：`cargo run --example gen_profiles`（纯离线，只读 `fixtures/`，不发网络请求）
//!
//! **这些期望值是从"当前引擎输出"取的，属于 characterization test**：
//! 它能钉住"以后别再改坏"，但**不能证明现在的输出是对的**——正确的部分靠
//! `tests/card_engine.rs` 里那些逐条手写的断言，以及真机验收。
//! 新增型号时：先跑本生成器拿骨架，再**人工把 expect 改成"应该是这样"**，
//! 这才是给外部 PR 用的验收基线。
//!
//! `values` 一律为空数组（= 不注入属性值，只做静态结构断言）；要测联动，
//! 人工在 profile 里补 `values` 与 `expect.linkage`。

use mijia_core::card::{CardEngine, CardSpec};
use mijia_core::layout;
use mijia_core::safety;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

fn fix(name: &str) -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(name);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("读 {} 失败: {e}", p.display()))
}

/// model → 官方模板类别（第 4 段 URN name）。
/// 来源是这台账号里 16 台设备的实测归类；没模板类别的走 `unknown-device-type` 兜底。
fn category_of(model: &str) -> &'static str {
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
        "soocare.magic_touch.m14" => "massager",
        _ => "unknown-device-type",
    }
}

fn main() {
    let out_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("profiles");
    std::fs::create_dir_all(&out_dir).expect("建 profiles 目录");

    let engine = CardEngine::from_json(
        &fix("templates/card_config_card_control_miotspec_config.json"),
        &fix("templates/card_config_card_default_txt.json"),
        &fix("templates/card_config_card_control_config.json"),
    )
    .expect("解析三份模板配置");

    let root: Value = serde_json::from_str(&fix("specs_by_model.json")).expect("specs_by_model");
    let mut written = 0usize;
    let models: Vec<&String> = root.as_object().map(|m| m.keys().collect()).unwrap_or_default();
    for model in models {
        let dev = &root[model];
        let spec = CardSpec::from_classify(dev);
        let category = category_of(model);
        let items = engine.items_for(&spec, category);
        if items.is_empty() {
            eprintln!("[跳过] {model} 引擎产出为空（既没命中模板也没落兜底）");
            continue;
        }
        let dec = safety::quick_switch(model, "", &spec, &items);
        let keys: Vec<String> = items.iter().map(layout::key_of).collect();
        let profile = json!({
            "version": 1,
            "model": model,
            "category": category,
            "scrub": "synthetic",
            "values": [],
            "expect": {
                "items": keys,
                "quick_switch": dec.allowed,
                "quick_switch_reason": dec.reason.code(),
                // 控制项绝不允许落在这些服务里（名单同样来自 safety，测试会独立复核一遍）
                "control_services_excluded": true
            },
            // 灰度双字段（用户 2026-11-17 拍板）：
            // expect.quick_switch 是**行为基线**（当前判定结果，测试钉住不许悄悄变）；
            // release.quick_switch 是**发布开关**（默认 false = 只读发一版），
            // 打开必须有人工确认记录（confirmed_by），且 release ⊆ 基线（tests/profiles.rs 拦住）。
            "release": {
                "quick_switch": false,
                "confirmed_by": "",
                "policy": "只读先行：打开需人工确认并在 confirmed_by 记录确认人/日期"
            }
        });
        let path = out_dir.join(format!("{model}.json"));
        std::fs::write(&path, serde_json::to_string_pretty(&profile).unwrap())
            .unwrap_or_else(|e| panic!("写 {} 失败: {e}", path.display()));
        written += 1;
        println!(
            "[写] {model:<28} 项={:<2} 基线={:<5} 发布=false reason={}",
            keys.len(),
            dec.allowed,
            dec.reason.code()
        );
    }
    println!("\n共写 {written} 份 profile → {}", out_dir.display());
}
