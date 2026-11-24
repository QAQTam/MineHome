//! 设备服务：列表缓存 / spec 属性读写 / 老 miIO rpc。
//! 编排逻辑在 [`DeviceClient`]，协议签名细节由 [`crate::cloud::CloudClient`] 封闭。

use crate::cloud::CloudClient;
use crate::error::{Error, Result};
use crate::session::Session;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};

/// 设计文档 §2 的领域模型（ArkTS 只见这个，不见协议字段）。
/// `token` 为 device_list_page 返回的设备 token（报告 §8：二期局域网直连用）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Device {
    pub did: String,
    pub model: String,
    pub name: String,
    pub online: bool,
    /// spec_type URN（device_list_page 只下发类型 URN；明细走 miot-spec.org）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spec_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spec: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// BLE MAC（"AA:BB:..." 或数字型；蓝牙设备本地广播收数用，档位 A）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mac: Option<String>,
    /// 房间名（`room` 模块归一化后的云端归属；未分配房间 / 拉取失败 = None）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub room: Option<String>,
    /// 分组序号：`家庭序号 * 1000 + 房间序号`（云端返回顺序，供上层稳定分组）。
    /// None = 未分组，排最后。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub room_rank: Option<i64>,
}

/// spec 属性读值（get_properties 单项）
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PropValue {
    pub did: String,
    pub siid: i64,
    pub piid: i64,
    #[serde(default)]
    pub code: i64,
    pub value: Value,
    /// 云端缓存更新时间（/miotspec/prop/get 返回；rpcv2 实时读无此值）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update_time: Option<i64>,
}

pub struct DeviceClient {
    pub cloud: CloudClient,
    next_id: AtomicU64,
}

impl DeviceClient {
    pub fn new(cloud: CloudClient) -> Self {
        Self {
            cloud,
            next_id: AtomicU64::new(1),
        }
    }

    fn rpc_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// 分页设备列表（当前主用端点，含 spec/plugin 配置）。
    pub async fn list_devices(&self, session: &Session) -> Result<Vec<Device>> {
        let body = json!({
            "start_did": "0",
            "limit": "200",
            "get_spec": true,
            "get_cariot_device": true,
            "dids": [],
        });
        let resp = self
            .cloud
            .post_signed(session, "/v2/home/device_list_page", &body)
            .await?;
        check_code(&resp)?;
        let list = resp
            .pointer("/result/list")
            .and_then(|v| v.as_array())
            .ok_or_else(|| Error::Protocol("device_list_page 响应缺 result.list".into()))?;
        Ok(list.iter().filter_map(parse_device).collect())
    }

