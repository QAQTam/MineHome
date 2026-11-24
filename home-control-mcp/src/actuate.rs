//! 执行编排：plan → 属性写入表 → `set_properties` → 实时回读比对。
//!
//! 放在领域层（不依赖 rmcp），因此可以用假云做单测（DESIGN.md §10.2）：
//! 真实设备冒烟只验证「云端接受并生效」，编排本身的正确性由这里的单测保证，
//! 两者不互相顶包。

use crate::{ActionSpec, CloudError, CloudResult, EntityBinding, HomeCloud, Plan, PropRef, PropSnapshot};
use serde_json::Value;

/// 把 plan 的数值填进 ActionSpec 的写入表。坐标全部来自策略文件。
pub fn build_writes(spec: &ActionSpec, value: Option<f64>) -> Vec<(i64, i64, Value)> {
    spec.writes
        .iter()
        .map(|w| {
            let v = match &w.value {
                Some(literal) => literal.clone(),
                None => {
                    let raw = value.unwrap_or_default();
                    if w.integer {
                        Value::from(raw.round() as i64)
                    } else {
                        Value::from(raw)
                    }
                }
            };
            (w.siid, w.piid, v)
        })
        .collect()
}

/// 回读值与写入值的等价比较（云端可能把 66 报成 66.0）。
pub fn values_match(written: &Value, observed: &Value) -> bool {
    match (written, observed) {
        (Value::Bool(a), Value::Bool(b)) => a == b,
        (Value::String(a), Value::String(b)) => a == b,
        (a, b) => match (a.as_f64(), b.as_f64()) {
            (Some(x), Some(y)) => (x - y).abs() < 1e-6,
            _ => false,
        },
    }
}

pub fn find<'a>(snaps: &'a [PropSnapshot], r: PropRef) -> Option<&'a PropSnapshot> {
    snaps.iter().find(|p| p.siid == r.siid && p.piid == r.piid)
}

/// 一次物理执行的结果。`mismatch` = 回读值与写入值不一致（云端没生效/滞后）。
#[derive(Debug, Clone, Default)]
pub struct ActuationOutcome {
    /// 按 `ActionSpec.verify` 顺序的回读值（可能为空 = 该动作无回读坐标）。
    pub observed: Vec<Value>,
    pub mismatch: bool,
}

/// 执行一个已批准 plan：下发 → 实时回读 → 比对。
///
/// at-most-once：调用方在进入本函数前已消费 plan 句柄；本函数失败时
/// **不重试**（重试会在「动作已发出、响应丢失」时造成重复动作，DESIGN.md §5.1）。
pub async fn run_plan(
    cloud: &dyn HomeCloud,
    binding: &EntityBinding,
    plan: &Plan,
    spec: &ActionSpec,
) -> CloudResult<ActuationOutcome> {
    let writes = build_writes(spec, plan.value);
    cloud.set_properties(&binding.did, &writes).await?;

    if spec.verify.is_empty() {
        return Ok(ActuationOutcome::default());
    }

    // 控制后回读必须走实时穿透：`/miotspec/prop/get` 是云缓存，
    // 刚下发的值可能还是旧的（App 侧同源经验：乐观更新会被缓存值打回）。
    let snaps = match cloud.get_properties_realtime(&binding.did, &spec.verify).await {
        Ok(s) if !s.is_empty() => s,
        _ => cloud.get_properties(&binding.did, &spec.verify).await?,
    };

    let mut observed = Vec::with_capacity(spec.verify.len());
    let mut mismatch = false;
    for r in &spec.verify {
        let snap = find(&snaps, *r).ok_or(CloudError::VerifyEmpty)?;
        if snap.code != 0 {
            return Err(CloudError::Rejected(snap.code));
        }
        if let Some((_, _, written)) = writes.iter().find(|(s, p, _)| *s == r.siid && *p == r.piid) {
            if !values_match(written, &snap.value) {
                mismatch = true;
            }
        }
        observed.push(snap.value.clone());
    }
    Ok(ActuationOutcome { observed, mismatch })
}

// ───────────────────────────── 单测（假云） ─────────────────────────────

#[cfg(test)]
pub mod testutil {
    use super::*;

