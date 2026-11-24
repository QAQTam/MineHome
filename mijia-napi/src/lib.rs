//! mijia-core 的鸿蒙 NAPI 绑定（ohos-rs）。
//!
//! 构建：`ohrs build`（需 aarch64-unknown-linux-ohos target + HarmonicNDK），
//! 产物 `dist/libmijia_napi.so` + `index.d.ts`。
//!
//! 注意：本 crate 在无 ohos target 的开发机上**不参与编译验证**，
//! 编译与真机冒烟待 DevEco 工程建立后进行（见 ohos/miot_sdk/README.md）。
//!
//! ArkTS 侧薄封装见 `ohos/miot_sdk/`（@miot/sdk）。

use mijia_core::card::CardItemVm;
use mijia_core::card::CardSpec;
use mijia_core::card_net;
use mijia_core::ble_auth::{
    build_spec_get, build_spec_ver, build_stream_frames, parse_ebe0ccc1, parse_ebe0ccc4,
    parse_spec_frame, LOGIN_CMD, MibleSession, PortInfo, SpecFrame, SpecRxState, SpecTxState,
};
use mijia_core::ble_cache;
use mijia_core::layout::CardLayout;
use mijia_core::safety::{self, ToggleRef};
use base64::Engine as _;
use mijia_core::ble;
use mijia_core::MiHome;
use napi_ohos::{Error, Result};
use napi_derive_ohos::napi;
use serde_json::Value;
use std::sync::Mutex;
use std::path::PathBuf;
use std::sync::OnceLock;

static HOME: OnceLock<MiHome> = OnceLock::new();
/// init(sessionPath) 时记录，用于推导 `<filesDir>/card/` 缓存目录。
static SESSION_PATH: OnceLock<PathBuf> = OnceLock::new();

fn home() -> Result<&'static MiHome> {
    HOME.get().ok_or_else(|| Error::from_reason("请先调用 init(sessionPath)"))
}

fn anyerr(e: impl std::fmt::Display) -> Error {
    Error::from_reason(e.to_string())
}

// -------------------------------------------------------------- 数据类型 ----

/// 领域设备模型（spec 以 JSON 文本透出，避免类型猜测）
#[napi(object)]
pub struct JsDevice {
    pub did: String,
    pub model: String,
    pub name: String,
    pub online: bool,
    /// spec_type URN（App 侧用它到 miot-spec.org 拉 spec 明细）
    pub spec_type: Option<String>,
    /// spec JSON 文本（无 spec 时为 null）
    pub spec: Option<String>,
    /// 设备 token（二期局域网直连用）
    pub token: Option<String>,
    /// BLE MAC（蓝牙设备本地广播收数用；非蓝牙设备为 null）
    pub mac: Option<String>,
    /// 房间名（云端 `/v2/homeroom/gethome` 归一化；未分配房间或拉取失败 = null）
    pub room: Option<String>,
    /// 分组序号（家庭序号*1000+房间序号，云端顺序）；null = 未分组，渲染排最后
    pub room_rank: Option<i64>,
}

#[napi(object)]
pub struct JsPropValue {
    pub did: String,
    pub siid: i64,
    pub piid: i64,
    pub code: i64,
    /// 属性值 JSON 文本
    pub value: String,
    /// 云端缓存更新时间（秒级时间戳，miotspec 通道返回；实时读为 null）
    pub update_time: Option<i64>,
}

/// 登录结果：ok=true 直连成功；need_confirm = notificationUrl
/// （securityStatus=16，交给 @miot/sdk 的 ConfirmPage 用 Web 组件加载）
#[napi(object)]
pub struct JsLoginResult {
    pub ok: bool,
    pub need_confirm: Option<String>,
}

// ---------------------------------------------------------------- API ------

/// 初始化：指定会话文件路径（建议 `<filesDir>/session.json`）。
/// 幂等；首次登录后 session.json 与 Python 原型格式互通。
#[napi]
pub fn init(session_path: String) -> Result<()> {
    let home = MiHome::new(&session_path).map_err(anyerr)?;
    HOME.set(home).map_err(|_| Error::from_reason("init 重复调用"))?;
    let _ = SESSION_PATH.set(PathBuf::from(&session_path));
    Ok(())
}

#[napi]
pub fn is_logged_in() -> bool {
    HOME.get().map(|h| h.is_logged_in()).unwrap_or(false)
}

/// 外部导入 session.json（写入 filesDir 后）重新加载会话。
#[napi]
pub fn reload_session() -> Result<()> {
    home()?.reload_session().map_err(anyerr)?;
    Ok(())
}

