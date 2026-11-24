//! DESIGN.md §10.5 / §10.6 的**客户端半边**：用真 rmcp 客户端（不是手搓
//! newline-delimited JSON）连本 server 的 stdio 子进程，验证
//!
//! 1. `qaqh-mcp` 的 `auto_lifecycle()`（Auto{preferred:[2026-07-28],
//!    legacy:Some(2025-11-25)}）与本 server 的 `supported_protocol_versions`
//!    **有交集且零回退**——ADR-0024 §后果点名的脆弱点；
//! 2. `server/discover` 真实回报只宣告 2026-07-28；
//! 3. legacy-only 客户端（`ClientLifecycleMode::Initialize`）连不上 ⇒ era 闸
//!    真会变化（宪规 18 反证）；
//! 4. 策略面在真客户端上行为一致（`reason` 只给审批人看、不参与风险判定）。
//!
//! 全程**不接真实云**（不配 `HOME_CONTROL_SESSION` ⇒ `cloud = None`）：
//! 本测试只证明协议与策略；物理下发由 `_work/mcp_smoke.py --actuate` 负责。

use rmcp::{
    RoleClient,
    model::{CallToolRequestParams, ClientCapabilities, ProtocolVersion, RequestMetaObject},
    service::{ClientLifecycleMode, ClientServiceExt, RunningService},
    transport::TokioChildProcess,
};
use serde_json::{Value, json};
use std::path::PathBuf;

/// 与 `qaqh-mcp/src/adapter.rs::auto_lifecycle()` 逐字同构。
/// 客户端契约若被降级（preferred 里去掉 2026-07-28），第一条测试立刻红。
fn auto_lifecycle() -> ClientLifecycleMode {
    ClientLifecycleMode::Auto {
        preferred_versions: vec![ProtocolVersion::V_2026_07_28],
        legacy_version: Some(ProtocolVersion::V_2025_11_25),
    }
}

fn workdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("hcm-era-{}-{}", tag, std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("approvals.txt"), "").unwrap();
    let _ = std::fs::remove_file(dir.join("pending.jsonl"));
    let _ = std::fs::remove_file(dir.join("audit.jsonl"));
    dir
}

async fn connect(
    dir: &PathBuf,
    lifecycle: ClientLifecycleMode,
) -> Result<RunningService<RoleClient, ()>, String> {
    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_home-control-mcp"));
    cmd.env("HOME_CONTROL_POLICY", "tests/fixtures/policy.era.json")
        .env("HOME_CONTROL_AUDIT", dir.join("audit.jsonl"))
        .env("HOME_CONTROL_APPROVALS", dir.join("approvals.txt"))
        .env("HOME_CONTROL_PENDING", dir.join("pending.jsonl"))
        .env_remove("HOME_CONTROL_SESSION")
        .stderr(std::process::Stdio::null());
    let transport = TokioChildProcess::new(cmd).map_err(|e| format!("spawn: {e}"))?;
    ().serve_with_lifecycle(transport, lifecycle)
        .await
        .map_err(|e| format!("{e}"))
}

/// rmcp 把我们的 JSON 文本放在 content[0].text；`isError` 单独看。
fn payload(res: &rmcp::model::CallToolResult) -> Value {
    let text = res
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.as_str()))
        .collect::<Vec<_>>()
        .join("");
    serde_json::from_str(&text).unwrap_or(Value::String(text))
}

async fn call(
    client: &RunningService<RoleClient, ()>,
    tool: &'static str,
    args: Value,
) -> rmcp::model::CallToolResult {
    client
        .call_tool(CallToolRequestParams::new(tool).with_arguments(
            args.as_object().cloned().unwrap_or_default(),
        ))
        .await
        .expect("tools/call 不该产生 protocol error")
}

fn modern_meta() -> RequestMetaObject {
    let mut meta = RequestMetaObject::new();
    meta.set_protocol_version(ProtocolVersion::V_2026_07_28);
    meta.set_client_capabilities(ClientCapabilities::default());
    meta
}

