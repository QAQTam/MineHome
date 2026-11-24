//! stdio MCP server 接线层（ADR-0024 / DESIGN.md）。
//!
//! 本层只做四件事：rmcp 工具签名、策略/云调用的编排、审计落盘、错误码投影。
//! **所有风险判定都在 `home_control_mcp`（纯领域层）里**，这里不参与判断。
//!
//! 协议 era = MCP `2026-07-28` Modern stateless only：只宣告 2026-07-28，
//! 不实现 `initialize`、不发 `Mcp-Session-Id`（override `supported_protocol_versions`）。
//!
//! 凭据口径（DESIGN.md §7）：env 里只有**路径**（session.json / policy.json /
//! 审计 JSONL / approvals）；`stdout` 只写 JSON-RPC，日志一律 `stderr`；
//! 工具结果里永不出现 did/siid/piid/凭据。

use home_control_mcp::{
    ActionRequest, Allowlist, AutoPolicy, CloudError, Cooldown, Environment, HomeCloud,
    MonitorRef, Plan, PlanStore, Policy, PolicyError, PolicyFile, PropRef, Reading, SessionGrant,
    TempSensor, ToolName, CLOUD_SOURCE, actuate, classify, may_auto_execute, now_ms, now_secs,
    policy_error_code, session, session::SessionHealth, to_epoch_ms,
};
use home_control_mcp::cloud::MijiaCloud;
use rmcp::{
    ErrorData as McpError, ServerHandler, ServiceExt,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock, ProtocolVersion},
    schemars, tool, tool_handler, tool_router,
    transport::stdio,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

/// 工具封闭集合的一致性断言（DESIGN.md §3）：`main.rs` 加了 `#[tool]` 却没同步
/// `ToolName`（或反之），启动即炸，而不是静默把多出来的能力暴露给模型。
fn assert_closed_toolset() {
    let mut exposed: Vec<String> = HomeServer::tool_router()
        .list_all()
        .into_iter()
        .map(|t| t.name.to_string())
        .collect();
    exposed.sort();
    let mut expected: Vec<String> = ToolName::ALL.iter().map(|t| t.as_str().to_owned()).collect();
    expected.sort();
    assert_eq!(exposed, expected, "工具集与 ToolName 封闭集合不一致");
}

/// `execute_action` 入参：只认 server-minted handle（SEP-2567）。
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct ExecuteArgs {
    plan_id: String,
}

/// 业务成功：`isError=false` + JSON 文本。
fn ok(data: Value) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::success(vec![ContentBlock::text(data.to_string())]))
}

/// 业务失败：**tool-level error**（`isError=true`，模型可见并纠正）。
/// 本 server 不产生 protocol error（DESIGN.md §6）。
fn fail(data: Value) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::error(vec![ContentBlock::text(data.to_string())]))
}

fn tool_err(detail: impl std::fmt::Display, code: &str) -> Value {
    json!({ "error": code, "detail": detail.to_string() })
}

fn perr(e: &PolicyError) -> Value {
    tool_err(e, policy_error_code(e))
}

fn cerr(e: &CloudError) -> Value {
    tool_err(e, e.code())
}

