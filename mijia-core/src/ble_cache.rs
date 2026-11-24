//! BLE 本地读数快照（档位 A 落库）+ 传感器"本地优先"合并。
//!
//! 数据面：ArkTS 扫到 MiBeacon 广播解码成功后调 [`ingest`]，按包内 MAC 归档
//! 各语义键的最新值与到达时间。消费面：[`apply_local`] 在卡片读值管线里
//! 把"比云端新鲜"的本地值并进 PropValue——比较对象是云端缓存的 update_time，
//! 不是无脑覆盖：哪路数据新用哪路，本地缺失/过期时云端缓存兜底。
//!
//! 语义键（`ble::parse_objects` 产出）→ miot-spec URN 属性名：
//! temperature→temperature / humidity→relative-humidity /
//! battery→battery-level / door_closed→contact-state / motion→motion。
//! 地址 (siid,piid) 从设备 spec 反查，因此温湿度计、门磁等不同型号无需逐型写表。

use crate::ble::MiBeacon;
use crate::card::{CardProp, CardSpec};
use crate::device::PropValue;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// 各语义键的新鲜度上限：超过即不再用本地值兜底（宁可空缺也不拿旧值撒谎）。
/// 门磁是事件+心跳型（约一小时一跳），电量一天一跳，温度计常态 10s~1min 一跳。
const FRESH_MS: &[(&str, i64)] = &[
    ("temperature", 10 * 60_000),
    ("humidity", 10 * 60_000),
    ("motion", 5 * 60_000),
    ("door_closed", 12 * 3_600_000),
    ("battery", 24 * 3_600_000),
];

/// 同一 (mac, packet_id) 在这个窗口内只落库一次。ArkTS 会话内已去重，
/// 这里兜住"重启扫描后旧包重放把 ts 顶新"的边缘情况。
const PACKET_DEDUP_MS: i64 = 60_000;

/// 一台设备的本地快照：语义键 → 最新值 / 到达时间。
/// 跨包合并是刻意设计：温湿度帧、门磁事件帧、电量帧各带各的 object，
/// 各自更新自己的键，拼成一份完整读数。
#[derive(Default)]
struct BleSnap {
    values: Map<String, Value>,
    /// 到达时间用 std HashMap：serde_json::Map 只对 Value 有 Default/全方法
    ts: HashMap<String, i64>,
    packet_id: u8,
    ts_any: i64,
}

/// 全局快照缓存（NAPI 单实例进程，OnceLock 与 card_net::ENGINE 同款静态形态）。
#[derive(Default)]
pub struct BleCache {
    /// 归一化小写 mac（12 hex）→ 快照
    map: Mutex<HashMap<String, BleSnap>>,
}

static CACHE: OnceLock<BleCache> = OnceLock::new();

pub fn global() -> &'static BleCache {
    CACHE.get_or_init(BleCache::default)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn fresh_ttl(key: &str) -> i64 {
    FRESH_MS
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, v)| *v)
        .unwrap_or(0)
}

fn norm_mac(mac: &str) -> String {
    mac.replace(':', "").to_lowercase()
}

impl BleCache {
    pub fn ingest(&self, mb: &MiBeacon) {
        let ts = now_ms();
        let mut map = self.map.lock().unwrap_or_else(|p| p.into_inner());
        let snap = map.entry(mb.mac.clone()).or_default();
        if snap.packet_id == mb.packet_id && ts - snap.ts_any < PACKET_DEDUP_MS {
            return;
        }
        for (k, v) in &mb.values {
            snap.values.insert(k.clone(), v.clone());
            snap.ts.insert(k.clone(), ts);
        }
        snap.packet_id = mb.packet_id;
        snap.ts_any = ts;
    }

    /// 归一化 mac 的只读快照：语义键 → (值, 到达时间 ms)
    fn snapshot(&self, mac_norm: &str) -> Option<(Map<String, Value>, HashMap<String, i64>)> {
        let map = self.map.lock().unwrap_or_else(|p| p.into_inner());
        map.get(mac_norm).map(|s| (s.values.clone(), s.ts.clone()))
    }
}

/// 落库入口（NAPI 桥转发）：`beacon_json` 是 [`crate::ble::MiBeacon`] 的 JSON 文本。
/// 解析失败（非 MiBeacon JSON）返回 false。
pub fn ingest(beacon_json: &str) -> bool {
    let mb: MiBeacon = match serde_json::from_str(beacon_json) {
        Ok(v) => v,
        Err(_) => return false,
    };
    global().ingest(&mb);
    true
}

