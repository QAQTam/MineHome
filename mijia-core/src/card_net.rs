//! # 卡片配置的网络层与编排（方案 A：抓取/解析/缓存全在 Rust）
//!
//! - `getappconfig` 是**普通无签名 GET**（实测 200）：三份配置带固定 version/lang 常量，
//!   响应为明文 JSON（`result.content` 为模板/文案字符串）。
//! - spec 明细走 `miot-spec.org/miot-spec-v2/instance?type=<urn>`（与 SpecKit 同款）。
//! - 全部落盘缓存到 `<filesDir>/card/`：配置 `<name>.json`、spec `spec/<safe>.json`；
//!   命中缓存即不联网，失败时卡片仍能渲染 DTO（只是无当前值）。
//! - [`card_items`] 是给 napi 的一站式编排：engine → spec → items_for →
//!   收集要读的属性 → `MiHome::get_properties`（miotspec 优先）→ evaluate(联动禁用)。

use crate::card::{CardEngine, CardItemVm, CardSpec};
use crate::error::{Error, Result};
use crate::mi_home::MiHome;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// 三份 getappconfig 常量：(name, version, lang)。版本/lang 由本层固定，
/// 避免 ArkTS 侧散落魔法值（见 card_design.md 第六节边界守则）。
pub const CFG_MIOTSPEC: (&str, &str, &str) = ("card_control_miotspec_config", "7", "en");
pub const CFG_LEGACY: (&str, &str, &str) = ("card_control_config", "13", "zh_CN");
pub const CFG_TXT: (&str, &str, &str) = ("card_default_txt", "2", "en");
const ALL_CONFIGS: [(&str, &str, &str); 3] = [CFG_MIOTSPEC, CFG_LEGACY, CFG_TXT];

const UA_OKHTTP: &str = "okhttp/3.12";
const GETAPPCONFIG: &str = "https://api.io.mi.com/app/service/getappconfig";
const SPEC_BASE: &str = "https://miot-spec.org/miot-spec-v2/instance?type=";

/// 会话文件路径 → `<filesDir>/card/`。
pub fn card_cache_dir(session_path: &Path) -> PathBuf {
    session_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("card")
}

/// Python `urllib.parse.quote(s, safe='')` 等价（query 值编码）。
fn pct(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

fn getappconfig_url(name: &str, version: &str, lang: &str) -> String {
    // 顺序 name/version/lang 与 Python 原型一致；紧凑 JSON 无空格
    let data = json!({"name": name, "version": version, "lang": lang}).to_string();
    format!("{GETAPPCONFIG}?data={}", pct(&data))
}

fn spec_url(spec_type: &str) -> String {
    format!("{SPEC_BASE}{}", pct(spec_type))
}

/// 单次无签名 GET（reqwest + rustls）。返回响应体文本。
async fn http_get(url: &str) -> Result<String> {
    let client = reqwest::Client::builder()
        .build()
        .map_err(|e| Error::Transport(format!("reqwest 构造失败: {e}")))?;
    let resp = client
        .get(url)
        .header("User-Agent", UA_OKHTTP)
        .send()
        .await
        .map_err(|e| Error::Transport(format!("GET 失败 {url}: {e}")))?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| Error::Transport(format!("读取响应失败: {e}")))?;
    if !status.is_success() {
        return Err(Error::Http {
            status: status.as_u16(),
            text: body.chars().take(200).collect(),
        });
    }
    Ok(body)
}

/// 抓一份 getappconfig 外层响应（校验 code=0 且 result.content 存在）。
pub async fn fetch_config(name: &str, version: &str, lang: &str) -> Result<String> {
    let body = http_get(&getappconfig_url(name, version, lang)).await?;
    let v: Value = serde_json::from_str(&body)?;
    let code = v.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
    if code != 0 {
        return Err(Error::Cloud {
            code,
            message: v
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or_default()
                .to_string(),
        });
    }
    Ok(body)
}

fn ensure_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    Ok(())
}