#[derive(Clone)]
struct HomeServer {
    allow: Arc<Allowlist>,
    plans: Arc<Mutex<PlanStore>>,
    policy: Arc<Mutex<AutoPolicy>>,
    cooldown: Arc<Mutex<Cooldown>>,
    grant: SessionGrant,
    sensor: Arc<Option<TempSensor>>,
    monitors: Arc<Vec<(String, MonitorRef)>>,
    /// `None` = 没配 `HOME_CONTROL_SESSION`；需要云的工具一律报 `cloud_not_wired`。
    cloud: Option<Arc<dyn HomeCloud>>,
    /// 审计 JSONL 路径（`HOME_CONTROL_AUDIT`）；缺省 = 只打 stderr。
    audit_path: Arc<Option<PathBuf>>,
    /// 带外批准句柄文件（`HOME_CONTROL_APPROVALS`，DESIGN.md §5.2）。
    approvals_path: Arc<Option<PathBuf>>,
    /// 待审批队列（`HOME_CONTROL_PENDING`，DESIGN.md §5.4）：判为需审批的 plan
    /// 追加到这里，供审批 UI / 人工读取；`execute_action` 一旦受理就摘掉。
    /// 模型没有任何工具能读或写它——它不是控制面，是给人看的窗口。
    pending_path: Arc<Option<PathBuf>>,
    /// 允许用 `passToken` 静默续期（`HOME_CONTROL_ALLOW_REFRESH`，**默认关**）。
    /// 关着的时候凭据过期就是纯 `need_relogin`，server 一个账号域调用都不发（§7.6）。
    allow_refresh: bool,
    /// 每进程最多续期一次（`session::may_refresh` 的第 4 个条件）。
    refresh_attempted: Arc<AtomicBool>,
    /// 凭据已被**证明**不可用（续期失败/续期后仍 401）⇒ 持续报 `need_relogin`，
    /// 不让它退化成 `cloud_transport` 噪声（§7.6）。
    session_dead: Arc<AtomicBool>,
}