    /// 取 BLE（blt.*）设备 MiBeacon 被动广播加密 key（32 hex = 16B bindkey）。
    /// 端点 `/v2/device/blt_get_beaconkey`（与 PiotrMachowski/Xiaomi-cloud-tokens-extractor
    /// 同口径）；非 BLE/无 key 设备返回 None。device_list 的 24-hex token **解不开**
    /// V5 广播（实测），档位 A 必须用本 key。
    pub async fn beaconkey(&self, session: &Session, did: &str) -> Result<Option<String>> {
        let body = json!({ "did": did, "pdid": 1 });
        let resp = self
            .cloud
            .post_signed(session, "/v2/device/blt_get_beaconkey", &body)
            .await?;
        let key = resp
            .pointer("/result/beaconkey")
            .or_else(|| resp.pointer("/result/key"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        Ok(key)
    }

    /// spec 属性读取（米家统一通道 `/miotspec/prop/get`）。
    /// WiFi/zigbee/BLE mesh/BLE 直连全覆盖：云端返回网关上报的缓存值
    /// （带 updateTime），BLE 设备无 rpcv2 实时穿透通道，米家卡片同读此缓存。
    pub async fn spec_get_properties(
        &self,
        session: &Session,
        did: &str,
        props: &[(i64, i64)],
    ) -> Result<Vec<PropValue>> {
        let params: Vec<Value> = props
            .iter()
            .map(|(siid, piid)| json!({"did": did, "siid": siid, "piid": piid}))
            .collect();
        let body = json!({ "params": params });
        let resp = self.cloud.post_signed(session, "/miotspec/prop/get", &body).await?;
        check_code(&resp)?;
        let arr = resp
            .get("result")
            .and_then(|v| v.as_array())
            .ok_or_else(|| Error::Protocol("miotspec prop/get 响应缺 result 数组".into()))?;
        Ok(arr
            .iter()
            .map(|item| PropValue {
                did: item.get("did").map(value_to_string).unwrap_or_else(|| did.to_string()),
                siid: item.get("siid").and_then(|v| v.as_i64()).unwrap_or(0),
                piid: item.get("piid").and_then(|v| v.as_i64()).unwrap_or(0),
                code: item.get("code").and_then(|v| v.as_i64()).unwrap_or(0),
                value: item.get("value").cloned().unwrap_or(Value::Null),
                update_time: item.get("updateTime").and_then(|v| v.as_i64()),
            })
            .collect())
    }

    /// spec 属性读取：`/home/rpcv2/<did>` get_properties（WiFi 设备实时穿透）
    pub async fn get_properties(
        &self,
        session: &Session,
        did: &str,
        props: &[(i64, i64)],
    ) -> Result<Vec<PropValue>> {
        let params: Vec<Value> = props
            .iter()
            .map(|(siid, piid)| json!({"did": did, "siid": siid, "piid": piid}))
            .collect();
        let body = json!({
            "id": self.rpc_id(),
            "method": "get_properties",
            "params": params,
        });
        let resp = self
            .cloud
            .post_signed(session, &format!("/home/rpcv2/{did}"), &body)
            .await?;
        check_code(&resp)?;
        let arr = resp
            .get("result")
            .and_then(|v| v.as_array())
            .ok_or_else(|| Error::Protocol("get_properties 响应缺 result 数组".into()))?;
        Ok(arr
            .iter()
            .map(|item| PropValue {
                did: item
                    .get("did")
                    .map(value_to_string)
                    .unwrap_or_else(|| did.to_string()),
                siid: item.get("siid").and_then(|v| v.as_i64()).unwrap_or(0),
                piid: item.get("piid").and_then(|v| v.as_i64()).unwrap_or(0),
                code: item.get("code").and_then(|v| v.as_i64()).unwrap_or(0),
                value: item.get("value").cloned().unwrap_or(Value::Null),
                update_time: None,
            })
            .collect())
    }

    /// spec 属性写入：`/home/rpcv2/<did>` set_properties（实测开关控制通过）
    pub async fn set_properties(
        &self,
        session: &Session,
        did: &str,
        props: &[(i64, i64, Value)],
    ) -> Result<()> {
        let params: Vec<Value> = props
            .iter()
            .map(|(siid, piid, value)| {
                json!({"did": did, "siid": siid, "piid": piid, "value": value})
            })
            .collect();
        let body = json!({
            "id": self.rpc_id(),
            "method": "set_properties",
            "params": params,
        });
        let resp = self
            .cloud
            .post_signed(session, &format!("/home/rpcv2/{did}"), &body)
            .await?;
        check_code(&resp)?;
        // 逐项校验（实测语义）：0=成功；1=指令已受理（mesh/网关异步上报，
        // 同值回写也报 1，需延迟回读验证）；负值（如 -5000）=设备拒绝。
        if let Some(arr) = resp.get("result").and_then(|v| v.as_array()) {
            check_set_items(arr)?;
        }
        // result=0 且 ret 各项为 "ok"/0 视为成功；设备可返回逐项错误码
        if let Some(ret) = resp.get("ret").and_then(|v| v.as_array()) {
            for r in ret {
                let bad = match r {
                    Value::Number(n) => n.as_i64().map(|c| c != 0).unwrap_or(false),
                    Value::String(s) => s == "error" || s == "fail",
                    _ => false,
                };
                if bad {
                    return Err(Error::Cloud {
                        code: -1,
                        message: format!("set_properties ret={r}"),
                    });
                }
            }
        }
        Ok(())
    }

    /// spec 属性写入（统一云控通道）：`/miotspec/prop/set`。
    /// 实测覆盖 BLE mesh（rpcv2 对 blt.* 报 "does not support sending rpc"）；
    /// 单项 code 语义与 rpcv2 一致。
    pub async fn spec_set_properties(
        &self,
        session: &Session,
        did: &str,
        props: &[(i64, i64, Value)],
    ) -> Result<()> {
        let params: Vec<Value> = props
            .iter()
            .map(|(siid, piid, value)| {
                json!({"did": did, "siid": siid, "piid": piid, "value": value})
            })
            .collect();
        let body = json!({ "params": params });
        let resp = self
            .cloud
            .post_signed(session, "/miotspec/prop/set", &body)
            .await?;
        check_code(&resp)?;
        if let Some(arr) = resp.get("result").and_then(|v| v.as_array()) {
            check_set_items(arr)?;
        }
        Ok(())
    }

    /// 执行 spec 动作：`/miotspec/action`（APK DeviceCardApi$SpecActionApi 同源；
    /// params 为**单对象** {did,siid,aiid,in}，result 为对象 {code,value}）。
    pub async fn call_action(
        &self,
        session: &Session,
        did: &str,
        siid: i64,
        iid: i64,
        in_params: &[Value],
    ) -> Result<Value> {
        let body = json!({
            "params": {"did": did, "siid": siid, "aiid": iid, "in": in_params}
        });
        let resp = self
            .cloud
            .post_signed(session, "/miotspec/action", &body)
            .await?;
        check_code(&resp)?;
        let result = resp.get("result").cloned().unwrap_or(Value::Null);
        let code = result.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
        if code != 0 {
            return Err(Error::Cloud {
                code,
                message: format!("action siid={siid} aiid={iid} code={code}"),
            });
        }
        Ok(result.get("value").cloned().unwrap_or(Value::Null))
    }

    /// 老 miIO 协议：`/home/rpc/<did>`（非 spec 设备，实测 miIO.info 通过）
    pub async fn call_miio(
        &self,
        session: &Session,
        did: &str,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        let body = json!({
            "id": self.rpc_id(),
            "method": method,
            "params": params,
        });
        let resp = self
            .cloud
            .post_signed(session, &format!("/home/rpc/{did}"), &body)
            .await?;
        check_code(&resp)?;
        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
    }
}

fn check_code(resp: &Value) -> Result<()> {
    match resp.get("code").and_then(|c| c.as_i64()) {
        Some(0) | None => Ok(()),
        Some(code) => Err(Error::Cloud {
            code,
            message: resp
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or_default()
                .to_string(),
        }),
    }
}

fn value_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// set 单项校验：code ∈ {0,1} 通过（1=已受理，异步上报），其余视为设备拒绝。
fn check_set_items(items: &[Value]) -> Result<()> {
    for it in items {
        let code = it.get("code").and_then(|v| v.as_i64()).unwrap_or(0);
        if code != 0 && code != 1 {
            return Err(Error::Cloud {
                code,
                message: format!(
                    "设备拒绝 (siid={} piid={} code={code})",
                    it.get("siid").and_then(|v| v.as_i64()).unwrap_or(0),
                    it.get("piid").and_then(|v| v.as_i64()).unwrap_or(0),
                ),
            });
        }
    }
    Ok(())
}

fn parse_device(item: &Value) -> Option<Device> {
    let did = item.get("did")?;
    Some(Device {
        did: value_to_string(did),
        model: item.get("model").and_then(|v| v.as_str()).unwrap_or("").into(),
        name: item.get("name").and_then(|v| v.as_str()).unwrap_or("").into(),
        online: matches!(item.get("isOnline"), Some(Value::Number(n)) if n.as_i64() == Some(1))
            || matches!(item.get("isOnline"), Some(Value::Bool(true))),
        spec_type: item
            .get("spec_type")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        spec: item.get("spec").filter(|v| v.is_object()).cloned(),
        token: item
            .get("token")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        mac: item.get("mac").filter(|v| !v.is_null()).map(value_to_string),
        // 房间归属不来自 device_list_page（实测该响应没有 room/home 字段），
        // 由 MiHome::list_devices 调 room 模块回填。
        room: None,
        room_rank: None,
    })
}
