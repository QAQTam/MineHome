//! # MiBeacon BLE 广播解析（档位 A：本地蓝牙收数，只读）
//!
//! 从 `0xFE95`（Mi Service）服务数据解析小米 MiBeacon **V2/V3 legacy** 广播帧并
//! AES-CCM* 解密，语义对齐权威参考实现 custom-components/ble_monitor 的
//! xiaomi.py（本地留档 `_work/xiaomi_ble_parser.py`）：
//! - 密钥：设备 token（真机 device list 为 hex 字符串：24 位=12B / 32 位=16B；
//!   base64 口径自适应）：legacy 12B → `token[0..6] ++ 8d3d3c97 ++ token[6..12]`
//!   扩成 16B；V4/V5 标准用 16B bind key 直用（token 只有 12B 时按 legacy 扩展
//!   法试一次，MIC 校验通过才算数）；
//! - legacy nonce(13B) = `ctrl2+devid2+cnt1 ++ mic前3B ++ mac逆序前5B`；
//!   V4/V5 nonce(12B) = `mac逆序6B ++ devid2+cnt1 ++ salt3`；AAD = `0x11`；
//! - legacy **不校验 MIC**（HA 用 decrypt 而非 decrypt_and_verify），本模块以
//!   CCM* keystream 解密（A0 只用于 S0，数据 keystream 从 A1 起）；
//!   V4/V5 **强制验 MIC**（RFC 3610 `U = T ⊕ S0[0..4]`，与 pycryptodome
//!   encrypt_and_digest 对拍，见 v5 golden 测试），防错钥静默出垃圾；
//! - object TLV：`type(2LE) + len(1) + data`，事件型（0x2xxx/0x4xxx）前置 5B 时间戳；
//!   人话值：0x1004 温度(÷10)、0x1006 湿度(÷10)、0x100d 温湿度、0x100a 电量、
//!   0x0007 门（0=开 1=关）。
//!
//! 零网络、零鸿蒙依赖：ArkTS `ble.on('BLEDeviceFind')` 拿原始 advertising 字节，
//! base64 后经 napi [`crate::ble`] 解码（仅本地上线；mesh/新硬件版本不在本期）。

use base64::Engine;
use serde_json::{Map, Value};

/// Legacy(V2/V3) 密钥扩展魔数（与 HA ble_monitor 一致）
const MAGIC: [u8; 4] = [0x8d, 0x3d, 0x3c, 0x97];

/// 一条解析成功的 MiBeacon 广播。
/// `Deserialize` 供 ble_cache::ingest 回读 NAPI 透传的 JSON（落库快照）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MiBeacon {
    /// 6 字节 MAC（标准序），小写 hex 无分隔
    pub mac: String,
    /// 小米产品/型号 id（广播内含或调用方设备匹配）
    pub device_id: u16,
    /// MiBeacon 版本（本模块只解 v2/v3）
    pub version: u8,
    /// 帧计数器（上层可用于去重）
    pub packet_id: u8,
    /// 是否加密帧（成功解密后为 true）
    pub encrypted: bool,
    /// 人话值（temperature/humidity/battery/door_closed ...）
    pub values: Map<String, Value>,
    /// 未识别 object typecode（hex），供真机字段核对；不视为错误
    pub unknown: Vec<String>,
}

/// 从 BLE 原始 advertising 字节（AD 结构序列）解析 MiBeacon。
/// `token`：米家设备列表 token 字段（hex 24/32 字符，或 base64 解出 12/16B）；
/// 明文帧无需 token（传空串即可），加密帧解密失败返回 None。
pub fn decode_adv(token: &str, adv: &[u8]) -> Option<MiBeacon> {
    decode_adv_mac(token, adv, None)
}

/// 同 [`decode_adv`]，但可传**设备真实 mac**（标准序）：RPA 广播 + 包内无 mac 的
/// 省电帧（如 MJWSD06MMC 0x4859 帧）V5 nonce 需要它，上层（弹层单目标）知道就能解。
pub fn decode_adv_mac(token: &str, adv: &[u8], known_mac: Option<&[u8; 6]>) -> Option<MiBeacon> {
    let payload = find_fe95(adv)?;
    parse_payload(token, payload, known_mac)
}