// 第一步：只生成 inherent impl + tool_router()。
// 不用 `#[tool_router(server_handler)]`，因为要显式 override supported_protocol_versions。
#[tool_router]
impl HomeServer {
    #[tool(
        description = "读取家中环境观测值：温度/湿度/传感器电量 + 只读监视器（门磁接触态、光照等）。返回带 source 与上报时间戳的「事实」；用户说「有点热」是意图不是事实。无参数，零副作用。"
    )]
    async fn read_environment(&self) -> Result<CallToolResult, McpError> {
        let Some(cloud) = self.cloud.as_ref() else {
            return fail(tool_err(CloudError::NotWired, CloudError::NotWired.code()));
        };
        let mut env = Environment {
            temperature_c: None,
            humidity_pct: None,
            battery_pct: None,
            reported_at_ms: None,
            monitors: Vec::new(),
            source: CLOUD_SOURCE,
            observed_at_ms: now_ms(),
        };

        // ① 主传感器：一次批量读（本期真机坐标 = 温湿度计3 mini 的自定义属性
        //    s2/p1001 温度、s2/p1002 湿度、s3/p1003 电量；标准 s2/p1|p2 会报 -704040003）
        if let Some(ts) = self.sensor.as_ref().as_ref() {
            let props = ts.props();
            // 只读调用：疑似凭据过期 ⇒ 允许时静默续期一次 ⇒ 重发一次（§7.6）
            let snaps = match self
                .read_once(Arc::clone(cloud), ts.did.clone(), props.clone())
                .await
            {
                Ok(s) => s,
                Err(e) => return fail(cerr(&e)),
            };
            let mut usable = false;
            for r in &props {
                let Some(snap) = actuate::find(&snaps, *r) else {
                    continue;
                };
                if snap.code != 0 {
                    continue; // 单属性失败 ⇒ 该字段留空，绝不填猜测值（宪规 5）
                }
                // 温度按 spec `value-range` 的 step=0.1 归整：f32 直读会吐出
                // 27.399999618530273 这种噪声，模型不需要看见它。
                let v = snap
                    .value
                    .as_f64()
                    .map(|f| if r.siid == ts.siid_temp && r.piid == ts.piid_temp {
                        (f * 10.0).round() / 10.0
                    } else {
                        f
                    });
                usable |= v.is_some();
                if let Some(t) = snap.update_time {
                    let ms = to_epoch_ms(t);
                    if ms > env.reported_at_ms.unwrap_or(0) {
                        env.reported_at_ms = Some(ms);
                    }
                }
                if r.siid == ts.siid_temp && r.piid == ts.piid_temp {
                    env.temperature_c = v;
                } else if Some(r.siid) == ts.siid_humidity && Some(r.piid) == ts.piid_humidity {
                    env.humidity_pct = v;
                } else if Some(r.siid) == ts.siid_battery && Some(r.piid) == ts.piid_battery {
                    env.battery_pct = v;
                }
            }
            if !usable {
                return fail(tool_err(
                    format!("{} 个属性都没读出数字（设备离线或云缓存为空）", props.len()),
                    "sensor_read_failed",
                ));
            }
        } else if self.monitors.is_empty() {
            return fail(tool_err(PolicyError::NoTempSensor, "no_temp_sensor"));
        }

        // ② 只读监视器：按 did 分组，一台设备一次批量读
        let mut grouped: Vec<(String, Vec<(String, PropRef)>)> = Vec::new();
        for (name, m) in self.monitors.iter() {
            let r = PropRef { siid: m.siid, piid: m.piid };
            match grouped.iter_mut().find(|(did, _)| *did == m.did) {
                Some((_, v)) => v.push((name.clone(), r)),
                None => grouped.push((m.did.clone(), vec![(name.clone(), r)])),
            }
        }
        for (did, refs) in grouped {
            let props: Vec<PropRef> = refs.iter().map(|(_, r)| *r).collect();
            let snaps = self
                .read_once(Arc::clone(cloud), did.clone(), props)
                .await
                .ok();
            for (name, r) in &refs {
                let reading = match snaps.as_ref().and_then(|s| actuate::find(s, *r)) {
                    Some(s) if s.code == 0 => Reading {
                        name: name.clone(),
                        value: Some(s.value.clone()),
                        reported_at_ms: s.update_time.map(to_epoch_ms),
                        error: None,
                    },
                    Some(_) => Reading {
                        name: name.clone(),
                        value: None,
                        reported_at_ms: None,
                        error: Some("read_failed"),
                    },
                    None => Reading {
                        name: name.clone(),
                        value: None,
                        reported_at_ms: None,
                        error: Some("no_value"),
                    },
                };
                env.monitors.push(reading);
            }
        }

        match serde_json::to_value(&env) {
            Ok(v) => ok(v),
            Err(e) => fail(tool_err(e, "shape_invalid")),
        }
    }

    #[tool(
        description = "列出可操作的实体（友好名 + 友好动作 + 值域 + 是否静默时段）与只读监视器名。绝不返回 did/siid/piid/凭据。grant.auto_enabled 表示本会话是否已授权低风险动作自动执行。"
    )]
    async fn list_capabilities(&self) -> Result<CallToolResult, McpError> {
        let entities: Vec<Value> = self
            .allow
            .names()
            .into_iter()
            .map(|name| {
                let b = &self.allow.0[name];
                let mut action_names: Vec<&String> = b.actions.keys().collect();
                action_names.sort();
                json!({
                    "name": name,
                    "actions": action_names.into_iter().map(|a| {
                        let s = &b.actions[a];
                        let w = s.writes.iter().find(|w| w.from_request);
                        json!({
                            "name": a,
                            "description": s.description,
                            "requires_value": s.takes_request_value(),
                            "value_range": w.map(|w| json!({
                                "min": w.min, "max": w.max,
                                "integer": w.integer,
                            })),
                            "needs_approval": !self
                                .policy
                                .lock()
                                .expect("policy mutex poisoned")
                                .auto_actions
                                .contains(&(name.clone(), a.clone())),
                        })
                    }).collect::<Vec<_>>(),
                    "quiet_hours": b.quiet_hours.is_some(),
                })
            })
            .collect();
        ok(json!({
            "entities": entities,
            "monitors": self.monitors.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>(),
            "temp_sensor_configured": self.sensor.as_ref().is_some(),
            "grant": { "auto_enabled": self.grant.auto_enabled },
            "cooldown_secs": self
                .policy
                .lock()
                .expect("policy mutex poisoned")
                .cooldown_secs,
            "protocol_note": "plan_id 由 propose_action 铸造，一次性消费",
        }))
    }

    #[tool(
        description = "提交一个物理动作意图（友好实体 + 友好动作 + 可选数值 + 可选 reason），返回一次性 plan handle。**本工具不产生任何物理动作**。reason 只给审批人看（≤200 字）且**不影响风险判定**；免审批与否由策略层的封闭白名单 + 会话授权开关决定，模型不能自批。"
    )]
    async fn propose_action(
        &self,
        Parameters(req): Parameters<ActionRequest>,
    ) -> Result<CallToolResult, McpError> {
        let class = {
            let policy = self.policy.lock().expect("policy mutex poisoned");
            match classify(&req, &self.allow, &policy, now_secs()) {
                Ok(c) => c,
                Err(e) => return fail(perr(&e)),
            }
        };
        // 风险判定在策略层；「低风险自动」还需每会话授权开关（DESIGN.md §4.1）。
        let approved = may_auto_execute(class, self.grant);
        let plan = self
            .plans
            .lock()
            .expect("plan store mutex poisoned")
            .mint(&req, approved);
        if !plan.approved {
            self.append_pending(&plan);
        }
        ok(json!({
            "plan_id": plan.plan_id,
            "entity": plan.entity,
            "action": plan.action,
            "value": plan.value,
            "risk_class": if class == home_control_mcp::RiskClass::Auto { "auto" } else { "approval" },
            "approved": plan.approved,
            "approval_required": !plan.approved,
            "next": if plan.approved {
                "execute_action(plan_id) 即可执行；句柄一次性，重复调用必定失败"
            } else {
                "已进入待审批队列；需用户批准（plan_id 写入 HOME_CONTROL_APPROVALS）后才能 execute_action"
            },
        }))
    }

    #[tool(
        description = "执行一个已批准的 plan（只接受 propose_action 铸造的 plan_id；句柄一次性消费 = 幂等键）。下发后实时回读比对并写审计；失败不回放。"
    )]
    async fn execute_action(
        &self,
        Parameters(args): Parameters<ExecuteArgs>,
    ) -> Result<CallToolResult, McpError> {
        // ① 云侧不可用时不消费 plan、不标记冷却，避免「消费了却没执行」的不一致
        let Some(cloud) = self.cloud.clone() else {
            return fail(tool_err(CloudError::NotWired, CloudError::NotWired.code()));
        };
        // ② 先看不吃：批准/冷却失败时 plan 必须还活着
        let Some(plan) = self
            .plans
            .lock()
            .expect("plan store mutex poisoned")
            .peek(&args.plan_id)
            .cloned()
        else {
            return fail(perr(&PolicyError::UnknownPlan(args.plan_id.clone())));
        };
        let binding = match self.allow.resolve(&plan.entity) {
            Ok(b) => b,
            Err(e) => return fail(perr(&e)),
        };
        // ③ 静默时段在**执行时**再判一次（propose 与 execute 之间可能跨点）
        if let Some(q) = &binding.quiet_hours {
            if q.contains(now_secs()) {
                return fail(perr(&PolicyError::QuietHours));
            }
        }
        // ④ 冷却窗口先看一眼（不标记）：放在批准消费之前，避免冷却把带外批准烧掉
        if let Err(e) = self
            .cooldown
            .lock()
            .expect("cooldown mutex poisoned")
            .check(&plan.entity)
        {
            return fail(perr(&e));
        }
        // ⑤ 批准 = propose 时策略已放行，或带外批准句柄（内存池 / approvals 文件）
        if !plan.approved {
            let in_pool = self
                .policy
                .lock()
                .expect("policy mutex poisoned")
                .consume_approval(&args.plan_id);
            if !in_pool && !self.consume_file_approval(&args.plan_id) {
                return fail(perr(&PolicyError::ApprovalRequired(plan.plan_id.clone())));
            }
        }
        let spec = match binding.actions.get(&plan.action) {
            Some(s) => s.clone(),
            None => return fail(perr(&PolicyError::UnknownAction(plan.entity, plan.action))),
        };
        // ⑥ 一次性消费句柄 + 打冷却标记，二者同批完成。
        //    此后任何失败都**不回放**（at-most-once，DESIGN.md §5.1）
        let plan = {
            let mut cooldown = self.cooldown.lock().expect("cooldown mutex poisoned");
            let mut plans = self.plans.lock().expect("plan store mutex poisoned");
            let taken = plans.take(&args.plan_id);
            match taken {
                Ok(p) => {
                    cooldown.mark(&p.entity);
                    p
                }
                Err(e) => return fail(perr(&e)),
            }
        };

        // 已决（无论后续成败）⇒ 从待审批队列摘掉，队列里不留过期项
        self.drop_pending(&args.plan_id);

        match actuate::run_plan(cloud.as_ref(), binding, &plan, &spec).await {
            Ok(out) if out.mismatch => {
                self.audit(&plan, false, Some("verify_mismatch"), Some(out.observed.clone()));
                fail(tool_err(
                    "云端接受了写入，但实时回读与期望值不一致（设备可能未生效）",
                    "verify_mismatch",
                ))
            }
            Ok(out) => {
                self.audit(&plan, true, None, Some(out.observed.clone()));
                ok(json!({
                    "plan_id": plan.plan_id,
                    "entity": plan.entity,
                    "action": plan.action,
                    "value": plan.value,
                    "ok": true,
                    "observed_after": out.observed,
                    "at_ms": now_ms(),
                }))
            }
            Err(e) => {
                // 云调用失败：plan 不回放，审计记 ok=false，由用户重新 propose。
                // 若形状像凭据过期且允许续期 ⇒ **只把会话换回来，不重发这次写**
                // （§7.6：写调用的响应丢失时无法区分「没送达」与「已执行」）。
                let refreshed = session::refresh_without_reissue(
                    &cloud,
                    &e,
                    self.allow_refresh,
                    &self.refresh_attempted,
                    &self.session_dead,
                )
                .await;
                self.audit(&plan, false, Some(e.code()), None);
                let mut out = cerr(&e);
                if refreshed {
                    out["detail"] = json!(format!(
                        "{}（已用 passToken 换回新会话；本次动作**未重发**，请重新 propose）",
                        out["detail"].as_str().unwrap_or_default()
                    ));
                }
                fail(out)
            }
        }
    }
}

