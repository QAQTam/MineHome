//! 表驱动卡片回归（社区 PR 的守门测试）。
//!
//! 遍历 `mijia-core/profiles/*.json`，每份档案自动展开成一整组断言：
//! **贡献者新增一个型号 = 加一个 JSON 文件，不需要写一行 Rust、不需要账号、不需要设备。**
//!
//! 两类断言：
//! 1. **黄金期望**（`expect.items` / `expect.quick_switch` / reason）——钉住该型号的输出；
//!    注意这批期望初值由 `examples/gen_profiles.rs` 从当前行为生成，属 characterization
//!    test：它防的是"以后改坏"，正确性仍靠 `card_engine.rs` 里手写的逐条断言与真机验收。
//! 2. **不依赖期望值的 property 断言**——这类才是真的"抓新 bug"：
//!    - 控制项绝不允许落在 `safety::skip_control_service` 名单里的服务上（布防/调参/system-parm…）；
//!    - 档案文件本身不得含账号字段（did / token / 用户自取设备名），防止有人把真机导出
//!      直接改名提交进来。
//!
//! 全部只读、纯 PC、零网络。

use mijia_core::card::{CardEngine, CardKind, CardSpec};
use mijia_core::layout;
use mijia_core::safety;
use serde_json::Value;
use std::path::{Path, PathBuf};

fn repo_fixtures() -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join("templates/card_config_card_control_miotspec_config.json");
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("读 {} 失败: {e}", p.display()))
}

fn fix(rel: &str) -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("读 {} 失败: {e}", p.display()))
}

fn engine() -> CardEngine {
    CardEngine::from_json(
        &repo_fixtures(),
        &fix("templates/card_config_card_default_txt.json"),
        &fix("templates/card_config_card_control_config.json"),
    )
    .expect("解析三份模板配置")
}

fn profiles_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("profiles")
}

/// 遍历目录里的档案，按 model 取 spec。
fn each_profile(mut f: impl FnMut(&Path, &Value, &Value, &CardSpec, &CardEngine)) {
    let dir = profiles_dir();
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("读 {} 失败: {e}", dir.display()))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("json"))
        .collect();
    files.sort();
    assert!(
        files.len() >= 10,
        "profiles/ 只有 {} 份档案，疑似目录被清空（应至少覆盖夹具里的全部型号）",
        files.len()
    );
    let root: Value = serde_json::from_str(&fix("specs_by_model.json")).expect("specs_by_model");
    let eng = engine();
    for path in files {
        let text = std::fs::read_to_string(&path).expect("读档案");
        let p: Value = serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("{} 不是合法 JSON: {e}", path.display()));
        let model = p["model"].as_str().unwrap_or("");
        assert!(
            root.get(model).is_some(),
            "{}: 档案里的 model={model} 在 fixtures/specs_by_model.json 里不存在",
            path.display()
        );
        let spec = CardSpec::from_classify(&root[model]);
        f(&path, &p, &root[model], &spec, &eng);
    }
}

#[test]
fn 档案覆盖数与夹具型号一致() {
    let root: Value = serde_json::from_str(&fix("specs_by_model.json")).expect("specs_by_model");
    let models = root.as_object().map(|m| m.len()).unwrap_or(0);
    let n = std::fs::read_dir(profiles_dir())
        .expect("读 profiles 目录")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
        .count();
    assert_eq!(n, models, "有型号缺档案，或有档案对应不到型号");
}

#[test]
fn 每份档案的黄金期望都成立() {
    each_profile(|path, p, _dev, spec, eng| {
        let category = p["category"].as_str().unwrap_or("unknown-device-type");
        let items = eng.items_for(spec, category);
        let got: Vec<String> = items.iter().map(layout::key_of).collect();
        let want: Vec<String> = p["expect"]["items"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|v| v.as_str().unwrap_or_default().to_string())
                    .collect()
            })
            .unwrap_or_default();
        assert_eq!(got, want, "{}: 卡片项序列变了", path.display());

        let dec = safety::quick_switch(p["model"].as_str().unwrap_or(""), "", spec, &items);
        let want_allowed = p["expect"]["quick_switch"].as_bool().unwrap_or(false);
        assert_eq!(
            dec.allowed, want_allowed,
            "{}: 圆钮放行结论变了（reason={}）",
            path.display(),
            dec.reason.code()
        );
        assert_eq!(
            p["expect"]["quick_switch_reason"].as_str().unwrap_or(""),
            dec.reason.code(),
            "{}: 放行理由码变了，PR 里要写清为什么",
            path.display()
        );
    });
}

