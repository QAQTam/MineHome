//! 能力盘点（只读）：拉设备列表并枚举 spec 中可写属性（=可控制能力）。
//! 不下发任何控制请求。
//!
//! 用法：cargo run --example spec_dump
use mijia_core::MiHome;

/// 从 URN 提取名称：urn:miot-spec-iot:property:on:0000... -> "on"
fn urn_name(urn: &str) -> &str {
    urn.split(':').nth(3).unwrap_or(urn)
}

#[tokio::main]
async fn main() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../session.json");
    let home = MiHome::new(&path).expect("创建 MiHome");
    let devices = home.list_devices().await.expect("list_devices");
    println!("共 {} 台设备：", devices.len());
    for d in &devices {
        println!("\n=== {} | {} | did={} | {}{} ===",
            d.name, d.model, d.did,
            if d.online { "online" } else { "offline" },
            if d.token.is_some() { " [token✓]" } else { "" });
        let Some(spec) = &d.spec else {
            println!("  (无 spec)");
            continue;
        };
        let Some(services) = spec.get("services").and_then(|v| v.as_array()) else {
            println!("  spec 无 services 数组，keys={:?}",
                spec.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>()));
            continue;
        };
        for svc in services {
            let svc_name = svc.get("type")
                .and_then(|t| t.as_str())
                .map(urn_name)
                .unwrap_or("?");
            let Some(props) = svc.get("properties").and_then(|v| v.as_array()) else {
                continue;
            };
            let writable: Vec<String> = props.iter().filter_map(|p| {
                let access = p.get("access").and_then(|v| v.as_array())?;
                let can_write = access.iter()
                    .any(|a| a.as_str() == Some("write") || a.as_str() == Some("writeonly"));
                if !can_write { return None; }
                let name = p.get("type").and_then(|t| t.as_str())
                    .map(urn_name).unwrap_or("?");
                let siid = svc.get("iid").and_then(|v| v.as_i64()).unwrap_or(0);
                let piid = p.get("iid").and_then(|v| v.as_i64()).unwrap_or(0);
                let range = p.get("value-range")
                    .and_then(|v| serde_json::to_string(v).ok())
                    .unwrap_or_default();
                let list = p.get("value-list")
                    .and_then(|v| serde_json::to_string(v).ok())
                    .unwrap_or_default();
                Some(format!("    s{siid}/p{piid} {svc_name}.{name} {}{}",
                    if range.is_empty() { String::new() } else { format!(" range={range}") },
                    if list.is_empty() { String::new() } else { format!(" list={list}") }))
            }).collect();
            let readable = props.iter()
                .filter(|p| p.get("access").and_then(|v| v.as_array())
                    .map(|a| a.iter().any(|a| a.as_str() == Some("read")))
                    .unwrap_or(false))
                .count();
            if !writable.is_empty() {
                for w in writable {
                    println!("{w}");
                }
            } else if readable > 0 {
                println!("    s{}/? [{svc_name}: 只读 x{readable}]",
                    svc.get("iid").and_then(|v| v.as_i64()).unwrap_or(0));
            }
        }
    }
}