impl HomeServer {
    /// 一次批量属性读，带 §7.6 的凭据语义：疑似过期 ⇒ （允许时）静默续期一次 ⇒
    /// 只读调用可以重发一次。**写调用绝不走这里**（at-most-once，§5.1）。
    async fn read_once(
        &self,
        cloud: Arc<dyn HomeCloud>,
        did: String,
        props: Vec<PropRef>,
    ) -> home_control_mcp::CloudResult<Vec<home_control_mcp::PropSnapshot>> {
        session::read_with_refresh(
            cloud,
            self.allow_refresh,
            &self.refresh_attempted,
            &self.session_dead,
            move |c: Arc<dyn HomeCloud>| {
                let did = did.clone();
                let props = props.clone();
                async move { c.get_properties(&did, &props).await }
            },
        )
        .await
    }

    /// 带外批准句柄：从 approvals 文件里吃掉这一行（一次性的持久形式）。
    /// 该文件由 QAQH 审批 UI 或人工写入，模型没有写权限 ⇒ 模型无法自批自审。
    fn consume_file_approval(&self, plan_id: &str) -> bool {
        let Some(path) = self.approvals_path.as_ref().as_ref() else {
            return false;
        };
        let Ok(text) = std::fs::read_to_string(path) else {
            return false;
        };
        let lines: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
        if !lines.iter().any(|l| *l == plan_id) {
            return false;
        }
        let kept: String = lines
            .iter()
            .filter(|l| **l != plan_id)
            .map(|l| format!("{l}\n"))
            .collect();
        match std::fs::write(path, kept) {
            Ok(()) => true,
            Err(e) => {
                // 删不掉 = 同一个批准可能被用两次 ⇒ 宁可不放行（fail closed）
                eprintln!("[home-control] approvals rewrite failed: {e}");
                false
            }
        }
    }