/// 退出会话（调试用）：清空内存会话并删除 session.json。
#[napi]
pub fn logout() -> Result<()> {
    home()?.logout().map_err(anyerr)?;
    Ok(())
}

/// 密码登录。ok=true 时会话已持久化；need_confirm 时把 url 交给 ConfirmPage。
#[napi]
pub async fn login_password(user: String, pass: String) -> Result<JsLoginResult> {
    match home()?.login_password(&user, &pass).await.map_err(anyerr)? {
        mijia_core::LoginFlow::Direct => Ok(JsLoginResult { ok: true, need_confirm: None }),
        mijia_core::LoginFlow::NeedNotification { url } => {
            Ok(JsLoginResult { ok: false, need_confirm: Some(url) })
        }
    }
}

/// 通知确认（ConfirmPage 拿到 passToken 后）收尾。user_id 可传空串。
#[napi]
pub async fn finish_notification(pass_token: String, user_id: String) -> Result<()> {
    home()?
        .finish_notification(&pass_token, &user_id)
        .await
        .map_err(anyerr)?;
    Ok(())
}

#[napi(object)]
pub struct JsPropRef {
    pub siid: i64,
    pub piid: i64,
}

/// 设备列表（spec_type + spec 文本透出）
#[napi]
pub async fn get_devices() -> Result<Vec<JsDevice>> {
    let devices = home()?.list_devices().await.map_err(anyerr)?;
    Ok(devices
        .into_iter()
        .map(|d| JsDevice {
            did: d.did,
            model: d.model,
            name: d.name,
            online: d.online,
            spec_type: d.spec_type,
            spec: d.spec.map(|v| v.to_string()),
            token: d.token,
            mac: d.mac,
            room: d.room,
            room_rank: d.room_rank,
        })
        .collect())
}

// ---------------------------------------------------- 档位 A：BLE 广播 ----

/// 解析一条 MiBeacon 广播（档位 A 本地收数，只读）。
/// `token`：设备列表 token（hex 24/32 字符 或 base64 解出 12/16B）；明文帧可传空串。
/// `adv_base64`：`ble.on('BLEDeviceFind')` 的 advertisingData 原始字节 base64。
/// `known_mac`：设备真实 mac（12 位 hex，可空）——RPA 广播+包内无 mac 的省电帧
/// （MJWSD06MMC 等）组 V5 nonce 必需；其它场景传空串即可。
/// 返回 MiBeacon JSON（mac/device_id/values/unknown...）；非米家广播/解不开返回 null。
#[napi]
pub fn ble_decode(token: String, adv_base64: String, known_mac: Option<String>) -> Option<String> {
    let adv = base64::engine::general_purpose::STANDARD.decode(adv_base64.as_bytes()).ok()?;
    let km = known_mac.as_deref().and_then(parse_mac_hex);
    let mb = ble::decode_adv_mac(&token, &adv, km.as_ref())?;
    serde_json::to_string(&mb).ok()
}