/// token → 原始 key 字节：24/32 位纯 hex（真机 device list 口径）优先按 hex 解，
/// 否则按 base64 解；仅接受 12B 或 16B 的合法 key 长度。
pub fn parse_token(token: &str) -> Option<Vec<u8>> {
    let t = token.trim();
    if t.is_empty() {
        return None;
    }
    if (t.len() == 24 || t.len() == 32) && t.chars().all(|c| c.is_ascii_hexdigit()) {
        let mut out = Vec::with_capacity(t.len() / 2);
        for i in (0..t.len()).step_by(2) {
            out.push(u8::from_str_radix(&t[i..i + 2], 16).ok()?);
        }
        return Some(out);
    }
    let raw = base64::engine::general_purpose::STANDARD.decode(t).ok()?;
    if raw.len() == 12 || raw.len() == 16 {
        Some(raw)
    } else {
        None
    }
}

/// legacy 12B bind key → 16B AES key（HA `token[0:6]+8d3d3c97+token[6:]`）
fn expand_legacy_key(raw12: &[u8]) -> [u8; 16] {
    let mut key = [0u8; 16];
    key[..6].copy_from_slice(&raw12[..6]);
    key[6..10].copy_from_slice(&MAGIC);
    key[10..].copy_from_slice(&raw12[6..]);
    key
}

/// 不解密，仅从广播头提取**包内 MAC**（标准序 hex，小写）。
/// 用途：设备用 RPA 源地址广播时，上层按 deviceId 查不到 token，可拿包内 mac 二次查找。
/// 帧无 mac 位/非 fe95 → None。
pub fn peek_mac(adv: &[u8]) -> Option<String> {
    let p = find_fe95(adv)?;
    if p.len() < 11 {
        return None;
    }
    let ctrl = u16::from_le_bytes([p[0], p[1]]);
    if (ctrl >> 4) & 1 != 1 {
        return None;
    }
    let mut mac = [0u8; 6];
    mac.copy_from_slice(&p[5..11]);
    mac.reverse();
    Some(mac.iter().map(|b| format!("{b:02x}")).collect())
}

/// 在 AD 结构中查找 `0x16` service data 且 uuid == 0xFE95（Mi Service）
fn find_fe95(adv: &[u8]) -> Option<&[u8]> {
    let mut p = 0usize;
    while p + 2 <= adv.len() {
        let len = adv[p] as usize;
        if len == 0 {
            break;
        }
        let end = p + 1 + len;
        if end > adv.len() {
            break; // 截断帧
        }
        let ty = adv[p + 1];
        let data = &adv[p + 2..end];
        if ty == 0x16 && data.len() >= 4 && data[0] == 0x95 && data[1] == 0xfe {
            return Some(&data[2..]);
        }
        p = end;
    }
    None
}

fn parse_payload(token: &str, payload: &[u8], known_mac: Option<&[u8; 6]>) -> Option<MiBeacon> {
    // ctrl(2LE) | device_id(2LE) | packet_counter(1) | [mac(6, 逆序)] | [capability(1)] | objs/mic
    if payload.len() < 9 {
        return None;
    }
    let ctrl = u16::from_le_bytes([payload[0], payload[1]]);
    let version = (ctrl >> 12) as u8;
    let encrypted = (ctrl >> 3) & 1 == 1;
    let mac_include = (ctrl >> 4) & 1 == 1;
    let capability_include = (ctrl >> 5) & 1 == 1;
    let object_include = (ctrl >> 6) & 1 == 1;
    let is_mesh = (ctrl >> 7) & 1 == 1;
    if is_mesh || version < 2 {
        return None; // mesh 广播（档位 C）与 v1 旧格式不在本期
    }
    let device_id = u16::from_le_bytes([payload[2], payload[3]]);
    let packet_id = payload[4];

    let mut i = 5usize;
    let mut mac = known_mac.copied().unwrap_or([0u8; 6]);
    if mac_include {
        if payload.len() < i + 6 {
            return None;
        }
        mac.copy_from_slice(&payload[i..i + 6]);
        mac.reverse(); // 广播内为逆序
        i += 6;
    }
    if capability_include {
        i += 1;
    }
    if !object_include {
        return None;
    }

    let objs: Vec<u8> = if encrypted {
        if version >= 4 {
            decrypt_v45(token, payload, i, &mac)?
        } else {
            decrypt_legacy(token, payload, i, &mac)?
        }
    } else {
        payload[i..].to_vec()
    };

    let (values, unknown) = parse_objects(&objs);
    Some(MiBeacon {
        mac: mac.iter().map(|b| format!("{b:02x}")).collect(),
        device_id,
        version,
        packet_id,
        encrypted,
        values,
        unknown,
    })
}