    /// 待审批队列 append（一行一 JSON）。审批方读它才能知道「在等什么、为什么」，
    /// 而不是只靠模型转述。
    fn append_pending(&self, plan: &Plan) {
        let Some(path) = self.pending_path.as_ref().as_ref() else {
            return;
        };
        let line = json!({
            "plan_id": plan.plan_id,
            "entity": plan.entity,
            "action": plan.action,
            "value": plan.value,
            "reason": plan.reason,
            "asked_at_ms": plan.created_at_ms,
        })
        .to_string();
        if let Err(e) = append_line(path, &line) {
            eprintln!("[home-control] pending append failed: {e}");
        }
    }

    /// 从待审批队列里删掉某 plan_id（整文件重写；队列本来就只装几个待决项）。
    fn drop_pending(&self, plan_id: &str) {
        let Some(path) = self.pending_path.as_ref().as_ref() else {
            return;
        };
        let Ok(text) = std::fs::read_to_string(path) else {
            return;
        };
        let lines: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
        let kept: Vec<&&str> = lines
            .iter()
            .filter(|l| {
                serde_json::from_str::<Value>(l)
                    .ok()
                    .and_then(|v| v.get("plan_id").and_then(|p| p.as_str().map(str::to_owned)))
                    .map(|id| id != plan_id)
                    .unwrap_or(true)
            })
            .collect();
        if kept.len() == lines.len() {
            return; // 没命中就不碰文件
        }
        let body: String = kept.into_iter().map(|l| format!("{l}\n")).collect();
        if let Err(e) = std::fs::write(path, body) {
            eprintln!("[home-control] pending rewrite failed: {e}");
        }
    }