fn parse_mac_hex(s: &str) -> Option<[u8; 6]> {
    let t = s.trim().replace(':', "");
    if t.len() != 12 || !t.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let mut m = [0u8; 6];
    for i in 0..6 {
        m[i] = u8::from_str_radix(&t[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(m)
}

/// 不解密，只提取 MiBeacon 包内真实 MAC（标准序小写 hex，无分隔）。
/// RPA 广播地址场景下上层按 deviceId 查不到 token 时用（非米家/无 mac 位返回 null）。
#[napi]
pub fn ble_peek_mac(adv_base64: String) -> Option<String> {
    let adv = base64::engine::general_purpose::STANDARD.decode(adv_base64.as_bytes()).ok()?;
    ble::peek_mac(&adv)
}

/// BLE（blt.*）设备 MiBeacon 广播 bindkey（32 hex = 16B）——档位 A 加密帧专用，
/// 与 device list 的 24-hex 云端 token 不同源；非 BLE 设备返回 null。
#[napi]
pub async fn ble_beaconkey(did: String) -> Result<Option<String>> {
    home()?
        .beaconkey(&did)
        .await
        .map_err(anyerr)
}

/// 本地读数落库（本地优先的基础）：解码成功的 MiBeacon JSON 写入全局快照缓存，
/// 按包内 MAC 归档、跨包合并语义键（温/湿/电/门磁各自更新各自的键）。
/// card_items / card_detail 读值时把"比云端新鲜"的本地值并进卡片（ble_cache::apply_local）。
#[napi]
pub fn ble_ingest(beacon_json: String) -> bool {
    ble_cache::ingest(&beacon_json)
}

/// 多目标试解（常驻扫描场景）：RPA 广播 + 包内无 mac 的 V4/V5 加密帧无法按
/// 广播地址查 key，逐目标试解。V4/V5 解密自带 MIC 校验，key 错必失败不会误归档；
/// legacy(v2/v3) 无 MIC 不参与试解（解错会"成功"产出垃圾值）。
/// `targets_json`：`[["12hex mac","key"], ...]`；返回命中目标的 MiBeacon JSON，全败 null。
#[napi]
pub fn ble_decode_multi(targets_json: String, adv_base64: String) -> Option<String> {
    let adv = base64::engine::general_purpose::STANDARD.decode(adv_base64.as_bytes()).ok()?;
    let targets: Vec<Vec<String>> = serde_json::from_str(&targets_json).ok()?;
    for t in targets {
        if t.len() < 2 {
            continue;
        }
        let km = parse_mac_hex(&t[0]);
        if let Some(mb) = ble::decode_adv_mac(&t[1], &adv, km.as_ref()) {
            if mb.version >= 4 {
                return serde_json::to_string(&mb).ok();
            }
        }
    }
    None
}

/// 批量读 spec 属性（一次请求；单项失败 code!=0 不影响其它项）
#[napi]
pub async fn get_properties(did: String, props: Vec<JsPropRef>) -> Result<Vec<JsPropValue>> {
    let refs: Vec<(i64, i64)> = props.iter().map(|p| (p.siid, p.piid)).collect();
    let vals = home()?.get_properties(&did, &refs).await.map_err(anyerr)?;
    Ok(vals
        .into_iter()
        .map(|v| JsPropValue {
            did: v.did,
            siid: v.siid,
            piid: v.piid,
            code: v.code,
            value: v.value.to_string(),
            update_time: v.update_time,
        })
        .collect())
}

/// 读 spec 属性
#[napi]
pub async fn get_property(did: String, siid: i64, piid: i64) -> Result<JsPropValue> {
    let vals = home()?
        .get_properties(&did, &[(siid, piid)])
        .await
        .map_err(anyerr)?;
    let v = vals.into_iter().next().ok_or_else(|| Error::from_reason("设备未返回属性"))?;
    Ok(JsPropValue {
        did: v.did,
        siid: v.siid,
        piid: v.piid,
        code: v.code,
        value: v.value.to_string(),
        update_time: v.update_time,
    })
}

/// 写 spec 属性。value 为 JSON 文本（true / 23.5 / "闭" / {...}）。
#[napi]
pub async fn set_property(did: String, siid: i64, piid: i64, value: String) -> Result<()> {
    let value: Value =
        serde_json::from_str(&value).map_err(|e| Error::from_reason(format!("value 非法 JSON: {e}")))?;
    home()?
        .set_properties(&did, &[(siid, piid, value)])
        .await
        .map_err(anyerr)
}

/// 老 miIO 协议（非 spec 设备）。params/result 均为 JSON 文本。
#[napi]
pub async fn call_method(did: String, method: String, params: String) -> Result<String> {
    let params: Value = serde_json::from_str(&params)
        .map_err(|e| Error::from_reason(format!("params 非法 JSON: {e}")))?;
    let result = home()?
        .call_miio(&did, &method, params)
        .await
        .map_err(anyerr)?;
    Ok(result.to_string())
}

/// 当前区域（v0.1 仅 cn）
#[napi]
pub fn current_server() -> String {
    "cn".to_string()
}

// ------------------------------------------------ 卡片模板 DTO（方案 A） ----

/// 归一化卡片项：ArkTS 唯一消费的 DTO（零模板 JSON 解析）。
/// Rust 侧已完成模板匹配 / prop_key 解析 / 文案合并 / 联动条件求值。
#[napi(object)]
pub struct JsCardItem {
    /// 组件类型（kind_of 的 snake_case）：toggle/enum_choice/stepper/...
    pub kind: String,
    pub siid: i64,
    pub piid: i64,
    /// 中文 label（instance comment / card_default_txt zh_CN）
    pub label: String,
    pub unit: String,
    /// 显示格式（"%.1f" / spec format）
    pub format: String,
    /// 数值缩放（大气压 0.001 等）
    pub ratio: f64,
    /// 当前读值 JSON 文本（true / 23.5 / "制冷" / null）
    pub value: Option<String>,
    /// 枚举选项 JSON 数组文本（[{value,label,en}]）
    pub choices: String,
    /// 联动启用态（关机时 mode/温度等为 false → ArkTS 灰显）
    pub enabled: bool,
    /// 单项读取 code（!=0 隐藏/标记失败）
    pub code: i64,
    /// 观察值是否忽略 label
    pub name_ignore: bool,
    /// 值域下界（spec value-range；二级滑条/步进用）
    pub min: Option<f64>,
    /// 值域上界
    pub max: Option<f64>,
    /// 值域步长
    pub step: Option<f64>,
    /// 模板步进增量（param_delta）
    pub param_delta: Option<f64>,
    /// true = (siid,piid) 是动作地址，点击走 callAction 而非 setProperty
    pub is_action: bool,
    /// 读值来源："local" = 本地蓝牙广播直收（比云端缓存新鲜）；null = 云端
    pub src: Option<String>,
}

fn to_js_card_item(it: CardItemVm) -> JsCardItem {
    JsCardItem {
        kind: it.kind.as_str().to_string(),
        siid: it.siid,
        piid: it.piid,
        label: it.label,
        unit: it.unit,
        format: it.format,
        ratio: it.ratio,
        value: it.value.map(|v| v.to_string()),
        choices: serde_json::to_string(&it.choices).unwrap_or_else(|_| "[]".to_string()),
        enabled: it.enabled,
        code: it.code,
        name_ignore: it.name_ignore,
        min: it.min,
        max: it.max,
        step: it.step,
        param_delta: it.param_delta,
        is_action: it.is_action,
        src: it.src,
    }
}

/// 卡片模板归一化（薄壳）：`mijia-core` 抓配置/解析/读值/求值，ArkTS 只渲染返回的 DTO。
/// 首页精简视图；二级弹层用 [`card_detail`]。本函数只读，不下发任何控制。
///
/// `visible`/`known` 是首页「自定义卡片」的两份名单（见 `mijia_core::layout`）：
/// `visible` = 要显示的项、且按这个顺序渲染（key 形如 `toggle:2:1`）；
/// `known` = 上次编辑时见过的全部 key，用来区分"用户明确不要"与"还没见过的新项"。
/// 两者都不传 = 没自定义过，行为与之前逐字一致（默认零变化）。
#[napi]
pub async fn card_items(
    did: String,
    spec_type: String,
    visible: Option<Vec<String>>,
    known: Option<Vec<String>>,
    profile: Option<String>,
    mac: Option<String>,
) -> Result<Vec<JsCardItem>> {
    let h = home()?;
    let sp = SESSION_PATH
        .get()
        .ok_or_else(|| Error::from_reason("请先调用 init(sessionPath)"))?;
    let cache_dir = card_net::card_cache_dir(sp);
    let mac_arg = mac.as_deref();
    // 有档案就走档案通道（带 spec 指纹校验，不匹配项一律剔除）；
    // 没档案才退回旧的 visible/known 名单。
    let items = match profile {
        Some(p) if !p.is_empty() => {
            card_net::card_items_profile(&did, &spec_type, &cache_dir, h, mac_arg, &p)
                .await
                .map_err(anyerr)?
                .0
        }
        _ => {
            let layout = build_layout(visible, known);
            card_net::card_items_layout(&did, &spec_type, &cache_dir, h, mac_arg, layout.as_ref())
                .await
                .map_err(anyerr)?
        }
    };
    Ok(items.into_iter().map(to_js_card_item).collect())
}

/// 生成可共享档案（不透明文本：含 spec 语义指纹，不含 did 与用户设备名）。
#[napi]
pub async fn export_card_profile(
    did: String,
    spec_type: String,
    model: String,
    visible: Vec<String>,
    known: Vec<String>,
    mac: Option<String>,
) -> Result<String> {
    let h = home()?;
    let sp = SESSION_PATH
        .get()
        .ok_or_else(|| Error::from_reason("请先调用 init(sessionPath)"))?;
    let cache_dir = card_net::card_cache_dir(sp);
    let layout = CardLayout { visible, known };
    card_net::export_profile(&did, &spec_type, &model, &cache_dir, h, mac.as_deref(), &layout)
        .await
        .map_err(anyerr)
}

/// 导入前预检：返回每一项被拒绝的原因（空数组 = 这份档案在当前固件上完全可用）。
#[napi]
pub async fn card_profile_warnings(
    spec_type: String,
    profile: String,
) -> Result<Vec<String>> {
    let sp = SESSION_PATH
        .get()
        .ok_or_else(|| Error::from_reason("请先调用 init(sessionPath)"))?;
    let cache_dir = card_net::card_cache_dir(sp);
    card_net::profile_warnings(&spec_type, &cache_dir, &profile)
        .await
        .map_err(anyerr)
}

/// 档案导入后的编辑器预览：勾选状态由 Rust 按当前固件校验后给出，
/// ArkTS 不参与"这项还算不算同一属性"的判断。
#[napi(object)]
pub struct JsLayoutPreview {
    pub options: Vec<JsLayoutOption>,
    pub warnings: Vec<String>,
}

#[napi]
pub async fn card_layout_preview(
    did: String,
    spec_type: String,
    profile: String,
    mac: Option<String>,
) -> Result<JsLayoutPreview> {
    let h = home()?;
    let sp = SESSION_PATH
        .get()
        .ok_or_else(|| Error::from_reason("请先调用 init(sessionPath)"))?;
    let cache_dir = card_net::card_cache_dir(sp);
    let (opts, warnings) =
        card_net::card_layout_options_profile(&did, &spec_type, &cache_dir, h, mac.as_deref(), &profile)
            .await
            .map_err(anyerr)?;
    Ok(JsLayoutPreview {
        options: opts
            .into_iter()
            .map(|o| JsLayoutOption {
                key: o.key,
                label: o.label,
                kind: o.kind,
                checked: o.checked,
            })
            .collect(),
        warnings,
    })
}

/// 只有传了 `visible` 才构造 layout；否则 None（= 不启用自定义，走默认渲染）。
fn build_layout(visible: Option<Vec<String>>, known: Option<Vec<String>>) -> Option<CardLayout> {
    visible.map(|v| CardLayout {
        visible: v,
        known: known.unwrap_or_default(),
    })
}

/// 编辑器数据源：该设备首页**全量**候选项（不过滤）+ 每项当前是否显示。
/// key 的拼法只存在于 Rust 一侧，ArkTS 不参与拼接（拼错就永远勾不上）。
#[napi(object)]
pub struct JsLayoutOption {
    pub key: String,
    pub label: String,
    pub kind: String,
    pub checked: bool,
}

#[napi]
pub async fn card_layout_options(
    did: String,
    spec_type: String,
    visible: Option<Vec<String>>,
    known: Option<Vec<String>>,
    mac: Option<String>,
) -> Result<Vec<JsLayoutOption>> {
    let h = home()?;
    let sp = SESSION_PATH
        .get()
        .ok_or_else(|| Error::from_reason("请先调用 init(sessionPath)"))?;
    let cache_dir = card_net::card_cache_dir(sp);
    let layout = build_layout(visible, known);
    let opts = card_net::card_layout_options(&did, &spec_type, &cache_dir, h, mac.as_deref(), layout.as_ref())
        .await
        .map_err(anyerr)?;
    Ok(opts
        .into_iter()
        .map(|o| JsLayoutOption {
            key: o.key,
            label: o.label,
            kind: o.kind,
            checked: o.checked,
        })
        .collect())
}

/// 二级卡片全量 DTO：模板全量项 + spec 全量可写属性 + 动作，
/// 附带 value-range/param_delta/is_action；读值与联动求值同 [`card_items`]。
#[napi]
pub async fn card_detail(
    did: String,
    spec_type: String,
    realtime: bool,
    mac: Option<String>,
) -> Result<Vec<JsCardItem>> {
    let h = home()?;
    let sp = SESSION_PATH
        .get()
        .ok_or_else(|| Error::from_reason("请先调用 init(sessionPath)"))?;
    let cache_dir = card_net::card_cache_dir(sp);
    let items = card_net::card_detail(&did, &spec_type, &cache_dir, h, mac.as_deref(), realtime)
        .await
        .map_err(anyerr)?;
    Ok(items.into_iter().map(to_js_card_item).collect())
}

/// 一键圆钮判定结论。**安全边界全部在 Rust**（`mijia_core::safety`），
/// ArkTS 只按 `allowed` 决定画不画圆钮、按 `hint` 解释为什么没有。
#[napi(object)]
pub struct JsSwitchDecision {
    pub allowed: bool,
    /// 机器可读码（allowed_spec_on / blocked_infra / …），给日志与档案断言用
    pub code: String,
    /// 给人看的一句话理由
    pub hint: String,
    /// spec 规范 `on` 属性地址；`null` = 该设备 spec 里没有可写 on（判定退回 label 兜底）
    pub on_siid: Option<i64>,
    pub on_piid: Option<i64>,
}

/// 问一句「这台设备的这个模板主开关能不能给一键」。spec 走磁盘缓存
/// （与 [`card_items`] 同一份，稳态不发网络请求）；缓存缺失且拉不到时
/// **不报错**，用空 spec 继续判定 → 退回 label 兜底，与旧 ArkTS 行为一致。
#[napi]
pub async fn quick_switch(
    model: String,
    spec_type: String,
    toggle_siid: i64,
    toggle_piid: i64,
    label: String,
) -> Result<JsSwitchDecision> {
    let sp = SESSION_PATH
        .get()
        .ok_or_else(|| Error::from_reason("请先调用 init(sessionPath)"))?;
    let cache_dir = card_net::card_cache_dir(sp);
    let spec = card_net::load_spec(&cache_dir, &spec_type)
        .await
        .unwrap_or_else(|_| CardSpec::default());
    let d = safety::quick_switch_ref(
        &model,
        &spec_type,
        &spec,
        Some(ToggleRef {
            siid: toggle_siid,
            piid: toggle_piid,
            label: &label,
        }),
    );
    let on = safety::spec_on_prop(&spec);
    Ok(JsSwitchDecision {
        allowed: d.allowed,
        code: d.reason.code().to_string(),
        hint: d.reason.hint().to_string(),
        on_siid: on.map(|(i, _)| i),
        on_piid: on.map(|(_, p)| p),
    })
}

/// 执行 spec 动作（二级卡片 action 按钮；siid/iid 来自 is_action=true 的项）。
/// 返回 out 值 JSON 文本（无 out 为 "null"）。
#[napi]
pub async fn call_action(did: String, siid: i64, iid: i64) -> Result<String> {
    let v = home()?
        .call_action(&did, siid, iid)
        .await
        .map_err(anyerr)?;
    Ok(v.to_string())
}

/// 文案是否为**未翻译的协议 token**（`basic_arming` / `Vertical Swing` 这类）。
/// 规则只有一份，就在 `safety::looks_like_spec_token`；ArkTS 侧原先自带正则副本
/// 已于本轮删除——两份实现迟早漂移，而这条规则决定用户看到的是人话还是乱码。
/// 同步函数：调用点在 `refreshCard` 的数据准备阶段（不在 build 渲染里），每台十几条。
#[napi]
pub fn text_is_spec_token(text: String) -> bool {
    safety::looks_like_spec_token(&text)
}

// --------------------------------------- 档位 B：BLE 直连（充电器 0x001B） ----
//
// GATT 登录成功后的 0x001B 数据通道（协议见 docs/analysis/蓝牙标准认证协议.md §8，
// 登录流镜像 `_work/ble_login.py::run_login`）。ArkTS ChargerGatt 只做字节搬运：
// 密钥派生 / CCM 解密 / spec v2 解析 / 端口解码全在 Rust。通道流组装（CMD start、
// 数据帧、ACK）在 ArkTS 侧，与 Python 同款。

/// 全局 BLE spec 会话槽（同一时刻至多一条 GATT 连接；close 即清）。
static BLE_SESSION: OnceLock<Mutex<Option<(MibleSession, SpecRxState, SpecTxState)>>> =
    OnceLock::new();

fn hex_encode(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push_str(&format!("{:02x}", x));
    }
    s
}

/// f32 → 两位小数 f64（避免 f32 尾数 9.100000381… 污染 JSON 与 UI 显示）。
/// 注意除法必须在 f64 域完成：f32 里 910/100 又回到 9.1_f32 的尾数。
fn round2(x: f32) -> f64 {
    (x * 100.0).round() as f64 / 100.0
}

fn hex_decode(s: &str) -> Result<Vec<u8>> {
    let t: String = s.trim().chars().filter(|c| !c.is_whitespace()).collect();
    if t.len() % 2 != 0 {
        return Err(Error::from_reason("hex 长度须为偶数"));
    }
    (0..t.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&t[i..i + 2], 16)
                .map_err(|e| Error::from_reason(format!("hex 非法: {e}")))
        })
        .collect()
}

