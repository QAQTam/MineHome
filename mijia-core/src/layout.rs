//! 首页「自定义卡片」数据层（轻档，2026-11-17 决策 q1=轻档）。
//!
//! 用户在设备上勾选「这台设备首页要显示哪几项、按什么顺序」，本层把这份选择
//! 应用到归一化卡片 DTO 上。**刻意只做两件事**：
//!
//! 1. **过滤**：没被勾选的项不进首页 DTO；
//! 2. **排序**：首页 DTO 顺序 = 用户给的顺序。
//!
//! 控件形态（`kind`）一律由模板 + spec 决定（`CardEngine` 的既有职责），本层
//! 不提供任何"把它改成滑条/开关"的能力——用户选错类型会把写值形态搞坏（圆钮
//! 写值就吃过只认 boolean 的亏），所以这个口子根本不开。
//!
//! 为什么放 Rust 而不是 ArkTS：这条逻辑必须能被纯 CPU 测试拦住（开放外部 PR 时
//! 不能要求贡献者有账号或模拟器）。ArkTS 只负责把勾出来的 key 列表传进来，
//! 再按返回顺序渲染。
//!
//! 一个关键取舍（第一版设计错了，重写记录在此）：**不能把"layout 里没提到的项"
//! 一律追加尾部**，那样用户取消勾选的项会被追回来，裁剪直接失效。所以要用两份
//! 名单区分两种"没提到"：
//! - 在 `known`（保存那次编辑器里见过的全部项）里却没勾 → 用户**明确不要** → 隐藏；
//! - 不在 `known` 里 → 用户当时**还没见过**（模板/spec 改版新增）→ 追加尾部并回报数量，
//!   由 UI 提示「有 N 个新项已加进来」。

use crate::card::{CardItemVm, CardKind};

/// 哨兵 key：用户显式把这台设备整张卡从首页隐藏。
/// 用哨兵而不是再加一个 bool 字段，是因为它天然进 `visible` 之外的同一份存储，
/// 少一个会互相矛盾的状态源。
pub const HIDE_MARK: &str = "__hide__";

/// 用户选择。`None` = 从未自定义过 → 首页行为与今天完全一致。
#[derive(Debug, Clone, Default)]
pub struct CardLayout {
    /// 勾选要显示的项，按用户给的显示顺序排列（**白名单**）。
    pub visible: Vec<String>,
    /// 保存那次编辑时见到的全部 key 快照，用来区分"明确不要"与"还没见过"。
    pub known: Vec<String>,
}

impl CardLayout {
    /// 是否被显式隐藏。
    pub fn is_hidden(&self) -> bool {
        self.visible.iter().any(|k| k == HIDE_MARK)
    }
}

/// DTO 项的稳定 key：`kind:siid:piid`。
///
/// 带上 kind 是因为同一个 `(siid,piid)` 可能同时以 `toggle` 与 `observe_number`
/// 出现（模板把 `on` 又当状态输出一遍），只用地址会互相顶掉。
pub fn key_of(it: &CardItemVm) -> String {
    key_of_parts(it.kind, it.siid, it.piid)
}

/// UI 侧构造 key（同一份拼接规则，不让 ArkTS 自己拼字符串）。
pub fn key_of_parts(kind: CardKind, siid: i64, piid: i64) -> String {
    format!("{}:{}:{}", kind.as_str(), siid, piid)
}

/// 应用用户选择。返回 `(要显示的项, 自动补进来的新项数)`。
///
/// 容错口径（都为"别让用户的一次勾选变成空白卡片"服务）：
/// - `visible` 里查无此 key（该项已从模板消失）→ 忽略，不报错、不留空位；
/// - `layout = None` 或 `visible` 为空 → 原样返回（**空名单当"没自定义"而不是"全隐藏"**，
///   避免一次误操作或存储损坏把整页清空；真要全隐藏走 `HIDE_MARK`）；
/// - `is_hidden()` → 返回空列表。
pub fn apply(items: Vec<CardItemVm>, layout: Option<&CardLayout>) -> (Vec<CardItemVm>, usize) {
    let Some(l) = layout else {
        return (items, 0);
    };
    if l.visible.is_empty() || items.is_empty() {
        return (items, 0);
    }
    if l.is_hidden() {
        return (Vec::new(), 0);
    }

    let mut picked: Vec<CardItemVm> = Vec::with_capacity(items.len());
    let mut used: Vec<bool> = vec![false; items.len()];
    // 已消费过的 key：visible 里出现重复 key（存储被手改/上游 UI bug）时，
    // 不能再把撞 key 的另一个项也捞出来，否则首页会出现两张一样的卡。
    let mut consumed: Vec<String> = Vec::with_capacity(l.visible.len());

    // 1) 按用户顺序取；每个 key 最多消费一个项
    for want in &l.visible {
        if want == HIDE_MARK || consumed.iter().any(|k| k == want) {
            continue;
        }
        for (i, it) in items.iter().enumerate() {
            if !used[i] && key_of(it) == *want {
                picked.push(items[i].clone());
                used[i] = true;
                consumed.push(want.clone());
                break;
            }
        }
    }

    // 2) 用户当时还没见过的新项追加尾部（保持模板内部相对顺序）；
    //    在 known 里却没被勾选的，视为用户明确排除，永不补回。
    let mut appended = 0usize;
    for (i, it) in items.iter().enumerate() {
        if used[i] {
            continue;
        }
        if l.known.iter().any(|k| *k == key_of(it)) {
            continue; // 明确不要
        }
        picked.push(it.clone());
        appended += 1;
    }
    (picked, appended)
}