#[test]
fn 控制项绝不允许落在被排除的服务上() {
    each_profile(|path, p, _dev, spec, eng| {
        let category = p["category"].as_str().unwrap_or("unknown-device-type");
        for it in eng.items_for(spec, category) {
            if !it.kind.is_control() || it.kind == CardKind::Hidden {
                continue;
            }
            let Some(svc) = spec.services.iter().find(|s| s.iid == it.siid) else {
                continue;
            };
            assert!(
                !safety::skip_control_service(&svc.name),
                "{}: 控制项 ({},{}) 落在被排除服务 {} 上",
                path.display(),
                it.siid,
                it.piid,
                svc.name
            );
        }
    });
}

#[test]
fn 基础设施设备的档案一律不给圆钮() {
    each_profile(|path, p, _dev, spec, eng| {
        let model = p["model"].as_str().unwrap_or("");
        if !safety::is_infra(model, "") {
            return;
        }
        let category = p["category"].as_str().unwrap_or("unknown-device-type");
        let items = eng.items_for(spec, category);
        let dec = safety::quick_switch(model, "", spec, &items);
        assert!(
            !dec.allowed && dec.reason == safety::SwitchReason::BlockedInfra,
            "{}: 基础设施设备 {model} 竞被放行一键（reason={}）",
            path.display(),
            dec.reason.code()
        );
    });
}

#[test]
fn 档案文件里不得有账号痕迹() {
    // 有人会把真机导出的 JSON 改个名就提交进来。这里按形态拦一道。
    let bad_keys = ["\"did\"", "\"userId\"", "\"token\"", "\"mac\"", "\"address\""];
    each_profile(|path, _p, _dev, _spec, _eng| {
        let text = std::fs::read_to_string(path).expect("读档案");
        for k in bad_keys {
            assert!(
                !text.contains(k),
                "{}: 档案含敏感字段 {k}",
                path.display()
            );
        }
        assert!(
            text.contains("\"scrub\""),
            "{}: 档案缺 scrub 脱敏标记",
            path.display()
        );
    });
}

#[test]
fn 档案能驱动布局过滤且不改形态() {
    each_profile(|path, p, _dev, spec, eng| {
        let model = p["model"].as_str().unwrap_or("");
        let category = p["category"].as_str().unwrap_or("unknown-device-type");
        let items = eng.items_for(spec, category);
        // 只留第一项：形态必须原样保留（轻档不允许改 kind）
        let first = layout::key_of(&items[0]).clone();
        let lay = mijia_core::layout::CardLayout {
            visible: vec![first.clone()],
            known: items.iter().map(layout::key_of).collect(),
        };
        let (out, appended) = layout::apply(items.clone(), Some(&lay));
        assert_eq!(out.len(), 1, "{model}: 裁剪没生效 {}", path.display());
        assert_eq!(layout::key_of(&out[0]), first);
        assert_eq!(out[0].kind, items[0].kind, "{model}: kind 被改动了");
        assert_eq!(appended, 0, "{model}: known 里的项被当新项追回");
    });
}

#[test]
fn 灰度发布开关不得超出基线且打开必须留确认痕迹() {
    // 双字段语义（用户 2026-11-17 拍板）：expect.quick_switch = 行为基线（判定结果），
    // release.quick_switch = 发布开关（只读先行，默认 false）。
    // 这里钉住两条不变量：
    //   1. release ⊆ 基线：判定不放行的型号，发布**永远**不许放行；
    //   2. release=true 必须有人工确认记录（confirmed_by 非空），否则不许合入。
    // "当前阶段全部应为 false" 不钉成永久规则——那是流程事实，不是不变量；
    // 逐型号打开时改数据、走评审即可，上面两条始终兜底。
    each_profile(|path, p, _dev, _spec, _eng| {
        let rel_on = p["release"]["quick_switch"].as_bool().unwrap_or(false);
        let base_on = p["expect"]["quick_switch"].as_bool().unwrap_or(false);
        assert!(
            !rel_on || base_on,
            "{}: release.quick_switch 放行而基线不放行——发布不得超出行为基线",
            path.display()
        );
        if rel_on {
            let who = p["release"]["confirmed_by"].as_str().unwrap_or("");
            assert!(
                !who.trim().is_empty(),
                "{}: release.quick_switch=true 但没有人工确认记录（confirmed_by）",
                path.display()
            );
        }
    });
}
