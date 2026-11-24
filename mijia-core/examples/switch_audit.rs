//! 圆钮放行审计（只读，不下发任何控制）：对账号下每台设备打印
//! 「模板主 toggle 是什么、spec 规范 on 是什么、判定结论与理由码」。
//!
//! 用途：判定逻辑已下沉到 `safety.rs`，这台"审计台"就是它的可观测面——
//! 出现"多放行/少放行"时不用靠真机截图反推，PC 上跑一遍就知道每台卡在哪条规则。
//!
//! 用法：cargo run --release --example switch_audit
//! 凭据：工作区根 session.json（与 Python 原型互通）

use mijia_core::{card_net, safety, MiHome};
use std::path::Path;

#[tokio::main]
async fn main() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../session.json");
    if !path.exists() {
        eprintln!("缺 session.json（{}）", path.display());
        std::process::exit(1);
    }
    let home = MiHome::new(&path).expect("MiHome");
    let cache = card_net::card_cache_dir(&path);
    let devices = home.list_devices().await.expect("list_devices");
    println!("账号下 {} 台设备\n", devices.len());

    let mut allowed = 0usize;
    for d in &devices {
        // 离线设备首页走「离线」分支，本就不给圆钮，跳出不判定
        if !d.online {
            println!("{:<24} {:<26} 离线，不参与判定", d.name, d.model);
            continue;
        }
        if d.spec_type.as_deref().unwrap_or("").is_empty() {
            println!("{:<24} {:<26} 无 spec_type，不判定", d.name, d.model);
            continue;
        }
        let st = d.spec_type.clone().unwrap_or_default();
        let items = match card_net::card_items(&d.did, &st, &cache, &home, None).await {
            Ok(v) => v,
            Err(e) => {
                println!("{:<24} {:<26} card_items 失败: {e}", d.name, d.model);
                continue;
            }
        };
        let spec = match card_net::load_spec(&cache, &st).await {
            Ok(s) => s,
            Err(e) => {
                // 与 napi 侧同口径：spec 取不到就用空 spec，判定退回 label 兜底
                println!("{:<24} {:<26} [spec 读取失败，按空 spec 判定: {e}]", d.name, d.model);
                Default::default()
            }
        };
        // 对照：上一轮的判定吃的是 device_list_page 的**内嵌** spec，
        // 搬进 Rust 后吃的是 miot-spec.org 的 instance spec。两者不一致就等于口径变了，
        // 所以这里把两个来源各自解析出的「规范 on」都打出来，用数据说话。
        let embedded_on = d
            .spec
            .as_ref()
            .map(|v| safety::spec_on_prop(&mijia_core::card::CardSpec::from_instance(v)))
            .flatten();
        let tg = safety::main_toggle(&items);
        let on = safety::spec_on_prop(&spec);
        let dec = safety::quick_switch(&d.model, &st, &spec, &items);
        if dec.allowed {
            allowed += 1;
        }
        println!(
            "{:<3} {:<24} toggle={:<9} 内嵌on={:<9} 公开on={:<9} {:<6} {:<22} label={}",
            if dec.allowed { "✅" } else { "—" },
            d.name,
            tg.map(|t| format!("{}:{}", t.siid, t.piid))
                .unwrap_or_else(|| "-无".to_string()),
            embedded_on
                .map(|(i, p)| format!("{i}:{p}"))
                .unwrap_or_else(|| "-无".to_string()),
            on.map(|(i, p)| format!("{i}:{p}"))
                .unwrap_or_else(|| "-无".to_string()),
            if dec.allowed { "ALLOW" } else { "DENY" },
            dec.reason.code(),
            tg.map(|t| t.label.clone())
                .unwrap_or_else(|| "-".to_string()),
        );
    }
    println!("\n在线判定放行 {allowed} 台（首页圆钮数应与此一致）");
}