/// 编辑器列表：把当前 DTO 摊平成 `(key, label, kind)` 供 UI 渲染勾选框。
/// 放在 Rust 是为了 ArkTS 不必自己拼 key（拼错就勾不上）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct LayoutOption {
    pub key: String,
    pub label: String,
    pub kind: String,
    /// 当前是否会被显示（勾上与否的初始值）
    pub checked: bool,
}

pub fn options(items: &[CardItemVm], layout: Option<&CardLayout>) -> Vec<LayoutOption> {
    let Some(l) = layout else {
        return items
            .iter()
            .map(|it| LayoutOption {
                key: key_of(it),
                label: it.label.clone(),
                kind: it.kind.as_str().to_string(),
                checked: true,
            })
            .collect();
    };
    let mut out: Vec<LayoutOption> = items
        .iter()
        .map(|it| {
            let k = key_of(it);
            // 没自定义过（visible 空）→ 全勾；自定义过 → 勾上等于在 visible 里
            let checked = l.visible.is_empty() || l.visible.iter().any(|v| *v == k);
            LayoutOption {
                key: k,
                label: it.label.clone(),
                kind: it.kind.as_str().to_string(),
                checked,
            }
        })
        .collect();
    // 编辑器行序必须等于首页实际顺序：勾选项按 visible 排位，未勾选项保持模板序殿后。
    // 否则「重开编辑器 → 原样保存」会把 visible 重写成模板序，用户排好的序（以及刚导入
    // 档案里的序）会被静默打乱。sort_by_key 是稳定排序，未勾项之间相对序不变。
    out.sort_by_key(|o| {
        if !o.checked {
            return usize::MAX;
        }
        l.visible.iter().position(|v| *v == o.key).unwrap_or(usize::MAX)
    });
    out
}

// ---------------------------------------------------------------------- 测试 ----

#[cfg(test)]
mod tests {
    use super::*;

    fn it(kind: CardKind, siid: i64, piid: i64, label: &str) -> CardItemVm {
        CardItemVm {
            kind,
            siid,
            piid,
            label: label.into(),
            unit: String::new(),
            format: String::new(),
            ratio: 1.0,
            choices: vec![],
            disabled_when: vec![],
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
        }
    }

    fn sample() -> Vec<CardItemVm> {
        vec![
            it(CardKind::Toggle, 2, 1, "开关"),
            it(CardKind::EnumChoice, 2, 2, "模式"),
            it(CardKind::Stepper, 2, 4, "设定温度"),
            it(CardKind::ObserveNumber, 4, 7, "室内温度"),
        ]
    }

    fn keys(items: &[CardItemVm]) -> Vec<String> {
        items.iter().map(key_of).collect()
    }

    /// 常见编辑结果：只留「开关 + 室内温度」，顺序用户定
    fn trimmed() -> CardLayout {
        CardLayout {
            visible: vec!["toggle:2:1".into(), "observe_number:4:7".into()],
            known: sample().iter().map(key_of).collect(),
        }
    }

    #[test]
    fn 没自定义时行为零变化() {
        let (out, appended) = apply(sample(), None);
        assert_eq!(out.len(), 4);
        assert_eq!(appended, 0);
        assert_eq!(keys(&out), vec![
            "toggle:2:1",
            "enum_choice:2:2",
            "stepper:2:4",
            "observe_number:4:7"
        ]);
        // visible 为空 = 当作"没自定义"，绝不因为一次误清空把整页变空白
        let empty = CardLayout::default();
        let (out2, _) = apply(sample(), Some(&empty));
        assert_eq!(out2.len(), 4);
    }

