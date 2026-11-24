/**
 * @miot/sdk 领域类型（设计文档 §2：ArkTS 只见领域模型，不见协议字段）。
 * 注意 ArkTS 命名约束：interface 全工程唯一，均带 Miot 前缀语义。
 */

export interface Device {
  did: string;
  model: string;
  name: string;
  online: boolean;
  /** spec_type URN（到 miot-spec.org 拉 spec 明细用） */
  specType?: string;
  /** spec JSON 对象（无则 undefined） */
  spec?: object;
  /** 设备 token（二期局域网直连用） */
  token?: string;
  /** BLE MAC（蓝牙设备本地广播收数用；非蓝牙设备无） */
  mac?: string;
  /**
   * 房间名（云端 `/v2/homeroom/gethome` 归一化，Rust 侧回填）。
   * undefined = 未分配房间，或云端归属拉取失败（此时首页不分组）。
   */
  room?: string;
  /** 分组序号：家庭序号*1000+房间序号，云端返回顺序；undefined = 未分组（排最后） */
  roomRank?: number;
}

export interface MiotPropValue {
  did: string;
  siid: number;
  piid: number;
  code: number;
  /** 属性值（boolean / number / string / object） */
  value: object | boolean | number | string | null;
  /** 云端缓存更新时间（秒级时间戳；实时读无此值） */
  updateTime?: number;
}

/**
 * 一键圆钮放行判定结论。判定本体在 Rust（`mijia-core/src/safety.rs`），
 * ArkTS 只消费：`allowed=false` 就一定不画圆钮，`hint` 用来解释原因。
 */
export interface MiotSwitchDecision {
  allowed: boolean;
  /** 机器可读码：allowed_* / blocked_infra / blocked_spec_mismatch / … */
  code: string;
  /** 给人看的一句话理由 */
  hint: string;
  /** spec 规范 on 属性地址；undefined = 该设备 spec 里没有可写 on（判定退回 label 兜底） */
  onSiid?: number;
  onPiid?: number;
}

/**
 * 首页「自定义卡片」编辑器的一行候选项。
 * `key` 由 Rust 生成（`kind:siid:piid`），ArkTS 不参与拼接——拼错就永远勾不上。
 */
export interface MiotLayoutOption {
  key: string;
  label: string;
  kind: string;
  checked: boolean;
}

/** 一台设备的自定义显示配置（存 preferences，两列名单语义见 mijia_core::layout） */
export interface MiotCardLayout {
  /** 要显示的项 key，按用户给的顺序 */
  visible: string[];
  /** 上次编辑时见过的全部 key（区分「明确不要」与「还没见过的新项」） */
  known: string[];
  /**
   * 可共享档案文本（由 Rust `profile::build` 生成，含 spec 语义指纹）。
   * 对 ArkTS 是**不透明**的：本层不解析它，只负责存取和回传，
   * 否则"什么算同一个属性"的判定会分裂成两处实现。
   * 没有它时退回 visible/known 名单渲染（无指纹校验）。
   */
  profile?: string;
}

/** 导入档案后的预览：勾选状态与"为什么这项被拒"都由 Rust 判定 */
export interface MiotLayoutPreview {
  options: MiotLayoutOption[];
  warnings: string[];
}

/** 密码登录的"需通知确认"分支 */
export interface NeedConfirmResult {
  needConfirm: string;
}

export type LoginResult = 'ok' | NeedConfirmResult;

export interface DeviceChangedEvent {
  did: string;
  online?: boolean;
  name?: string;
}

export type Unsubscribe = () => void;

/** 路由参数：小米登录页。logoutFirst=true 时先走官方 /pass/logout 再显示登录页 */
export class LoginRouteParam {
  logoutFirst: boolean = false;
}

/** 卡片枚举选项（Rust 归一化 DTO，ArkTS 不解析模板） */
export interface MiotCardChoice {
  /** bool 传感状态项（cardType 18）的选项值为 boolean：false=门已关/已关闭 */
  value: number | string | boolean;
  label: string;
  en: string;
}

/**
 * 归一化卡片项（方案 A：Rust CardEngine 已完成模板匹配/prop_key 解析/
 * 中文文案合并/联动禁用求值），ArkTS 零模板 JSON 解析。
 * 本期起：二级弹层（cardDetail）控制类**可交互下发**（用户显式授权）；
 * 首页（cardItems）仍按只读预览渲染。
 */
export interface MiotCardItem {
  /** toggle/enum_choice/stepper/seek_bar/observe_number/status_text/sensor_status/gear/... */
  kind: string;
  siid: number;
  piid: number;
  label: string;
  unit: string;
  format: string;
  ratio: number;
  value: object | boolean | number | string | null;
  choices: MiotCardChoice[];
  /** 联动启用态：false 时控制项灰显（如空调关机时 mode/温度） */
  enabled: boolean;
  code: number;
  nameIgnore: boolean;
  /** 值域下界（spec value-range；二级滑条/步进用，首页项无） */
  min?: number;
  /** 值域上界 */
  max?: number;
  /** 值域步长 */
  step?: number;
  /** 模板步进增量（param_delta，如目标温度 1） */
  paramDelta?: number;
  /** true = (siid,piid) 为动作地址，点击走 callAction */
  isAction: boolean;
  /** 读值来源："local" = 本地蓝牙广播直收（比云端缓存新鲜）；缺省 = 云端 */
  src?: string;
}