#[tokio::test]
async fn auto_client_negotiates_modern_with_zero_fallback() {
    let dir = workdir("auto");
    let client = connect(&dir, auto_lifecycle())
        .await
        .expect("Auto 客户端必须直连成功（零回退）");

    // server/discover 真实回报：只宣告 2026-07-28
    let discovered = client
        .discover(modern_meta())
        .await
        .expect("server/discover 必须可用");
    let versions: Vec<String> = discovered
        .supported_versions
        .iter()
        .map(|v| v.as_str().to_string())
        .collect();
    assert_eq!(versions, vec!["2026-07-28".to_string()], "era 必须 modern-only");
    assert!(
        discovered.capabilities.tools.is_some(),
        "discover 要宣告 tools 能力"
    );

    // 封闭工具集：恰好 4 个（rmcp 的 list_all 已按名排序）
    let tools = client.list_all_tools().await.expect("tools/list");
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    assert_eq!(
        names,
        vec![
            "execute_action",
            "list_capabilities",
            "propose_action",
            "read_environment"
        ],
        "工具集必须与 ToolName 封闭集合一致"
    );

    // schema 里没有任何坐标/凭据/配置面
    let s = serde_json::to_value(&tools).unwrap().to_string();
    for forbidden in ["\"did\"", "\"siid\"", "\"piid\"", "session.json", "HOME_CONTROL"] {
        assert!(!s.contains(forbidden), "工具面泄露 {forbidden}");
    }
    assert!(s.contains("reason"), "propose_action 的 schema 里要有 reason");

    let _ = client.cancel().await;
}

#[tokio::test]
async fn legacy_only_client_is_rejected_by_the_era_gate() {
    // 宪规 18 的反证：哪天有人往 supported_protocol_versions 里加回 legacy，
    // 这条会红——era 闸不是注释里的声明。
    let dir = workdir("legacy");
    let err = connect(&dir, ClientLifecycleMode::Initialize)
        .await
        .expect_err("legacy-only 客户端不该连上 modern-only server");
    let text = err.to_lowercase();
    assert!(
        text.contains("unsupported") || text.contains("32022") || text.contains("protocol version"),
        "失败原因应是协议版本被拒，实际：{err}"
    );
}

#[tokio::test]
async fn policy_surface_behaves_the_same_over_a_real_client() {
    let dir = workdir("policy");
    let client = connect(&dir, auto_lifecycle()).await.expect("connect");

    // 没有云 ⇒ 只读工具也诚实：报 cloud_not_wired，不编造读数
    let res = call(&client, "read_environment", json!({})).await;
    assert_eq!(res.is_error, Some(true));
    assert_eq!(payload(&res)["error"], "cloud_not_wired");

    // 能力清单：友好名 + 值域，无坐标；默认未授权
    let res = call(&client, "list_capabilities", json!({})).await;
    assert_eq!(res.is_error, Some(false));
    let caps = payload(&res);
    let names: Vec<&str> = caps["entities"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["测试深夜灯", "测试风扇"]);
    assert_eq!(caps["grant"]["auto_enabled"], json!(false));

    // 待审批：铸句柄 → approval_required，并落 pending 文件（带 reason，不带 did）
    let res = call(
        &client,
        "propose_action",
        json!({"entity":"测试风扇","action":"set_speed","value":66,"reason":"室温 27.4℃，要开风扇"}),
    )
    .await;
    assert_eq!(res.is_error, Some(false));
    let plan = payload(&res);
    assert_eq!(plan["approval_required"], json!(true));
    assert_eq!(plan["risk_class"], json!("approval"));
    let pid = plan["plan_id"].as_str().unwrap().to_string();

    let pending = std::fs::read_to_string(dir.join("pending.jsonl")).unwrap();
    assert!(pending.contains(&pid), "pending 队列里应有该 plan");
    assert!(pending.contains("室温 27.4℃"), "pending 要带上模型给的理由");
    assert!(!pending.contains("000000000"), "pending 不得带 did");

    // 未批准就执行 ⇒ 云检查在最前面：不烧 plan、不烧批准、不进冷却
    let res = call(&client, "execute_action", json!({"plan_id": pid.clone()})).await;
    assert_eq!(res.is_error, Some(true));
    assert_eq!(payload(&res)["error"], "cloud_not_wired");

    // 全天静默夹具 ⇒ 连句柄都不该铸
    let res = call(&client, "propose_action", json!({"entity":"测试深夜灯","action":"on"})).await;
    assert_eq!(payload(&res)["error"], "quiet_hours");

    // 值域 / reason 上限 / strict-input
    let res = call(
        &client,
        "propose_action",
        json!({"entity":"测试风扇","action":"set_speed","value":0}),
    )
    .await;
    assert_eq!(payload(&res)["error"], "value_out_of_range");

    let res = call(
        &client,
        "propose_action",
        json!({"entity":"测试风扇","action":"off","reason":"x".repeat(201)}),
    )
    .await;
    assert_eq!(payload(&res)["error"], "reason_too_long");

    let res = call(
        &client,
        "propose_action",
        json!({"entity":"测试风扇","action":"off","did":"000000000"}),
    )
    .await;
    assert_eq!(res.is_error, Some(true), "多塞 did 必须被拒");

    let _ = client.cancel().await;
}