    /// 与本期真实设备形态同构的测试夹具：p5c 的无级风速 = s8/p1（1..100 整数），
    /// 同时把 s2/p1 开机一起下发（真机验证过的「同时下发」形态）。
    pub fn make_binding(did: &str) -> EntityBinding {
        let spec: ActionSpec = serde_json::from_value(serde_json::json!({
            "writes": [
                {"siid": 8, "piid": 1, "from_request": true, "min": 1.0, "max": 100.0, "integer": true},
                {"siid": 2, "piid": 1, "value": true}
            ],
            "verify": [{"siid": 8, "piid": 1}, {"siid": 2, "piid": 1}]
        }))
        .expect("test action spec");
        let mut actions = std::collections::HashMap::new();
        actions.insert("set_speed".to_string(), spec);
        EntityBinding {
            did: did.to_string(),
            model: "dmaker.fan.p5c".to_string(),
            actions,
            quiet_hours: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::*;
    use super::*;
    use crate::cloud::test_support::{FakeCloud, FakeMode};
    use crate::{ActionRequest, PlanStore};

    fn plan(value: Option<f64>) -> Plan {
        let mut store = PlanStore::default();
        store.mint(
            &ActionRequest {
                entity: "落地扇".into(),
                action: "set_speed".into(),
                value,
                reason: None,
            },
            true,
        )
    }

    #[test]
    fn writes_take_literal_and_request_values() {
        let b = make_binding("1");
        let spec = &b.actions["set_speed"];
        let w = build_writes(spec, Some(66.0));
        assert_eq!(w[0], (8, 1, Value::from(66_i64)));
        assert_eq!(w[1], (2, 1, Value::from(true)));
    }

    #[test]
    fn request_value_is_rounded_for_integer_props() {
        let b = make_binding("1");
        let w = build_writes(&b.actions["set_speed"], Some(65.7));
        assert_eq!(w[0].2, Value::from(66_i64));
    }

    #[test]
    fn values_match_tolerates_number_repr() {
        assert!(values_match(&Value::from(66_i64), &Value::from(66.0)));
        assert!(values_match(&Value::from(true), &Value::from(true)));
        assert!(!values_match(&Value::from(true), &Value::from(1_i64)));
        assert!(!values_match(&Value::from(66_i64), &Value::from(35_i64)));
        assert!(!values_match(&Value::Null, &Value::Null));
    }

    #[tokio::test]
    async fn run_plan_sets_then_reads_back_and_agrees() {
        let cloud = FakeCloud::new();
        let b = make_binding("900000010");
        cloud.seed("900000010", 8, 1, Value::from(35_i64));
        cloud.seed("900000010", 2, 1, Value::from(false));

        let out = run_plan(&cloud, &b, &plan(Some(66.0)), &b.actions["set_speed"])
            .await
            .expect("run");
        assert!(!out.mismatch, "回读应与写入一致: {:?}", out.observed);
        assert_eq!(out.observed, vec![Value::from(66_i64), Value::from(true)]);
        assert_eq!(cloud.writes_for("900000010").len(), 1, "只下发一次");
    }

    #[tokio::test]
    async fn run_plan_reports_mismatch_when_device_did_not_apply() {
        // Frozen = 云端接受了写入但设备状态没变（网关掉线/离线缓存的典型故障）
        let cloud = FakeCloud::with_mode(FakeMode::Frozen);
        let b = make_binding("900000010");
        cloud.seed("900000010", 8, 1, Value::from(35_i64));
        cloud.seed("900000010", 2, 1, Value::from(true));

        let out = run_plan(&cloud, &b, &plan(Some(66.0)), &b.actions["set_speed"])
            .await
            .expect("run");
        assert!(out.mismatch, "回读仍是旧值必须报 mismatch");
        assert_eq!(out.observed, vec![Value::from(35_i64), Value::from(true)]);
    }

    #[tokio::test]
    async fn run_plan_fails_closed_when_cloud_rejects_write() {
        let cloud = FakeCloud::new();
        cloud.fail_writes();
        let b = make_binding("900000010");
        let e = run_plan(&cloud, &b, &plan(Some(66.0)), &b.actions["set_speed"])
            .await
            .expect_err("must fail");
        assert_eq!(e.code(), "write_rejected");
        assert!(cloud.writes_for("900000010").is_empty(), "被拒的写入不该记成成功");
    }

    #[tokio::test]
    async fn run_plan_fails_when_readback_missing() {
        // 读值恒空：不能谎报「已生效」
        let cloud = FakeCloud::with_mode(FakeMode::NoReads);
        let b = make_binding("900000010");
        let e = run_plan(&cloud, &b, &plan(Some(66.0)), &b.actions["set_speed"])
            .await
            .expect_err("must fail");
        assert_eq!(e.code(), "verify_empty");
    }
}