/// 登录密钥三件套（纯函数，登录流全程用）。token = 设备 12B 云 token（24 位 hex，
/// device list 口径）；app_rand/dev_rand = 登录双向随机数（各 16B hex）。
#[napi(object)]
pub struct JsMibleKeys {
    /// 64B 派生密钥 hex：RX key=okm[0:16]、TX key=okm[16:32]、nonce 前缀 okm[32:40]
    pub okm: String,
    /// devConfirm 期望值 hex（32B）：与设备 type=12 上送值比对，PASS 才发 appConfirm
    pub dev_confirm: String,
    /// appConfirm hex（32B）：登录流 type=10 上送（每连接只评估第一个候选，勿试错）
    pub app_confirm: String,
}

#[napi]
pub fn mible_derive(
    token_hex: String,
    app_rand_hex: String,
    dev_rand_hex: String,
) -> Result<JsMibleKeys> {
    let token = hex_decode(&token_hex)?;
    let app = hex_decode(&app_rand_hex)?;
    let dev = hex_decode(&dev_rand_hex)?;
    let s = MibleSession::derive(&token, &app, &dev);
    Ok(JsMibleKeys {
        okm: hex_encode(&s.okm),
        dev_confirm: hex_encode(&s.dev_confirm(&app, &dev)),
        app_confirm: hex_encode(&s.app_confirm(&app, &dev)),
    })
}

