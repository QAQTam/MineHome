//! 调试（只读）：打印 device_list_page 第一台设备的原始字段名与类型。
use mijia_core::MiHome;
use serde_json::Value;

#[tokio::main]
async fn main() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../session.json");
    let home = MiHome::new(&path).expect("创建 MiHome");
    // 借用 DeviceClient 的原始响应：通过 list_devices 后无法回看原始，
    // 这里直接复用 cloud 端点。为最小侵入，用 list_devices 的第一台打印
    // 已解析字段 + 通过 home 暴露的原始接口（若有）。
    let devices = home.list_devices().await.expect("list_devices");
    let first = devices.first().expect("空列表");
    println!("已解析字段: did={} model={} spec={:?}",
        first.did, first.model,
        first.spec.as_ref().map(|s| s.to_string().chars().take(80).collect::<String>()));
    // 提示：需要原始 payload 时改用 cloud::CloudClient 侧调试
}