/// 读磁盘缓存（有效 JSON）或联网抓取后落盘。
async fn load_config(cache_dir: &Path, cfg: (&str, &str, &str)) -> Result<String> {
    let (name, version, lang) = cfg;
    let path = cache_dir.join(format!("{name}.json"));
    if let Ok(text) = std::fs::read_to_string(&path) {
        if serde_json::from_str::<Value>(&text).is_ok() {
            return Ok(text);
        }
    }
    let body = fetch_config(name, version, lang).await?;
    ensure_dir(cache_dir)?;
    std::fs::write(&path, &body)?;
    Ok(body)
}

/// 加载并解析三份配置 → CardEngine（不缓存 engine 本身，见 [`engine`]）。
pub async fn load_engine(cache_dir: &Path) -> Result<CardEngine> {
    let mut bodies: Vec<String> = Vec::with_capacity(ALL_CONFIGS.len());
    for cfg in ALL_CONFIGS {
        bodies.push(load_config(cache_dir, cfg).await?);
    }
    CardEngine::from_json(&bodies[0], &bodies[2], &bodies[1])
}

static ENGINE: OnceLock<CardEngine> = OnceLock::new();

/// 全局复用已解析的 engine（解析 79KB+207KB 一次即可；并发重复加载无害）。
async fn engine(cache_dir: &Path) -> Result<&'static CardEngine> {
    if let Some(e) = ENGINE.get() {
        return Ok(e);
    }
    let e = load_engine(cache_dir).await?;
    let _ = ENGINE.set(e);
    Ok(ENGINE.get().expect("engine 已设置"))
}

fn spec_cache_path(cache_dir: &Path, spec_type: &str) -> PathBuf {
    let safe = spec_type.replace([':', '/'], "_");
    cache_dir.join("spec").join(format!("{safe}.json"))
}

/// 加载 spec 实例：磁盘缓存 → miot-spec.org GET → CacheSpec。
pub async fn load_spec(cache_dir: &Path, spec_type: &str) -> Result<CardSpec> {
    let path = spec_cache_path(cache_dir, spec_type);
    let text = if let Ok(t) = std::fs::read_to_string(&path) {
        t
    } else {
        let body = http_get(&spec_url(spec_type)).await?;
        let v: Value = serde_json::from_str(&body)?;
        if v.get("services").and_then(|s| s.as_array()).map_or(true, |a| a.is_empty()) {
            return Err(Error::Protocol(format!("spec 响应无 services: {spec_type}")));
        }
        ensure_dir(path.parent().unwrap_or(cache_dir))?;
        std::fs::write(&path, &body)?;
        body
    };
    let v: Value = serde_json::from_str(&text)?;
    Ok(CardSpec::from_instance(&v))
}

/// 去重收集需要读的属性地址（item 自身 + 联动条件引用的 key）。
/// 动作项（is_action）不可读，跳过。
fn read_refs(items: &[CardItemVm]) -> Vec<(i64, i64)> {
    let mut refs: Vec<(i64, i64)> = Vec::new();
    for it in items {
        if !it.is_action {
            for r in std::iter::once((it.siid, it.piid))
                .chain(it.disabled_when.iter().map(|c| (c.siid, c.piid)))
            {
                if r != (0, 0) && !refs.contains(&r) {
                    refs.push(r);
                }
            }
        }
    }
    refs
}

/// 把本地来源（蓝牙直收）标记写回 DTO；ArkTS 按 `src=="local"` 渲染「蓝牙」徽标。
fn mark_local(items: &mut [CardItemVm], local: &[(i64, i64)]) {
    if local.is_empty() {
        return;
    }
    for it in items.iter_mut() {
        if local.contains(&(it.siid, it.piid)) {
            it.src = Some("local".to_string());
        }
    }
}

