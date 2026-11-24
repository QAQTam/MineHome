//! 家庭 / 房间归属（只读）。
//!
//! 端点从反编译的官方 App 里翻出（见 `_work/frontend_ui_notes.md` 第七节）：
//! - `POST /v2/homeroom/gethome` → `result.homelist[]`：`{id,name,dids[],roomlist[{id,name,icon,dids[]}]}`
//!   （带 `has_more` / `max_id`，按家庭分页）
//! - `POST /v2/homeroom/get_dev_room_page` → `result.info[]`：`{id,dids[],roomlist[{id,dids[]}]}`
//!   （**`limit` 必填**，缺失报 `code=-4001001 invalid limit`；`max_id` 是 did 游标）
//!
//! 实测否掉的旧路径（`api.io.mi.com/app` 上 404）：`/user/api/app/v3/homeroom/gethome`、
//! `/user/api/app/v1/homeroom/gethome`、`/mgr/homeprop`(get_homeinfo)、`/v2/home/home_list_page`。
//!
//! 归属信息是**装饰性**的：拿不到就返回空 map，上层按"不分组"渲染，绝不因它失败而丢设备列表。

use crate::cloud::CloudClient;
use crate::error::{Error, Result};
use crate::session::Session;
use serde_json::{json, Value};
use std::collections::HashMap;

/// 归一化后的单台设备房间归属（ArkTS 只见这个，不见协议字段）。
#[derive(Debug, Clone)]
pub struct RoomRef {
    /// 家庭名（多家庭账号区分用）
    pub home_name: String,
    /// 房间名
    pub room_name: String,
    /// 云端返回顺序推导出的分组序号：`家庭序号 * 1000 + 房间序号`。
    /// 上层按它排序分组，未分组的设备为 `None`，排最后。
    pub rank: i64,
}

pub struct RoomClient {
    cloud: CloudClient,
}

impl RoomClient {
    pub fn new(cloud: CloudClient) -> Self {
        Self { cloud }
    }

    /// did → 房间归属。`gethome` 是第一页必须成功的端点；`get_dev_room_page` 只用于
    /// 补分页成员，失败一律降级。
    pub async fn device_rooms(&self, session: &Session) -> Result<HashMap<String, RoomRef>> {
        // room_id → 归属（含空房间）：get_dev_room_page 只给 id，靠它反查房间名
        let mut rooms: HashMap<String, RoomRef> = HashMap::new();
        let mut acc: HashMap<String, RoomRef> = HashMap::new();
        let mut any = false;

        // 1) gethome：房间名 + 家庭/房间成员（按 max_id 翻页，最多 5 页）
        let mut cursor = String::new();
        for page in 0..5 {
            let body = if cursor.is_empty() {
                json!({})
            } else {
                json!({ "max_id": cursor })
            };
            let resp = self
                .cloud
                .post_signed(session, "/v2/homeroom/gethome", &body)
                .await;
            match resp {
                Ok(v) => {
                    check_ok(&v)?;
                    let got = parse_gethome(&v);
                    acc.extend(got.dids);
                    rooms.extend(got.rooms);
                    any = true;
                    if !got.has_more || got.max_id.is_empty() {
                        break;
                    }
                    cursor = got.max_id;
                }
                Err(e) if page == 0 => return Err(e),
                Err(_) => break,
            }
        }

        // 2) get_dev_room_page：按 did 分页的成员表（补 gethome 可能被截断的 dids）。
        //    失败只降级，不影响已经拿到的归属。
        //    注：翻页游标参数名 `max_id` 是按响应字段推的，本账号只 1 页（has_more=false），
        //    **未实测**；参数不对时服务端报错 → 这里直接 break，不影响第一页结果。
        let mut did_cursor = String::new();
        for _ in 0..10 {
            let body = if did_cursor.is_empty() {
                json!({ "limit": "300" })
            } else {
                json!({ "limit": "300", "max_id": did_cursor })
            };
            let resp = self
                .cloud
                .post_signed(session, "/v2/homeroom/get_dev_room_page", &body)
                .await;
            let v = match resp {
                Ok(v) => v,
                Err(_) => break,
            };
            if check_ok(&v).is_err() {
                break;
            }
            let (more, next) = merge_dev_room_page(&v, &rooms, &mut acc);
            any = true;
            if !more || next.is_empty() {
                break;
            }
            did_cursor = next;
        }

        if any {
            Ok(acc)
        } else {
            Err(Error::Protocol("房间归属为空".into()))
        }
    }
}