    /// 审计：JSONL append（友好名 + plan_id + 观测值；**无 did/siid/piid/凭据**）。
    fn audit(
        &self,
        plan: &Plan,
        ok_flag: bool,
        code: Option<&'static str>,
        observed: Option<Vec<Value>>,
    ) {
        let rec = home_control_mcp::AuditRecord {
            plan_id: plan.plan_id.clone(),
            entity: plan.entity.clone(),
            action: plan.action.clone(),
            value: plan.value,
            reason: plan.reason.clone(),
            at_ms: now_ms(),
            ok: ok_flag,
            error: code,
            observed_after: observed,
        };
        let line = match serde_json::to_string(&rec) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("[home-control] audit serialize failed: {e}");
                return;
            }
        };
        match self.audit_path.as_ref().as_ref() {
            Some(path) => {
                if let Err(e) = append_line(path, &line) {
                    eprintln!("[home-control] audit append failed: {e}");
                }
            }
            None => eprintln!("[home-control] audit(no file) {line}"),
        }
    }
}

fn append_line(path: &Path, line: &str) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)?;
        }
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(f, "{line}")
}

// 第二步：显式 impl 块。宏补 call_tool / list_tools / get_info / get_tool，
// 本方法 override 版本列表 ⇒ 默认 `discover` 只宣告 2026-07-28（modern-only）。
#[tool_handler(
    name = "home-control-mcp",
    instructions = "只读：read_environment、list_capabilities。写：propose_action（不产生动作）→ execute_action（需策略免审批或用户带外批准）。物理动作不可自批自审；不要猜 did/坐标，用友好名。"
)]
impl ServerHandler for HomeServer {
    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Borrowed(&[ProtocolVersion::V_2026_07_28])
    }
}