/// Legacy(V2/V3) 解密：CCM* keystream，不校验 MIC（对齐 HA decrypt() 行为）。
fn decrypt_legacy(token: &str, payload: &[u8], i: usize, mac: &[u8; 6]) -> Option<Vec<u8>> {
    let raw = parse_token(token)?;
    if raw.len() != 12 {
        return None; // legacy 只有 12B bind key 口径
    }
    if payload.len() < i + 4 {
        return None; // 至少密文 0B + mic 4B（mic 前 3B 进 nonce）
    }
    let key = expand_legacy_key(&raw);

    let mut nonce = [0u8; 13];
    nonce[..5].copy_from_slice(&payload[..5]); // ctrl2+devid2+cnt1
    nonce[5..8].copy_from_slice(&payload[payload.len() - 4..payload.len() - 1]); // mic 前 3B
    // HA 参考实现的 mac 参数为广播逆序原始字节，mac[::-1][:-1] 等价于
    // **标准序前 5 字节**（与 pycryptodome 对拍验证，见 golden 测试）
    nonce[8..13].copy_from_slice(&mac[..5]);

    let ct = &payload[i..payload.len() - 4]; // 去掉尾部 mic
    Some(ccm_star_keystream(&key, &nonce, ct))
}

/// MiBeacon V4/V5 解密（真机温湿度计 3 mini 实测走这版）：
/// key = token 解出 **16B** 直用；token 仅 12B 时按 legacy magic 扩展试一次
/// （两种候选都以 **MIC 校验** 定夺，HA decrypt_and_verify 同语义）；
/// nonce(12B) = mac 逆序 6B ++ payload[2..5](devid2+cnt1) ++ payload[-7..-4](salt3)；
/// 密文 = payload[i .. len-7]，mic 4B、salt 已入 nonce。
fn decrypt_v45(token: &str, payload: &[u8], i: usize, mac: &[u8; 6]) -> Option<Vec<u8>> {
    let raw = parse_token(token)?;
    let candidates: Vec<[u8; 16]> = match raw.len() {
        16 => vec![raw[..16].try_into().ok()?],
        12 => vec![expand_legacy_key(&raw)],
        _ => return None,
    };
    if payload.len() < i + 3 + 7 {
        return None; // 至少 ct 3B + salt 3B + mic 4B
    }
    let mut nonce = [0u8; 12];
    let mut mac_rev = *mac;
    mac_rev.reverse();
    nonce[..6].copy_from_slice(&mac_rev);
    nonce[6..9].copy_from_slice(&payload[2..5]);
    let n = payload.len();
    nonce[9..12].copy_from_slice(&payload[n - 7..n - 4]);
    let ct = &payload[i..n - 7];
    let mic: [u8; 4] = payload[n - 4..].try_into().ok()?;
    for key in candidates {
        let pt = ccm_star_keystream(&key, &nonce, ct);
        if ccm_star_tag(&key, &nonce, &pt) == mic {
            return Some(pt);
        }
    }
    None
}

/// AES-CCM*（无 MAC 形态）数据段加/解密：keystream 由 B1.. 计数器块生成。
/// pycryptodome decrypt()/encrypt()（HA 所用，不校验 MIC）flags = q-1
/// （q = 15 - len(nonce)，nonce 12B→q=3→flags 0x02；13B→q=2→flags 0x01），
/// **不含** aad/mac_len 位 —— 与 RFC 标准 CCM(m=4) 编码不同，已对拍确认。
fn ccm_star_keystream(key: &[u8], nonce: &[u8], ct: &[u8]) -> Vec<u8> {
    use aes::cipher::{BlockEncrypt, KeyInit};
    let ecb = aes::Aes128::new_from_slice(key).expect("16B key");
    let mut out = ct.to_vec();
    let q = 15 - nonce.len(); // 计数器字段字节宽
    let flags = (q - 1) as u8;
    let mut ctr: u64 = 1; // A0 只产 S0（tag 掩码），数据从 A1 开始
    for chunk in out.chunks_mut(16) {
        let mut block = [0u8; 16];
        block[0] = flags;
        block[1..1 + nonce.len()].copy_from_slice(nonce);
        let cbe = ctr.to_be_bytes();
        block[16 - q..].copy_from_slice(&cbe[8 - q..]);
        ecb.encrypt_block((&mut block).into());
        for (o, k) in chunk.iter_mut().zip(block.iter()) {
            *o ^= k;
        }
        ctr += 1;
    }
    out
}

