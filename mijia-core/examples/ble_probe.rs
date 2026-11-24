//! PC 只读工具：解码一条 MiBeacon 广播。
//! 用法：cargo run --release --example ble_probe -- <token|""> <adv_hex> [known_mac_hex]
//! 输出：解析出的 JSON（mac/temperature/humidity/battery/door_closed/unknown...）
use mijia_core::ble;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: ble_probe <token> <adv_hex> [known_mac]");
        std::process::exit(2);
    }
    let token = if args[1] == "-" { "" } else { args[1].as_str() };
    let hex = args[2].trim();
    let adv: Option<Vec<u8>> = (0..hex.len() / 2)
        .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok())
        .collect();
    let mb = || -> Option<mijia_core::ble::MiBeacon> {
        let a = adv.as_deref()?;
        if let Some(m) = args.get(3) {
            let t = m.trim().replace(':', "");
            if t.len() != 12 {
                return None;
            }
            let mut km = [0u8; 6];
            for i in 0..6 {
                km[i] = u8::from_str_radix(&t[i * 2..i * 2 + 2], 16).ok()?;
            }
            return ble::decode_adv_mac(token, a, Some(&km));
        }
        ble::decode_adv(token, a)
    };
    match mb() {
        Some(mb) => println!("{}", serde_json::to_string_pretty(&mb).unwrap()),
        None => println!("null  // 非米家 fe95 广播 / 解不开 / 不支持的格式"),
    }
}