/// 云侧从 env 拿 **session.json 的路径**，凭据本体不进 env（DESIGN.md §7）。
/// 缺配置 / 构造失败 → None，工具返回 `cloud_not_wired`，不静默放行。
fn load_cloud() -> Option<Arc<dyn HomeCloud>> {
    let path = std::env::var("HOME_CONTROL_SESSION").ok()?;
    match MijiaCloud::new(&path) {
        Ok(cloud) => Some(Arc::new(cloud)),
        Err(e) => {
            // stdio 规范：日志走 stderr，stdout 只写 MCP JSON-RPC。
            eprintln!("[home-control] cloud init failed: {} (code={})", e, e.code());
            None
        }
    }
}

fn env_path(key: &str) -> Arc<Option<PathBuf>> {
    Arc::new(
        std::env::var(key)
            .ok()
            .map(PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty()),
    )
}

/// `policy.json` 从 env 拿**路径**。缺配置 = 空策略（所有动作 `unknown_entity`）；
/// 但策略**内容越界**（如把空调写进 allowlist）= 拒绝启动，不降级（DESIGN.md §4.3）。
fn load_policy() -> Result<Policy, String> {
    let Ok(path) = std::env::var("HOME_CONTROL_POLICY") else {
        eprintln!("[home-control] HOME_CONTROL_POLICY 未配置 ⇒ allowlist 为空");
        return PolicyFile::default().into_policy();
    };
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("读取 {path} 失败: {e}"))?;
    let file: PolicyFile =
        serde_json::from_str(&text).map_err(|e| format!("policy.json 解析失败: {e}"))?;
    file.into_policy()
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    assert_closed_toolset();
    let policy = match load_policy() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[home-control] policy rejected, refusing to start: {e}");
            std::process::exit(2);
        }
    };
    let server = HomeServer {
        allow: Arc::new(policy.allow),
        plans: Arc::new(Mutex::new(PlanStore::default())),
        policy: Arc::new(Mutex::new(policy.auto)),
        cooldown: Arc::new(Mutex::new(policy.cooldown)),
        grant: policy.grant,
        sensor: Arc::new(policy.temp_sensor),
        monitors: Arc::new(policy.monitors),
        cloud: load_cloud(),
        audit_path: env_path("HOME_CONTROL_AUDIT"),
        approvals_path: env_path("HOME_CONTROL_APPROVALS"),
        pending_path: env_path("HOME_CONTROL_PENDING"),
        allow_refresh: std::env::var("HOME_CONTROL_ALLOW_REFRESH")
            .map(|v| matches!(v.trim(), "1" | "true" | "yes" | "on"))
            .unwrap_or(false),
        refresh_attempted: Arc::new(AtomicBool::new(false)),
        session_dead: Arc::new(AtomicBool::new(false)),
    };
    // 会话健康**必须实测**，不能拿「文件里有 session」当结论（§7.6）：
    // 过期的 serviceToken 在 `has_session_file()` 里一样返回 true。
    let health = match server.cloud.as_ref() {
        None => SessionHealth::Unknown("未配置 HOME_CONTROL_SESSION".into()),
        Some(c) => match c.probe().await {
            Ok(()) => SessionHealth::Alive,
            Err(e) if session::looks_like_expired_credentials(&e) => {
                SessionHealth::Expired(e.code().into())
            }
            Err(e) => SessionHealth::Unknown(e.code().into()),
        },
    };
    eprintln!(
        "[home-control] start: entities={} monitors={} cloud={} session={} has_file={} allow_refresh={} auto_enabled={}",
        server.allow.0.len(),
        server.monitors.len(),
        server.cloud.is_some(),
        health.label(),
        match server.cloud.as_ref() {
            Some(c) => c.has_session_file().await,
            None => false,
        },
        server.allow_refresh,
        server.grant.auto_enabled,
    );
    if !matches!(health, SessionHealth::Alive) {
        eprintln!(
            "[home-control] 会话状态={:?}；静默续期={}",
            health,
            if server.allow_refresh { "开（首次疑似过期时续一次）" } else { "关（需人工重新登录）" }
        );
    }
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