/// CCM 认证标签 U（m=4，AAD=0x11，RFC 3610 §2.2/2.3）：
/// `X_1=E(B0); X_{i+1}=E(X_i⊕B_i)` 覆盖 B0、aad 块、明文块；`U = X_last[0..4] ⊕ S0[0..4]`。
/// B0 尾部 L 字节存 **l(m)**（消息长度 BE），Flags = 64|8·M'|L'（Adata=1、M'=(4-2)/2=1）。
/// 与 pycryptodome `encrypt_and_digest` 的 digest 字节级一致（v5 golden 测试对拍）。
fn ccm_star_tag(key: &[u8; 16], nonce: &[u8], pt: &[u8]) -> [u8; 4] {
    use aes::cipher::{BlockEncrypt, KeyInit};
    let ecb = aes::Aes128::new_from_slice(key).expect("16B key");
    let l = 15 - nonce.len(); // 长度/计数字段字节宽（q）
    let lp = (l - 1) as u8; // L'
    let mut block = [0u8; 16];
    // B0
    block[0] = 64 | (1 << 3) | lp;
    block[1..1 + nonce.len()].copy_from_slice(nonce);
    let lmbe = (pt.len() as u64).to_be_bytes();
    block[16 - l..].copy_from_slice(&lmbe[8 - l..]);
    let mut x = block; // X_1 = E(B0)
    ecb.encrypt_block((&mut x).into());
    // aad 块：l(a)=0x0001 ++ 0x11 ++ 0
    let mut ba = [0u8; 16];
    ba[0] = 0x00;
    ba[1] = 0x01;
    ba[2] = 0x11;
    for (xi, bi) in x.iter_mut().zip(ba.iter()) {
        *xi ^= bi;
    }
    ecb.encrypt_block((&mut x).into());
    // 明文块（0 填充；空消息无块）
    for chunk in pt.chunks(16) {
        let mut bm = [0u8; 16];
        bm[..chunk.len()].copy_from_slice(chunk);
        for (xi, bi) in x.iter_mut().zip(bm.iter()) {
            *xi ^= bi;
        }
        ecb.encrypt_block((&mut x).into());
    }
    // S0 = E(A0)：Flags = L'，counter 0
    let mut a0 = [0u8; 16];
    a0[0] = lp;
    a0[1..1 + nonce.len()].copy_from_slice(nonce);
    ecb.encrypt_block((&mut a0).into());
    let mut t = [0u8; 4];
    for i in 0..4 {
        t[i] = x[i] ^ a0[i];
    }
    t
}