/// 语义键 → spec 属性 URN 名候选（按序匹配）。
fn spec_names(key: &str) -> &'static [&'static str] {
    match key {
        "temperature" => &["temperature"],
        "humidity" => &["relative-humidity", "humidity"],
        "battery" => &["battery-level", "battery"],
        "door_closed" => &["contact-state"],
        "motion" => &["motion", "motion-state"],
        _ => &[],
    }
}

fn spec_prop<'a>(spec: &'a CardSpec, siid: i64, piid: i64) -> Option<&'a CardProp> {
    spec.services
        .iter()
        .find(|s| s.iid == siid)
        .and_then(|s| s.props.iter().find(|p| p.iid == piid && !p.is_action))
}

/// 本地语义值 → spec 形态：contact-state 按 spec format 决定 bool 还是 0/1；
/// 其余键（float/uint）直接透传（evaluate/formatItemValue 已按 spec 渲染）。
fn local_value(key: &str, v: &Value, format: &str) -> Value {
    if key == "door_closed" {
        let closed = v.as_bool().unwrap_or(false);
        if format == "bool" {
            return Value::Bool(closed);
        }
        return serde_json::json!(if closed { 1 } else { 0 });
    }
    v.clone()
}

/// 把"本地新鲜"的读数并进 props（card_items/card_detail 读值管线钩子）。
///
/// 判定规则（每条 ref 独立，**本地新鲜即优先**）：
/// - 本地值缺失 / 超过该键 TTL → 不参与（云端缓存或空缺照旧）；
/// - 云端有 update_time（miotspec 缓存读，含网关上报）→ **本地赢**：广播是手机
///   自己收的实时值，网关绕云一圈再快也多一跳；且云 update_time 是服务器时钟，
///   与手机时钟有偏差，「谁新用谁」在网关常报场景会永远选云，本地形同虚设；
/// - 云端无 update_time（rpcv2 实时读）→ 例外：这是控制下发后的确认回读，
///   云端就是当前值，本地不覆盖；
/// - 云端 code!=0（读失败项）/ 整次读失败（离线）→ 本地兜底。
///
/// 返回 (合并后的 props, 本地来源的 (siid,piid) 集合——调用方据此给 DTO 打来源标)。
pub fn apply_local(
    mac: Option<&str>,
    did: &str,
    spec: &CardSpec,
    refs: &[(i64, i64)],
    mut props: Vec<PropValue>,
) -> (Vec<PropValue>, Vec<(i64, i64)>) {
    let mac_norm = match mac {
        Some(m) if !m.is_empty() => norm_mac(m),
        _ => return (props, Vec::new()),
    };
    let (values, ts) = match global().snapshot(&mac_norm) {
        Some(v) => v,
        None => return (props, Vec::new()),
    };
    if values.is_empty() {
        return (props, Vec::new());
    }
    let now = now_ms();
    let mut local: Vec<(i64, i64)> = Vec::new();
    for r in refs {
        let (siid, piid) = *r;
        let p = match spec_prop(spec, siid, piid) {
            Some(p) => p,
            None => continue,
        };
        // spec 属性名反查语义键（候选名按序，命中第一个即认）
        let key = match values
            .keys()
            .find(|k| spec_names(k).contains(&p.name.as_str()))
        {
            Some(k) => k.clone(),
            None => continue,
        };
        let t = ts.get(&key).copied().unwrap_or(0);
        if t <= 0 || now - t > fresh_ttl(&key) {
            continue;
        }
        let cloud_realtime = match props.iter().find(|v| v.siid == siid && v.piid == piid) {
            // 云端没这条值（离线整读失败/模板没这属性）→ 本地兜底
            None => false,
            Some(c) => {
                if c.code != 0 {
                    false // 云端读失败项：本地兜底（evaluate 对 code!=0 是隐藏/标记）
                } else {
                    // 缓存读（有 update_time）→ 本地新鲜即赢；rpcv2 实时读 → 云端即当前值
                    c.update_time.is_none()
                }
            }
        };
        if cloud_realtime {
            continue;
        }
        let value = local_value(&key, values.get(&key).unwrap_or(&Value::Null), p.format.as_str());
        let pv = PropValue {
            did: did.to_string(),
            siid,
            piid,
            code: 0,
            value,
            update_time: Some(t / 1000),
        };
        match props.iter().position(|v| v.siid == siid && v.piid == piid) {
            Some(i) => props[i] = pv,
            None => props.push(pv),
        }
        if !local.contains(r) {
            local.push(*r);
        }
    }
    (props, local)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 迷你 spec：温度/湿度/电量/门磁四个属性（地址随意但固定，测映射与合并）
    fn mini_spec() -> CardSpec {
        let v: Value = json!({
            "services": [
                { "iid": 1, "type": "urn:miot-spec-v2:service:device-information:00000001",
                  "properties": [
                    { "iid": 1, "type": "urn:miot-spec-v2:property:manufacturer:00000001",
                      "format": "string", "access": ["read"] }
                  ] },
                { "iid": 2, "type": "urn:miot-spec-v2:service:environment:00000008",
                  "properties": [
                    { "iid": 1, "type": "urn:miot-spec-v2:property:temperature:0000002E",
                      "format": "float", "access": ["read"] },
                    { "iid": 2, "type": "urn:miot-spec-v2:property:relative-humidity:0000000C",
                      "format": "float", "access": ["read"] }
                  ] },
                { "iid": 3, "type": "urn:miot-spec-v2:service:battery:0000000D",
                  "properties": [
                    { "iid": 1, "type": "urn:miot-spec-v2:property:battery-level:0000000F",
                      "format": "uint8", "access": ["read"] }
                  ] },
                { "iid": 4, "type": "urn:miot-spec-v2:service:contact:00000016",
                  "properties": [
                    { "iid": 2, "type": "urn:miot-spec-v2:property:contact-state:00000030",
                      "format": "bool", "access": ["read"] }
                  ] }
            ]
        });
        CardSpec::from_instance(&v)
    }

    fn pv(siid: i64, piid: i64, value: Value, update_time: Option<i64>) -> PropValue {
        PropValue {
            did: "blt.3.1test".into(),
            siid,
            piid,
            code: 0,
            value,
            update_time,
        }
    }

    /// 落一条快照：mac 小写归一 + 全语义键，ts 可控（往回拨秒数）
    fn seed_snap(mac: &str, secs_ago: i64) {
        let mb = MiBeacon {
            mac: mac.to_string(),
            device_id: 0x055b,
            version: 5,
            packet_id: 7,
            encrypted: true,
            values: {
                let mut m = Map::new();
                m.insert("temperature".into(), json!(25.6));
                m.insert("humidity".into(), json!(43.2));
                m.insert("battery".into(), json!(88));
                m.insert("door_closed".into(), json!(false));
                m
            },
            unknown: Vec::new(),
        };
        global().ingest(&mb);
        if secs_ago > 0 {
            // 把 ts 往回拨，模拟"本地数据已老化"
            let mut map = global().map.lock().unwrap_or_else(|p| p.into_inner());
            let snap = map.get_mut(mac).unwrap();
            for (_, t) in snap.ts.iter_mut() {
                *t -= secs_ago * 1000;
            }
            snap.ts_any -= secs_ago * 1000;
        }
    }

    /// 并行测试共享全局缓存：每个测试用独立 mac，避免去重窗口与 ts 回拨互踩。
    fn mac(n: u8) -> String {
        format!("aabbcc0000{n:02x}")
    }

    #[test]
    fn local_fills_when_cloud_empty() {
        let mac = mac(1);
        seed_snap(&mac, 0);
        let spec = mini_spec();
        let refs = vec![(2, 1), (2, 2), (3, 1)];
        let (props, local) = apply_local(Some(&mac), "blt.x", &spec, &refs, Vec::new());
        assert_eq!(props.len(), 3);
        assert_eq!(props[0].value, json!(25.6));
        assert_eq!(props[1].value, json!(43.2));
        assert_eq!(props[2].value, json!(88));
        assert_eq!(local, vec![(2, 1), (2, 2), (3, 1)]);
    }

    #[test]
    fn local_wins_when_newer_than_cloud() {
        let mac = mac(2);
        seed_snap(&mac, 0);
        let spec = mini_spec();
        let refs = vec![(2, 1)];
        // 云端缓存 120s 前上报
        let cloud = vec![pv(2, 1, json!(24.0), Some(now_ms() / 1000 - 120))];
        let (props, local) = apply_local(Some(&mac), "blt.x", &spec, &refs, cloud);
        assert_eq!(props.len(), 1);
        assert_eq!(props[0].value, json!(25.6));
        assert_eq!(local, vec![(2, 1)]);
    }

    /// 本地新鲜（TTL 内）⇒ 直接赢云缓存——哪怕云 update_time 更新（网关常报 +
    /// 云服务器时钟偏差会让"谁新用谁"永远选云，违背优先本地的产品语义）
    #[test]
    fn fresh_local_wins_over_newer_cloud_cache() {
        let mac = mac(3);
        // 本地 9 分钟前（温度 TTL 10 分钟内仍新鲜）。不 seed 整 600s：
        // 断言执行时 now-t 还会再涨几 ms，恰踩 TTL 边界会偶发判过期
        seed_snap(&mac, 540);
        let spec = mini_spec();
        let refs = vec![(2, 1)];
        let cloud = vec![pv(2, 1, json!(24.0), Some(now_ms() / 1000))];
        let (props, local) = apply_local(Some(&mac), "blt.x", &spec, &refs, cloud);
        assert_eq!(local, vec![(2, 1)]);
        assert_eq!(props[0].value, json!(25.6), "本地新鲜应覆盖云缓存");
        assert!(props[0].update_time.is_some());
    }

    /// 本地过期（超 TTL）→ 云缓存原样保留（不拿离开范围前的广播旧值撒谎）
    #[test]
    fn stale_local_falls_back_to_cloud() {
        let mac = mac(9);
        seed_snap(&mac, 11 * 60); // 温度 TTL 10 分钟已过
        let spec = mini_spec();
        let refs = vec![(2, 1)];
        let cloud = vec![pv(2, 1, json!(24.0), Some(now_ms() / 1000))];
        let (props, local) = apply_local(Some(&mac), "blt.x", &spec, &refs, cloud);
        assert!(local.is_empty(), "过期本地值不参与");
        assert_eq!(props[0].value, json!(24.0), "云缓存原样保留");
    }

    #[test]
    fn realtime_cloud_read_wins() {
        let mac = mac(4);
        seed_snap(&mac, 0);
        let spec = mini_spec();
        let refs = vec![(2, 1)];
        // rpcv2 实时读：无 update_time，云端即当前值
        let cloud = vec![pv(2, 1, json!(24.0), None)];
        let (props, local) = apply_local(Some(&mac), "blt.x", &spec, &refs, cloud);
        assert_eq!(props[0].value, json!(24.0));
        assert!(local.is_empty());
    }

    #[test]
    fn stale_local_never_fills() {
        let mac = mac(5);
        seed_snap(&mac, 11 * 60); // 温度 TTL 10 分钟已过
        let spec = mini_spec();
        let refs = vec![(2, 1)];
        let (props, local) = apply_local(Some(&mac), "blt.x", &spec, &refs, Vec::new());
        assert!(props.is_empty());
        assert!(local.is_empty());
    }

    #[test]
    fn contact_state_bool_conversion() {
        let mac = mac(6);
        seed_snap(&mac, 0);
        let spec = mini_spec();
        let refs = vec![(4, 2)];
        let (props, _) = apply_local(Some(&mac), "blt.x", &spec, &refs, Vec::new());
        assert_eq!(props[0].value, json!(false)); // format=bool → Bool
    }

    #[test]
    fn cloud_code_failure_is_backfilled() {
        let mac = mac(7);
        seed_snap(&mac, 0);
        let spec = mini_spec();
        let refs = vec![(3, 1)];
        let mut cloud = vec![pv(3, 1, json!(null), None)];
        cloud[0].code = -4001;
        let (props, local) = apply_local(Some(&mac), "blt.x", &spec, &refs, cloud);
        assert_eq!(props[0].value, json!(88));
        assert_eq!(props[0].code, 0);
        assert_eq!(local, vec![(3, 1)]);
    }

    #[test]
    fn no_mac_or_unknown_mac_is_noop() {
        let mac = mac(8);
        seed_snap(&mac, 0);
        let spec = mini_spec();
        let refs = vec![(2, 1)];
        for mac in [None, Some(""), Some("000000000000")] {
            let (props, local) = apply_local(mac, "blt.x", &spec, &refs, Vec::new());
            assert!(props.is_empty());
            assert!(local.is_empty());
        }
    }

    #[test]
    fn ingest_parses_beacon_json_and_merges_keys() {
        // 模拟两帧：一帧温+湿（0x0d 形态），一帧只有电量
        let a = json!({
            "mac": "1122334455aa", "device_id": 1, "version": 5, "packet_id": 1,
            "encrypted": true,
            "values": { "temperature": 21.5, "humidity": 50.0 }, "unknown": []
        });
        let b = json!({
            "mac": "1122334455aa", "device_id": 1, "version": 5, "packet_id": 2,
            "encrypted": true, "values": { "battery": 66 }, "unknown": []
        });
        assert!(ingest(&a.to_string()));
        assert!(ingest(&b.to_string()));
        let spec = mini_spec();
        let refs = vec![(2, 1), (2, 2), (3, 1)];
        let (props, local) = apply_local(Some("11:22:33:44:55:AA"), "blt.x", &spec, &refs, Vec::new());
        assert_eq!(props.len(), 3); // 跨包合并：三键齐活
        assert_eq!(props[0].value, json!(21.5));
        assert_eq!(props[2].value, json!(66));
        assert_eq!(local.len(), 3);
    }

    #[test]
    fn ingest_rejects_garbage() {
        assert!(!ingest("not json"));
        assert!(!ingest("{}"));
    }
}