/// 二级弹层编排：detail_items（模板全量 + spec 可写 + 动作）→ 读值 → 联动求值。
/// `realtime=true` 用 rpcv2 穿透读（下发后的确认回读，避免 miotspec 缓存旧值
/// 打回乐观更新）；false 用云端缓存读（常规轮询，负载低）。
pub async fn card_detail(
    did: &str,
    spec_type: &str,
    cache_dir: &Path,
    home: &MiHome,
    mac: Option<&str>,
    realtime: bool,
) -> Result<Vec<CardItemVm>> {
    let eng = engine(cache_dir).await?;
    let spec = load_spec(cache_dir, spec_type).await?;
    let mut items = eng.detail_items(&spec, spec_type);
    let refs = read_refs(&items);
    let props = if refs.is_empty() {
        Vec::new()
    } else {
        let r = if realtime {
            home.get_properties_realtime(did, &refs).await
        } else {
            home.get_properties(did, &refs).await
        };
        r.unwrap_or_default()
    };
    // 本地优先：BLE 广播落库的读数比云端缓存新鲜时并进去（见 ble_cache 模块）
    let (props, local) = crate::ble_cache::apply_local(mac, did, &spec, &refs, props);
    eng.evaluate(&mut items, &props);
    mark_local(&mut items, &local);
    Ok(items)
}

/// 一站式编排（napi 薄壳只调此函数）：
/// 模板归一化 → 读当前属性值 → 联动求值 → 返回可直接渲染的 DTO。
/// 属性读取失败（离线/未登录/风控）时降级为空 props：卡片仍渲染，仅无当前值。
pub async fn card_items(
    did: &str,
    spec_type: &str,
    cache_dir: &Path,
    home: &MiHome,
    mac: Option<&str>,
) -> Result<Vec<CardItemVm>> {
    let eng = engine(cache_dir).await?;
    let spec = load_spec(cache_dir, spec_type).await?;
    let mut items = eng.items_for(&spec, spec_type);
    let refs = read_refs(&items);
    let props = if refs.is_empty() {
        Vec::new()
    } else {
        home.get_properties(did, &refs).await.unwrap_or_default()
    };
    let (props, local) = crate::ble_cache::apply_local(mac, did, &spec, &refs, props);
    eng.evaluate(&mut items, &props);
    mark_local(&mut items, &local);
    Ok(items)
}

/// 首页卡片（带用户自定义显示项）。语义与 [`card_items`] 一致，只是最后多一步
/// `layout::apply` 过滤 + 排序；`visible` 为空 = 用户没自定义过，行为完全等同 `card_items`。
pub async fn card_items_layout(
    did: &str,
    spec_type: &str,
    cache_dir: &Path,
    home: &MiHome,
    mac: Option<&str>,
    layout: Option<&crate::layout::CardLayout>,
) -> Result<Vec<CardItemVm>> {
    let items = card_items(did, spec_type, cache_dir, home, mac).await?;
    Ok(crate::layout::apply(items, layout).0)
}

/// 编辑器数据源：**不过滤**的全量项 + 每项当前是否显示。
/// 单独开一个入口是为了让 key 的拼法只存在于 Rust 一侧
/// （ArkTS 自己拼 `kind:siid:piid` 一旦拼错，勾选就永远对不上）。
pub async fn card_layout_options(
    did: &str,
    spec_type: &str,
    cache_dir: &Path,
    home: &MiHome,
    mac: Option<&str>,
    layout: Option<&crate::layout::CardLayout>,
) -> Result<Vec<crate::layout::LayoutOption>> {
    let items = card_items(did, spec_type, cache_dir, home, mac).await?;
    Ok(crate::layout::options(&items, layout))
}

/// 编辑器数据源的**档案版**：给定档案文本，返回"按当前固件实际会生效"的勾选状态。
///
/// 为什么必须有这个入口：档案对 ArkTS 是不透明文本，它不该自己解析 JSON 去猜
/// 哪些项该打勾——那样"什么算同一个属性"的判定就分裂成两处了。所以让 Rust 先按
/// 当前 spec 校验指纹，把结论以 `checked` 形式交回去（不匹配项自然是未勾选）。
pub async fn card_layout_options_profile(
    did: &str,
    spec_type: &str,
    cache_dir: &Path,
    home: &MiHome,
    mac: Option<&str>,
    profile: &str,
) -> Result<(Vec<crate::layout::LayoutOption>, Vec<String>)> {
    let items = card_items(did, spec_type, cache_dir, home, mac).await?;
    let spec = load_spec(cache_dir, spec_type).await.unwrap_or_default();
    let (layout, warnings) = crate::profile::restore_text(profile, &spec);
    Ok((crate::layout::options(&items, Some(&layout)), warnings))
}