/// object TLV 序列 → 人话值 map。
/// typecode 布局：低 8 位 = 对象类型；高字节 bit5(0x20) 置位表示事件型（前置 5B 时间戳，
/// 如门磁 0x2007）。MJWSD06MMC 的 0x48xx 系列**不带时间戳**（真机实帧 + HA obj4801
/// struct.unpack("f", 4B) 口径），按精确 tc 先行处理。
fn parse_objects(objs: &[u8]) -> (Map<String, Value>, Vec<String>) {
    let mut values = Map::new();
    let mut unknown: Vec<String> = Vec::new();
    let mut p = objs;
    while p.len() >= 3 {
        let tc = u16::from_le_bytes([p[0], p[1]]);
        let len = p[2] as usize;
        p = &p[3..];
        if p.len() < len {
            break;
        }
        let raw = &p[..len];
        p = &p[len..];
        // MJWSD06MMC 精确 tc（无时间戳）
        match (tc, raw.len()) {
            (0x4801, 4) => {
                let t = f32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]) as f64;
                let t = (t * 10.0).round() / 10.0;
                values.insert("temperature".into(), serde_json::json!(t));
                continue;
            }
            (0x4802, 1) => {
                values.insert("humidity".into(), serde_json::json!(raw[0] as u64));
                continue;
            }
            (0x4803, 1) => {
                values.insert("battery".into(), serde_json::json!(raw[0] as u64));
                continue;
            }
            _ => {}
        }
        let obj_type = (tc & 0xff) as u8;
        let is_event = (tc >> 8) & 0x20 != 0;
        let d = if is_event && raw.len() >= 5 { &raw[5..] } else { raw };
        match obj_type {
            0x01 => {
                // 0x1001 按键等其它形态：档位 A 不消费
            }
            0x03 if d.len() == 1 => {
                values.insert("motion".into(), serde_json::json!(d[0] != 0));
            }
            0x04 if d.len() == 2 => {
                let t = i16::from_le_bytes([d[0], d[1]]) as f64 / 10.0;
                values.insert("temperature".into(), serde_json::json!(t));
            }
            0x06 if d.len() == 2 => {
                let h = u16::from_le_bytes([d[0], d[1]]) as f64 / 10.0;
                values.insert("humidity".into(), serde_json::json!(h));
            }
            0x0d if d.len() == 4 => {
                let t = i16::from_le_bytes([d[0], d[1]]) as f64 / 10.0;
                let h = u16::from_le_bytes([d[2], d[3]]) as f64 / 10.0;
                values.insert("temperature".into(), serde_json::json!(t));
                values.insert("humidity".into(), serde_json::json!(h));
            }
            0x0a if !d.is_empty() => {
                values.insert("battery".into(), serde_json::json!(d[0] as u64));
            }
            0x07 if !d.is_empty() => {
                // 门：0=开 1=关（其余状态忽略）
                match d[0] {
                    0x00 => values.insert("door_closed".into(), serde_json::json!(false)),
                    0x01 => values.insert("door_closed".into(), serde_json::json!(true)),
                    _ => None,
                };
            }
            0x0f | 0x10 | 0x12 | 0x15 | 0x16 => {
                // 照明移动/牙刷/开关类：档位 A 不消费（传感器为主）
            }
            _ => {
                let s = format!("{tc:04x}");
                if !unknown.contains(&s) {
                    unknown.push(s);
                }
            }
        }
    }
    (values, unknown)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 明文帧：v2 + mac + obj(0x1004=25.6℃, 0x1006=43.2%, 0x100a=88)
    #[test]
    fn plain_frame_decodes() {
        let mac: [u8; 6] = [0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc];
        let mut payload = vec![0x52, 0x20, 0x5b, 0x05, 0x07]; // ctrl: v2,mac,obj | devid | cnt
        payload.extend_from_slice(&[mac[5], mac[4], mac[3], mac[2], mac[1], mac[0]]);
        payload.extend_from_slice(&[0x04, 0x10, 0x02, 0x00, 0x01]); // temp int16 256→25.6
        payload.extend_from_slice(&[0x06, 0x10, 0x02, 0xb0, 0x01]); // hum u16 432→43.2
        payload.extend_from_slice(&[0x0a, 0x10, 0x01, 0x58]); // battery 88
        let mut adv = vec![0x02, 0x01, 0x06, 0x03 + payload.len() as u8, 0x16, 0x95, 0xfe];
        adv.extend_from_slice(&payload);

        let mb = decode_adv("", &adv).expect("明文帧应解析");
        assert_eq!(mb.mac, "123456789abc");
        assert_eq!(mb.values["temperature"], serde_json::json!(25.6));
        assert_eq!(mb.values["humidity"], serde_json::json!(43.2));
        assert_eq!(mb.values["battery"], serde_json::json!(88));
        assert!(!mb.encrypted);
    }

    /// legacy 加密往返：用同一 keystream 函数造帧（结构自洽 + nonce/key 扩展断言），
    /// 真机广播再与 HA 参考实现对照校准。
    #[test]
    fn encrypted_roundtrip() {
        let token12: [u8; 12] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
        let token_b64 =
            base64::engine::general_purpose::STANDARD.encode(token12);
        let mut key = [0u8; 16];
        key[..6].copy_from_slice(&token12[..6]);
        key[6..10].copy_from_slice(&MAGIC);
        key[10..].copy_from_slice(&token12[6..]);

        let mac: [u8; 6] = [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff];
        let mut rev = mac;
        rev.reverse();
        let objs: Vec<u8> = vec![0x04, 0x10, 0x02, 0x00, 0x01, 0x06, 0x10, 0x02, 0xb0, 0x01];

        // 组帧：ctrl v2+enc+mac+obj = (2<<12)|(1<<6)|(1<<4)|(1<<3) = 0x2058
        let mut payload = vec![0x58, 0x20, 0x5b, 0x05, 0x09];
        payload.extend_from_slice(&rev);
        // 密文 = keystream XOR objs；mic 4B 任意（前 3B 参与 nonce，最后一字节忽略）
        let mic = [0x11, 0x22, 0x33, 0x44];
        let mut nonce = [0u8; 13];
        nonce[..5].copy_from_slice(&payload[..5]);
        nonce[5..8].copy_from_slice(&[mic[0], mic[1], mic[2]]);
        // 与 decrypt_legacy 同构：mac 标准序前 5B
        nonce[8..13].copy_from_slice(&mac[..5]);
        payload.extend_from_slice(&ccm_star_keystream(&key, &nonce, &objs));
        payload.extend_from_slice(&mic);

        let mut adv = vec![0x02, 0x01, 0x06, 0x03 + payload.len() as u8, 0x16, 0x95, 0xfe];
        adv.extend_from_slice(&payload);

        let mb = decode_adv(&token_b64, &adv).expect("加密帧应解密");
        assert!(mb.encrypted);
        assert_eq!(mb.mac, "aabbccddeeff");
        assert_eq!(mb.values["temperature"], serde_json::json!(25.6));
        assert_eq!(mb.values["humidity"], serde_json::json!(43.2));
    }

    /// 门磁 door 事件（0x2007 事件型，带 5B 时间戳）
    #[test]
    fn door_event_plain() {
        let mac: [u8; 6] = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06];
        let mut payload = vec![0x52, 0x20, 0x89, 0x09, 0x01];
        payload.extend_from_slice(&[mac[5], mac[4], mac[3], mac[2], mac[1], mac[0]]);
        // event door: type 0x2007 len 6: ts(5)+status(1)=1(关)
        payload.extend_from_slice(&[0x07, 0x20, 0x06, 1, 0, 0, 0, 0, 0x01]);
        let mut adv = vec![0x02, 0x01, 0x06, 0x03 + payload.len() as u8, 0x16, 0x95, 0xfe];
        adv.extend_from_slice(&payload);
        let mb = decode_adv("", &adv).expect("门事件");
        assert_eq!(mb.values["door_closed"], serde_json::json!(true));
    }

    #[test]
    fn mesh_and_short_frames_rejected() {
        // mesh bit=1 → None
        let mut payload = vec![0xc2, 0x20, 0x00, 0x00, 0x00, 0, 0, 0, 0, 0, 0];
        payload.extend_from_slice(&[0x04, 0x10, 0x02, 0, 0]);
        let mut adv = vec![0x02, 0x01, 0x06, 0x03 + payload.len() as u8, 0x16, 0x95, 0xfe];
        adv.extend_from_slice(&payload);
        assert!(decode_adv("", &adv).is_none());
        assert!(decode_adv("", &[0x02, 0x01, 0x06]).is_none());
    }

    /// peek_mac：RPA 源地址场景下按包内真实 mac 二次找 token
    #[test]
    fn peek_mac_extracts_payload_mac() {
        // ctrl 0x5958：v5+enc+mac+obj，mac 逆序 f097dd38c1a4 → 标准序 a4c138dd97f0
        let adv = Vec::from_hex(
            "020106191695fe5859b5556ff097dd38c1a4c5a096df150500a58f9797",
        );
        assert_eq!(peek_mac(&adv).as_deref(), Some("a4c138dd97f0"));
        // 无 mac 位（0x5948）→ None
        let adv2 = Vec::from_hex(
            "020106161695fe4859b5556c580208bac35a7f15050029eef6a5",
        );
        assert!(peek_mac(&adv2).is_none());
    }

    /// 互操作黄金向量：PyCryptodome CCM(mac_len=4,aad=0x11) 按 HA ble_monitor
    /// legacy 方案造帧（见 _work 会话脚本），证明与参考实现字节级一致。
    #[test]
    fn golden_vectors_from_pycryptodome() {
        // 加密帧
        let adv = Vec::from_hex(
            "020106201695fe58205b0509ffeeddccbbaab4e805e3cf0b07f3629754bb314a11223344",
        );
        let mb = decode_adv("AQIDBAUGBwgJCgsM", &adv).expect("黄金向量应解密");
        assert_eq!(mb.mac, "aabbccddeeff");
        assert_eq!(mb.values["temperature"], serde_json::json!(25.6));
        assert_eq!(mb.values["humidity"], serde_json::json!(43.2));
        assert_eq!(mb.values["battery"], serde_json::json!(88));

        // 明文帧
        let padv = Vec::from_hex("020106181695fe52205b0502ffeeddccbbaa0410020c01061002f401");
        let pmb = decode_adv("", &padv).expect("明文黄金向量");
        assert_eq!(pmb.values["temperature"], serde_json::json!(26.8));
        assert_eq!(pmb.values["humidity"], serde_json::json!(50.0));
    }

    /// hex 测试辅助（仅 tests 内用）
    trait VecHex {
        fn from_hex(s: &str) -> Vec<u8>;
    }
    impl VecHex for Vec<u8> {
        fn from_hex(s: &str) -> Vec<u8> {
            (0..s.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
                .collect()
        }
    }

    /// V5 golden 向量（`_work/v5_golden.py`，pycryptodome encrypt_and_digest、
    /// HA xiaomi.py 口径）：16B hex token 直用 / 12B hex token legacy 扩展，
    /// 均需过 MIC 并得 26.3℃ / 51.2%（devid 0x55b5 = 真机温湿度计同款）。
    #[test]
    fn golden_vectors_v5() {
        let adv = Vec::from_hex(
            "0201061c1695fe5859b5552af097dd38c1a4c1e84d72ed86c50b527602f3d8d9",
        );
        let mb = decode_adv("00112233445566778899aabbccddeeff", &adv).expect("v5 16B token");
        assert_eq!(mb.mac, "a4c138dd97f0");
        assert_eq!(mb.version, 5);
        assert_eq!(mb.packet_id, 42);
        assert_eq!(mb.values["temperature"], serde_json::json!(26.3));
        assert_eq!(mb.values["humidity"], serde_json::json!(51.2));

        let adv2 = Vec::from_hex(
            "0201061c1695fe5859b5552bf097dd38c1a4ebf825528bc9aa230f177e77fa74",
        );
        let mb2 = decode_adv("0102030405060708090a0b0c", &adv2).expect("v5 12B token 扩展解密");
        assert_eq!(mb2.values["temperature"], serde_json::json!(26.3));
        assert_eq!(mb2.values["humidity"], serde_json::json!(51.2));

        // 错 key：MIC 不过 → None（不得吐垃圾值）
        assert!(decode_adv("ff112233445566778899aabbccddeeff", &adv).is_none());
    }

    /// token 形态自适应：hex(24/32) 与 base64 等价；非法拒绝
    #[test]
    fn token_hex_base64_equivalence() {
        assert_eq!(
            parse_token("0102030405060708090a0b0c").unwrap(),
            parse_token("AQIDBAUGBwgJCgsM").unwrap()
        );
        assert_eq!(parse_token("00112233445566778899aabbccddeeff").unwrap().len(), 16);
        assert!(parse_token("zzzzzzzzzzzzzzzzzzzzzzzz").is_none());
        assert!(parse_token("").is_none());
        assert!(parse_token("AAAAAAAAAAAAAAAAAAAA").is_none()); // b64 解出 15B
    }

    /// 无包内 mac 省电帧（真机 MJWSD06MMC ctrl 0x5148 形态）：known_mac 提供身份
    /// mac 才能组 V5 nonce；对象 0x4801 float 温度 / 0x4802 湿度 / 0x4803 电量（无时间戳）。
    #[test]
    fn v5_nomac_with_known_mac() {
        let adv = Vec::from_hex(
            "0201061e1695fe4851b5557a96a46494a7bf5f5d01bd02e43c76d11505002ea5059f",
        );
        let km = [0xa4u8, 0xc1, 0x38, 0xdd, 0x97, 0xf0];
        // 无 known_mac → nonce 全零 mac，MIC 不过
        assert!(decode_adv("00112233445566778899aabbccddeeff", &adv).is_none());
        let mb = decode_adv_mac("00112233445566778899aabbccddeeff", &adv, Some(&km))
            .expect("nomac 帧 + known_mac 应解密");
        assert_eq!(mb.mac, "a4c138dd97f0");
        assert_eq!(mb.values["temperature"], serde_json::json!(25.1));
        assert_eq!(mb.values["humidity"], serde_json::json!(86));
        assert_eq!(mb.values["battery"], serde_json::json!(100));
    }
}
