//! 集成冒烟：真实会话拉设备列表（只读，不控制任何设备）。
//!
//! 用法：cargo run --example live_devices
//! 凭据：工作区根目录 session.json（Python 原型生成，格式互通）。

use mijia_core::MiHome;

#[tokio::main]
async fn main() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../session.json");
    if !path.exists() {
        eprintln!("缺 session.json（{}）——先用 Python 原型登录", path.display());
        std::process::exit(1);
    }
    let home = MiHome::new(&path).expect("创建 MiHome");
    if !home.is_logged_in() {
        eprintln!("session.json 加载失败");
        std::process::exit(1);
    }
    let devices = home.list_devices().await.expect("list_devices");
    println!("共 {} 台设备：", devices.len());
    for d in &devices {
        println!(
            "  {:<12} {:<28} {:<16} {}{}  房间={:<12} rank={:?}",
            d.did,
            d.model,
            d.name,
            if d.online { "online" } else { "offline" },
            if d.token.is_some() { "  [token✓]" } else { "" },
            // 三态：None=云端归属没拉到（未知）；Some("")=拉到了但没进房间
            match &d.room {
                None => "?未知".to_string(),
                Some(s) if s.is_empty() => "-未分组".to_string(),
                Some(s) => s.clone(),
            },
            d.room_rank,
        );
    }
}