/// 生成**可共享档案**（含 spec 语义指纹，见 `crate::profile`）。
/// 档案文本不含 did、不含用户自取设备名，可以直接贴进 PR。
pub async fn export_profile(
    did: &str,
    spec_type: &str,
    model: &str,
    cache_dir: &Path,
    home: &MiHome,
    mac: Option<&str>,
    layout: &crate::layout::CardLayout,
) -> Result<String> {
    let items = card_items(did, spec_type, cache_dir, home, mac).await?;
    let spec = load_spec(cache_dir, spec_type).await.unwrap_or_default();
    Ok(crate::profile::build(model, spec_type, &spec, &items, layout).to_string())
}

/// 用档案渲染首页卡片：先按**当前** spec 校验每一项的指纹，
/// 不匹配的一律剔除——宁可少显示一个控件，也不能把用户当年勾的「灯开关」
/// 重绑到改版后的别的属性上（`profile` 模块里那条安全性质）。
/// 返回 (要显示的项, 被剔除的原因)。
pub async fn card_items_profile(
    did: &str,
    spec_type: &str,
    cache_dir: &Path,
    home: &MiHome,
    mac: Option<&str>,
    profile: &str,
) -> Result<(Vec<CardItemVm>, Vec<String>)> {
    let items = card_items(did, spec_type, cache_dir, home, mac).await?;
    let spec = load_spec(cache_dir, spec_type).await.unwrap_or_default();
    let (layout, warnings) = crate::profile::restore_text(profile, &spec);
    Ok((crate::layout::apply(items, Some(&layout)).0, warnings))
}

/// 只校验不渲染：导入别人档案时，先把每一项的拒绝原因给用户看清楚再落盘。
pub async fn profile_warnings(
    spec_type: &str,
    cache_dir: &Path,
    profile: &str,
) -> Result<Vec<String>> {
    let spec = load_spec(cache_dir, spec_type).await.unwrap_or_default();
    Ok(crate::profile::restore_text(profile, &spec).1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn getappconfig_url_uses_lang_key() {
        let u = getappconfig_url("card_control_miotspec_config", "7", "en");
        assert!(u.contains("lang"), "参数 key 必须是 lang（写 language 报 -8）");
        assert!(!u.contains("language"));
        assert!(u.contains("data=%7B"), "data 应 percent-encode JSON");
        assert!(u.contains("card_control_miotspec_config"));
    }

    #[test]
    fn spec_url_encodes_urn() {
        let u = spec_url("urn:miot-spec-v2:device:fan:0000A007:dmaker-p5c:1");
        assert!(u.contains("%3A"), "URN 冒号应编码");
        assert!(u.contains("dmaker-p5c"));
    }

    #[test]
    fn cache_dir_is_under_filesdir() {
        let p = Path::new("/data/app/files/session.json");
        assert_eq!(card_cache_dir(p), PathBuf::from("/data/app/files/card"));
    }

    #[test]
    fn read_refs_dedups_and_drops_placeholder() {
        use crate::card::{CardCond, CardKind};
        let mk = |siid, piid, conds| CardItemVm {
            kind: CardKind::ObserveNumber,
            siid,
            piid,
            label: String::new(),
            unit: String::new(),
            format: String::new(),
            ratio: 1.0,
            choices: vec![],
            disabled_when: conds,
            enabled: true,
            value: None,
            code: 0,
            name_ignore: false,
            min: None,
            max: None,
            step: None,
            param_delta: None,
            is_action: false,
            src: None,
        };
        let items = vec![
            mk(2, 1, vec![]),
            mk(0, 0, vec![]),
            mk(2, 2, vec![CardCond { siid: 2, piid: 1, expect: "false".into() }]),
        ];
        let r = read_refs(&items);
        assert_eq!(r, vec![(2, 1), (2, 2)]);
    }
}