/// 登录成功（0x21）后开会话：派生 okm 并把 RX sn/roll 清零。每次新 GATT 连接调用一次。
#[napi]
pub fn ble_session_open(token_hex: String, app_rand_hex: String, dev_rand_hex: String) -> Result<()> {
    let token = hex_decode(&token_hex)?;
    let app = hex_decode(&app_rand_hex)?;
    let dev = hex_decode(&dev_rand_hex)?;
    let session = MibleSession::derive(&token, &app, &dev);
    let slot = BLE_SESSION.get_or_init(|| Mutex::new(None));
    let mut g = slot.lock().map_err(|_| Error::from_reason("BLE 会话槽锁中毒"))?;
    *g = Some((session, SpecRxState::default(), SpecTxState::default()));
    Ok(())
}

/// 断开时关会话（幂等）。
#[napi]
pub fn ble_session_close() {
    if let Some(slot) = BLE_SESSION.get() {
        if let Ok(mut g) = slot.lock() {
            *g = None;
        }
    }
}

/// 解密+解析一帧 0x001B 推送（已按通道流拼好：`[sn u16LE][密文‖MIC]`，实测 21B）。
/// 返回 JSON：`{"tid","opcode","objects":[{siid,piid,port?,value_u64?,state?,
/// voltage_v,current_a,power_w?}]}`；端口字段仅 siid=2 piid 1..4（C1/C2/C3/A）。
/// MIC 不符 / 未开会话 / 帧非法返回 null。
#[napi]
pub fn ble_spec_decode(frame_hex: String) -> Option<String> {
    let frame = hex_decode(&frame_hex).ok()?;
    let slot = BLE_SESSION.get()?;
    let mut g = slot.lock().ok()?;
    let (session, rx, _tx) = g.as_mut()?;
    let pt = rx.decrypt(&frame, session)?;
    let opcode_num = *pt.get(4)?; // 原始 opcode 字节（parse 后枚举不带原值）
    let f = parse_spec_frame(&pt)?;
    Some(frame_to_json(&f, opcode_num))
}