// ------------------------------------------------------------------ 解析 ----

fn code_of(v: &Value) -> Option<i64> {
    v.get("code").and_then(|c| c.as_i64())
}

fn check_ok(v: &Value) -> Result<()> {
    match code_of(v) {
        Some(0) | None => Ok(()),
        Some(code) => Err(Error::Cloud {
            code,
            message: v
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or_default()
                .to_string(),
        }),
    }
}

/// 数字/字符串混合的 id、did 列表统一成 String（gethome 给字符串，
/// get_dev_room_page 给数字，实测两边 id 类型不一致）。
fn id_str(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn id_list(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| a.iter().filter_map(id_str).collect())
        .unwrap_or_default()
}

/// `gethome` 的解析产物。
#[derive(Debug, Default)]
pub struct GetHome {
    /// did → 归属。**只含被放进某个房间的设备**；只在 `home.dids` 里而没有房间的
    /// 设备不进这张表（上层视为「未分组」）。
    pub dids: HashMap<String, RoomRef>,
    /// room_id → 归属（含空房间），供 `get_dev_room_page` 用真实 id 反查房间名。
    pub rooms: HashMap<String, RoomRef>,
    pub has_more: bool,
    pub max_id: String,
}

pub fn parse_gethome(resp: &Value) -> GetHome {
    let mut out = GetHome::default();
    let homes = resp
        .pointer("/result/homelist")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    for (hi, home) in homes.iter().enumerate() {
        let home_name = home
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if let Some(rooms) = home.get("roomlist").and_then(|v| v.as_array()) {
            for (ri, room) in rooms.iter().enumerate() {
                let room_name = room
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if room_name.is_empty() {
                    continue;
                }
                let rank = (hi as i64) * 1000 + ri as i64;
                let rf = RoomRef {
                    home_name: home_name.clone(),
                    room_name: room_name.clone(),
                    rank,
                };
                if let Some(rid) = room.get("id").and_then(id_str) {
                    out.rooms.insert(rid, rf.clone());
                }
                for did in id_list(room.get("dids").unwrap_or(&Value::Null)) {
                    out.dids.insert(did, rf.clone());
                }
            }
        }
    }
    out.has_more = resp
        .pointer("/result/has_more")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    out.max_id = resp
        .pointer("/result/max_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    out
}

/// `get_dev_room_page` 的成员表并到已有归属里。本页的 roomlist **只有 id 和 dids、
/// 没有房间名**，所以用 `room_index`（gethome 的 room_id → 归属）按真实 id 反查房间名；
/// id 对不上（陌生房间）就跳过。已在表里的 did 不改写。返回 (has_more, max_id)。
pub fn merge_dev_room_page(
    resp: &Value,
    room_index: &HashMap<String, RoomRef>,
    acc: &mut HashMap<String, RoomRef>,
) -> (bool, String) {
    let info = resp
        .pointer("/result/info")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    for home in &info {
        let Some(roomlist) = home.get("roomlist").and_then(|v| v.as_array()) else {
            continue;
        };
        for room in roomlist {
            let Some(rid) = room.get("id").and_then(id_str) else {
                continue;
            };
            let Some(t) = room_index.get(&rid) else { continue };
            for did in id_list(room.get("dids").unwrap_or(&Value::Null)) {
                acc.entry(did).or_insert_with(|| t.clone());
            }
        }
    }
    let more = resp
        .pointer("/result/has_more")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let max_id = resp
        .pointer("/result/max_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    (more, max_id)
}

// ------------------------------------------------------------------ 测试 ----

#[cfg(test)]
mod tests {
    use super::*;

    const GETHOME: &str = r#"{
      "code": 0, "message": "",
      "result": {
        "homelist": [
          { "id": "458001595286", "name": "新家",
            "dids": ["900000001", "900000003"],
            "temp_dids": null,
            "roomlist": [
              { "id": "458001943684", "name": "卧室", "icon": "bedroom_1",
                "dids": ["router-synthetic-0001"] },
              { "id": "458001943685", "name": "客厅", "dids": null }
            ] },
          { "id": "458001595287", "name": "老家",
            "dids": [], "roomlist": [ { "id": "1", "name": "书房", "dids": ["900000002"] } ] }
        ],
        "has_more": false, "max_id": ""
      }
    }"#;

    const DEV_ROOM_PAGE: &str = r#"{
      "code": 0, "message": "",
      "result": {
        "info": [ { "id": 458001595286,
                     "dids": ["900000001"],
                     "roomlist": [ { "id": 458001943684,
                                      "dids": ["router-synthetic-0001",
                                               "900000010"] } ] } ],
        "max_id": "router-synthetic-0001",
        "has_more": false
      }
    }"#;

    #[test]
    fn gethome_给出房间归属与序号() {
        let v: Value = serde_json::from_str(GETHOME).unwrap();
        let got = parse_gethome(&v);
        assert!(!got.has_more);
        assert_eq!(got.max_id, "");
        let map = &got.dids;
        // 只在 home.dids 里、没有房间的设备 → 不进表（上层视为未分组）
        assert!(!map.contains_key("900000001"));
        assert!(!map.contains_key("900000003"));
        let bedroom = map.get("router-synthetic-0001").unwrap();
        assert_eq!(bedroom.room_name, "卧室");
        assert_eq!(bedroom.home_name, "新家");
        assert_eq!(bedroom.rank, 0);
        // 第二个房间的 dids 为 null 不应 panic
        assert!(!map.values().any(|r| r.room_name == "客厅"));
        // 第二个家庭里的房间：rank = 1*1000 + 0
        assert_eq!(map.get("900000002").unwrap().rank, 1000);
    }

    #[test]
    fn dev_room_page_按真实房间id补齐陌生设备() {
        let g: Value = serde_json::from_str(GETHOME).unwrap();
        let got = parse_gethome(&g);
        let rooms = &got.rooms;
        let mut acc = got.dids;
        let p: Value = serde_json::from_str(DEV_ROOM_PAGE).unwrap();
        let (more, next) = merge_dev_room_page(&p, rooms, &mut acc);
        assert!(!more);
        assert_eq!(next, "router-synthetic-0001");
        // 本页新出现的 did 靠真实 room id 反查名字（本用例里 gethome 侧是字符串 id、
        // 本页是数字 id，id_str 归一化后必须对上）
        assert_eq!(acc.get("900000010").unwrap().room_name, "卧室");
        // 已在表里的不被改写
        assert!(acc.get("900000003").is_none());
        // 空房间（客厅，dids=null）也进了 rooms，名字可被反查
        assert!(rooms.contains_key("458001943685"));
    }

    #[test]
    fn 错误码原样上抛() {
        let v: Value = serde_json::from_str(r#"{"code":-4001001,"message":"invalid limit"}"#)
            .unwrap();
        let e = check_ok(&v).unwrap_err();
        match e {
            Error::Cloud { code, message } => {
                assert_eq!(code, -4001001);
                assert_eq!(message, "invalid limit");
            }
            other => panic!("期望 Cloud，得到 {other:?}"),
        }
    }
}