    #[test]
    fn 勾选顺序决定首页顺序且未勾选项被裁掉() {
        let (out, appended) = apply(sample(), Some(&trimmed()));
        assert_eq!(keys(&out), vec!["toggle:2:1", "observe_number:4:7"]);
        assert_eq!(appended, 0);
        // 模式/温度步进是用户明确排除的（在 known 里），不得被追回
        assert!(!keys(&out).contains(&"enum_choice:2:2".to_string()));
        assert!(!keys(&out).contains(&"stepper:2:4".to_string()));
    }

    #[test]
    fn 未知key忽略_没见过的新项才追加() {
        let mut l = trimmed();
        l.visible.push("enum_choice:9:9".to_string()); // 模板改版后这项已消失
        let mut items = sample();
        // 本次刷新多出一项，用户上次编辑时还没见过（不在 known 里）
        items.push(it(CardKind::SensorStatus, 2, 9, "门磁状态"));
        let (out, appended) = apply(items, Some(&l));
        assert_eq!(appended, 1, "只有 known 里没有的那项应被补进来");
        assert_eq!(
            keys(&out),
            vec!["toggle:2:1", "observe_number:4:7", "sensor_status:2:9"]
        );
    }

    #[test]
    fn 隐藏哨兵优先且不影响其他设备() {
        let mut l = trimmed();
        l.visible.push(HIDE_MARK.to_string());
        let (out, appended) = apply(sample(), Some(&l));
        assert!(out.is_empty());
        assert_eq!(appended, 0);
        assert!(!CardLayout::default().is_hidden());
    }

    #[test]
    fn 同地址不同形态的项不会互相顶掉() {
        // 模板把 on 又当观察值输出一遍：toggle:2:1 与 observe_number:2:1 必须可分别勾选
        let mut items = sample();
        items.push(it(CardKind::ObserveNumber, 2, 1, "开关状态"));
        let l = CardLayout {
            visible: vec![key_of_parts(CardKind::ObserveNumber, 2, 1)],
            known: items.iter().map(key_of).collect(),
        };
        let (out, _) = apply(items, Some(&l));
        assert_eq!(keys(&out), vec!["observe_number:2:1"]);
        assert_eq!(out[0].kind, CardKind::ObserveNumber);
    }

    #[test]
    fn 重复key只取第一次不生成两张卡() {
        let mut items = sample();
        items.push(it(CardKind::Toggle, 2, 1, "开关(模板重复项)"));
        let l = CardLayout {
            visible: vec!["toggle:2:1".to_string(), "toggle:2:1".to_string()],
            known: sample().iter().map(key_of).collect(),
        };
        let (out, _) = apply(items, Some(&l));
        // 第二个同名 key 找不到未使用的项 → 不产出；第二个重复 toggle 在 known 里 → 不追加
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn 编辑器选项反映当前勾选且带kind() {
        let opts = options(&sample(), Some(&trimmed()));
        assert_eq!(opts.len(), 4);
        let on: Vec<&str> = opts.iter().filter(|o| o.checked).map(|o| o.key.as_str()).collect();
        assert_eq!(on, vec!["toggle:2:1", "observe_number:4:7"]);
        // 勾选项按 visible 序排在前两位，未勾选项保持模板序殿后
        assert_eq!(opts[2].kind, "enum_choice");
        // 没自定义时全部预勾选
        let all = options(&sample(), None);
        assert!(all.iter().all(|o| o.checked));
    }

    #[test]
    fn 编辑器行序等于首页顺序_未勾选项殿后() {
        // visible 与模板序相反：编辑器必须按首页显示顺序出行，而不是模板序。
        // 这是档案导入路径的前提——预览列表 → 原样保存，不得把档案里的序洗掉。
        let l = CardLayout {
            visible: vec!["observe_number:4:7".into(), "toggle:2:1".into()],
            known: sample().iter().map(key_of).collect(),
        };
        let opts = options(&sample(), Some(&l));
        let order: Vec<&str> = opts.iter().map(|o| o.key.as_str()).collect();
        assert_eq!(
            order,
            vec![
                "observe_number:4:7",
                "toggle:2:1",
                "enum_choice:2:2",
                "stepper:2:4"
            ]
        );
        // 用编辑器给出的顺序原样保存，apply 的结果序不能变（往返一致）
        let round = CardLayout {
            visible: opts.iter().filter(|o| o.checked).map(|o| o.key.clone()).collect(),
            known: l.known.clone(),
        };
        let (out, _) = apply(sample(), Some(&round));
        assert_eq!(keys(&out), vec!["observe_number:4:7", "toggle:2:1"]);
    }
}