/// spec v2 帧 → 卡片消费的 JSON（纯函数，可测）。
fn frame_to_json(f: &SpecFrame, opcode_num: u8) -> String {
    let mut objects = Vec::new();
    for o in &f.objects {
        let mut obj = serde_json::json!({
            "siid": o.siid,
            "piid": o.piid,
        });
        if let Some(v) = o.value_u64() {
            obj["value_u64"] = serde_json::json!(v);
        }
        // 充电器端口信息：siid=2 piid 1..4 = C1/C2/C3/A（位域见 ble_auth.rs PortInfo）
        if o.siid == 2 && (1..=4).contains(&o.piid) {
            if let Some(v) = o.value_u64() {
                let p = PortInfo::from_u32(v as u32);
                obj["port"] = serde_json::json!(["C1", "C2", "C3", "A"][(o.piid - 1) as usize]);
                obj["state"] = serde_json::json!(p.state);
                obj["voltage_v"] = serde_json::json!(round2(p.voltage_v()));
                obj["current_a"] = serde_json::json!(round2(p.current_a()));
                obj["power_w"] = serde_json::json!(round2(p.power_w()));
            }
        }
        objects.push(obj);
    }
    serde_json::json!({
        "tid": f.tid,
        "opcode": opcode_num,
        "objects": objects,
    })
    .to_string()
}

/// spec v2 下行加密（登录成功后开会话才可用）：明文帧 → spec 通道下行帧
/// `[sn u16LE][密文‖MIC]` hex（TX key=okm[16:32]，sn/roll 会话内自增）。
/// 未开会话返回 null。
#[napi]
pub fn ble_spec_encrypt(plaintext_hex: String) -> Option<String> {
    let pt = hex_decode(&plaintext_hex).ok()?;
    let slot = BLE_SESSION.get()?;
    let mut g = slot.lock().ok()?;
    let (session, _rx, tx) = g.as_mut()?;
    Some(hex_encode(&tx.encrypt(&pt, session)))
}

/// mible 0x0010 登录命令帧（`24000000`，写 0x0010 发起登录）。
#[napi]
pub fn ble_login_cmd() -> String {
    hex_encode(&LOGIN_CMD)
}

/// spec v2 版本交换明文帧 hex（登录成功后先发一帧换版本）。
#[napi]
pub fn ble_build_ver_frame(tid: f64) -> String {
    hex_encode(&build_spec_ver(tid as u16))
}

/// spec v2 get 请求明文帧 hex。`oids_json`：`[[siid,piid],...]`，
/// 如 `[[2,1001],[2,1002],[3,1003]]`（温湿度计 s2.p1001/p1002 + s3.p1003）。
#[napi]
pub fn ble_build_spec_get(oids_json: String, tid: f64) -> Result<String> {
    let raw: Vec<Vec<f64>> = serde_json::from_str(&oids_json).map_err(anyerr)?;
    let oids: Vec<(u8, u16)> = raw
        .iter()
        .filter(|o| o.len() >= 2)
        .map(|o| (o[0] as u8, o[1] as u16))
        .collect();
    Ok(hex_encode(&build_spec_get(&oids, tid as u16)))
}

/// 0x0019 分片流上行帧序列（CMD start `[0000 00 type n]` + 数据帧 `[seq][≤18B]`），
/// 返回逐帧 hex，按序 writeNoRsp 到 0x0019。`pkg_type`：11=appRandom、10=appConfirm。
#[napi]
pub fn ble_build_stream_frames(pkg_type: f64, payload_hex: String) -> Result<Vec<String>> {
    let payload = hex_decode(&payload_hex)?;
    Ok(build_stream_frames(pkg_type as u8, &payload)
        .into_iter()
        .map(|f| hex_encode(&f))
        .collect())
}

/// 免登录快路径读数（档位 B）：ebe0ccc1 = `[temp int16 LE ÷100][humi u8]
/// [电压 int16 LE ÷1000]`（06 实机定案），可选带 ebe0ccc4 电量。
#[napi(object)]
pub struct JsHtReading {
    /// ℃
    pub temperature: f64,
    /// %
    pub humidity: f64,
    /// 电池电压 V
    pub voltage_v: f64,
    /// 电量 %（未传 ccc4 时 null）
    pub battery: Option<f64>,
}

#[napi]
pub fn ble_parse_ht(ccc1_hex: String, ccc4_hex: Option<String>) -> Option<JsHtReading> {
    let raw = hex_decode(&ccc1_hex).ok()?;
    let h = parse_ebe0ccc1(&raw)?;
    let battery = match ccc4_hex {
        Some(hx) => hex_decode(&hx)
            .ok()
            .and_then(|b| parse_ebe0ccc4(&b))
            .map(f64::from),
        None => None,
    };
    Some(JsHtReading {
        temperature: h.temp_c as f64,
        humidity: h.humidity as f64,
        voltage_v: h.voltage_v as f64,
        battery,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hex_roundtrip() {
        assert_eq!(hex_decode("0a1B2c").unwrap(), vec![0x0a, 0x1b, 0x2c]);
        assert_eq!(hex_decode("0a 1b").unwrap(), vec![0x0a, 0x1b]);
        assert!(hex_decode("abc").is_err());
        assert!(hex_decode("zz").is_err());
        assert_eq!(hex_encode(&[0x0a, 0x1b, 0x2c]), "0a1b2c");
    }

    /// 端口 JSON：手工构造 spec v2 明文（opcode=4 properties_changed，
    /// siid=2 piid=1 u32 LE [state=1][contract=10][I=11→1.1A][V=91→9.1V]）。
    /// CCM 解密路径由 mijia-core::ble_auth 的 golden 向量覆盖。
    #[test]
    fn test_frame_to_json_port() {
        let mut pt: Vec<u8> = Vec::new();
        pt.extend_from_slice(&(2u16 << 12 | 15u16).to_le_bytes()); // ver2 len15
        pt.extend_from_slice(&7u16.to_le_bytes()); // tid
        pt.push(4); // properties_changed
        pt.push(1); // count
        pt.push(2); // siid
        pt.extend_from_slice(&1u16.to_le_bytes()); // piid=1 (C1)
        pt.extend_from_slice(&((5u16 << 12) | 4u16).to_le_bytes()); // uint32
        pt.extend_from_slice(&u32::from_le_bytes([1, 10, 11, 91]).to_le_bytes());
        let f = parse_spec_frame(&pt).unwrap();
        let out = frame_to_json(&f, 4);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["tid"], 7);
        assert_eq!(v["opcode"], 4);
        let o = &v["objects"][0];
        assert_eq!(o["port"], "C1");
        assert_eq!(o["state"], 1);
        assert_eq!(o["voltage_v"], 9.1);
        assert_eq!(o["current_a"], 1.1);
        assert_eq!(o["value_u64"], u32::from_le_bytes([1, 10, 11, 91]));
    }
}
