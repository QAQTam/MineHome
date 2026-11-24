/**
 * 主页面：米家风格设备卡片栅格 + 登录/导入。
 * 方案 A：卡片按 Rust CardEngine 归一化 DTO 渲染，ArkTS 不做任何模板 JSON 解析。
 * 本期（导航与快控）：
 * 1. 底部 **API 26 悬浮切换条**（Tabs.barFloatingStyle + 沉浸材质）：设备 / 监控 / 我的；
 * 2. 含主开关的设备卡片给**圆形电源钮**，一次点击按服务端归一化的默认预设
 *    直接下发（bool / 0-1 / "true" 形态自适应；动作型走 callAction），
 *    乐观翻转 + 1.2/2.8/5.2s 三次确认回读，不再必须进弹层；
 * 3. **卡片位置按 did 锚定**：云端 device_list_page 返回顺序不保证稳定，
 *    这里用持久化槽位（老设备守旧位、新设备追加尾部）钉住栅格，刷新不跳；
 * 4. 登录主推「三方平台登录页直跳」（pages/PlatformLogin），密码登录降级为备用。
 * 用户已显式授权开放真机控制（默认点击即下发，不阻断二次确认）。
 */

import { uiMaterial } from '@kit.ArkUI';
import { abilityAccessCtrl, common } from '@kit.AbilityKit';
import { preferences } from '@kit.ArkData';
import { fileIo as fs, picker } from '@kit.CoreFileKit';
import { util } from '@kit.ArkTS';
import { hilog } from '@kit.PerformanceAnalysisKit';
import { miot } from '../miot/native';
import { Device, LoginResult, NeedConfirmResult,
  MiotCardItem, MiotCardChoice, MiotSwitchDecision,
  MiotLayoutOption, MiotCardLayout, MiotLayoutPreview } from '../miot/types';
import { LoginRouteParam } from '../miot/types';
import { DeviceIconTint, MetricIconTint, iconFor, metricIcon } from '../miot/SpecKit';
import { BleReading } from '../miot/BleKit';
import { BleIngest } from '../miot/BleIngest';
import { BleGatt, GattResult, GattValues } from '../miot/BleGatt';

const DOMAIN = 0x0000;
/** 二级弹层观察值轮询间隔（ms）：BLE 设备靠网关上报刷 miotspec 缓存 */
const SHEET_POLL_MS: number = 6000;
/** 下发后乐观更新压制回读旧值的确认窗口（ms）：窗口内 burst 实时读直到命中目标 */
const OPT_SETTLE_MS: number = 6000;
/** 卡片圆钮下发后的确认回读节拍（ms）：云端缓存追平前多次刷卡片 */
const QUICK_CONFIRM_MS: number[] = [1200, 2800, 5200];
/** 卡片排序锚存储（preferences，无需权限） */
const UI_PREF_STORE: string = 'ui_state';
const KEY_CARD_ORDER: string = 'card_order_v1';
/** 首页自定义显示项（轻档拼卡）存储键 */
const KEY_CARD_LAYOUT: string = 'card_layout_v1';
/**
 * 「整张卡从首页隐藏」的哨兵 key。值必须与 Rust `layout::HIDE_MARK` 一致——
 * 这是两边唯一的共享字面量，改一边就会静默失效，所以在此互相点名。
 */
const LAYOUT_HIDE_MARK: string = '__hide__';
/** 卡片高度 / 栅格间距：分组后每组 Grid 的高度要按行数算，必须与 DeviceCardView 用同一组常量 */
const CARD_H_VP: number = 148;
const CARD_GAP_VP: number = 12;
/** 未分组（云端已知但没有房间）的排序值：大于任何真实房间序号，保证排最后 */
const RANK_UNASSIGNED: number = 1000000;

/** 底部切换条索引：0 设备 / 1 监控 / 2 我的 */
const TAB_HOME: number = 0;
const TAB_MONITOR: number = 1;
const TAB_ME: number = 2;

/** 乐观更新记录：目标值 + 确认截止时间 */
class OptPatch {
  value: object | boolean | number | string | null = null;
  deadline: number = 0;
}

/** 控制预览 chip（联动禁用体现：enabled=false 时灰显） */
export class CardChip {
  key: string = '';
  text: string = '';
  enabled: boolean = true;
}

/** 可视化指标（图标 + 数值，如 🌡23.5℃ 💧45%；温度计/水滴用 sys.symbol） */
export class CardMetric {
  key: string = '';
  text: string = '';
  icon: Resource = $r('sys.symbol.drop_fill');
  color: string = '#3aa2e8';
  /** true = 本地蓝牙广播直收（比云端缓存新鲜），渲染「蓝牙」来源徽标 */
  local: boolean = false;
}

/** 弹层状态监控行（观察项 + 可视化图标，未知类型不显示图标） */
export class ObsRow {
  it: MiotCardItem;
  hasIcon: boolean = false;
  iconRes: Resource = $r('sys.symbol.drop_fill');
  iconColor: string = '#3aa2e8';

  constructor(it: MiotCardItem, mi: MetricIconTint | null) {
    this.it = it;
    if (mi !== null) {
      this.hasIcon = true;
      this.iconRes = mi.res;
      this.iconColor = mi.color;
    }
  }
}

/** 卡片视图模型（@Observed：卡片内部字段更新时子组件联动） */
@Observed
export class DeviceCard {
  did: string = '';
  name: string = '';
  model: string = '';
  online: boolean = false;
  specType: string = '';
  /** 分类符号图标（HarmonyOS sys.symbol，卡片/弹层共用） */
  icon: Resource = $r('sys.symbol.house_equipment');
  /** 图标前景色（离线时灰显由视图层处理） */
  iconFg: string = '#9899a0';
  /** 图标容器底色（由 iconFor 派生：前景色 15% alpha） */
  iconBg: string = '#f0f0f2';
  /** 归一化 DTO（Rust CardEngine 产出，ArkTS 只读渲染） */
  items: MiotCardItem[] = [];
  /** 观察值/状态文案（存在可视化指标时不再展示纯文本） */
  status: string = '';
  /** 可视化指标（图标+数值，最多 2 个；空=回落纯文本 status） */
  metrics: CardMetric[] = [];
  /** 主开关读值 */
  onState: boolean = false;
  /** 是否含 Toggle */
  hasSwitch: boolean = false;
  /**
   * 圆钮一键快控是否放行。结论**由 Rust `safety::quick_switch_ref` 判定**，
   * ArkTS 不自己推断电源语义；拿不到结论（异常）一律按 false 处理。
   */
  quickSwitch: boolean = false;
  /** 判定理由码（allowed_spec_on / blocked_infra / …），用于日志与说明 */
  switchCode: string = '';
  /** 判定理由的人话，弹层里解释「为什么没有圆钮」 */
  switchHint: string = '';
  /** 主开关项（圆钮直接下发用：siid/piid/写值形态/isAction） */
  switchItem: MiotCardItem | null = null;
  /** 主开关联动可用态（false=灰显且触摸穿透到卡片弹层） */
  switchEnabled: boolean = true;
  /** 圆钮下发中（互斥 + 转圈） */
  switchBusy: boolean = false;
  /** 圆钮乐观目标值（null=无待确认；确认窗口内压制缓存旧值回显） */
  switchPending: boolean | null = null;
  /** 乐观确认截止时间（ms epoch）；超窗未确认即接受设备真实值 */
  switchDeadline: number = 0;
  /** 控制预览（只读，含联动灰显） */
  chips: CardChip[] = [];
  /** 是否存在被联动禁用的控制项 */
  linkDisabled: boolean = false;
  /** BLE 本地直收（档位 A）用 */
  mac: string = '';
  token: string = '';
  /**
   * 房间名。'' = 云端已知该设备没进任何房间（未分组）；
   * 云端归属拉取失败时 Rust 给 null，ArkTS 保留上一次的值不重排。
   */
  room: string = '';
  /** 房间分组序号（家庭序号*1000+房间序号）；未分组为 -1，渲染时排最后 */
  roomRank: number = -1;
}

/** 首页房间分组（@Observed：组内卡片数组变化时联动） */
@Observed
export class RoomGroup {
  /** 房间名；'' = 未分组 */
  name: string = '';
  /** 组序号（取组内第一台设备的 roomRank） */
  rank: number = -1;
  cards: DeviceCard[] = [];
}

/**
 * 单张设备卡片（HarmonyOS 卡片风格）：
 * 着色圆角容器 + sys.symbol 分类图标、按压缩放回弹（scale 优于尺寸动画）、
 * 悬浮阴影区分在线态、在线/离线徽标、控制预览胶囊 chips。
 */
@Component
struct DeviceCardView {
  @ObjectLink card: DeviceCard;
  /** 按压态：驱动 scale 回弹动画 */
  @State pressed: boolean = false;
  /** 圆钮按压态（独立于卡片按压，避免点开关时整卡在缩） */
  @State pwPressed: boolean = false;
  onTap: () => void = (): void => undefined;
  /** 圆钮点击：直接按默认预设下发（不打开弹层） */
  onSwitchTap: (c: DeviceCard) => void = (c: DeviceCard): void => undefined;

  build() {
    Column() {
      Row() {
        // 分类图标容器（着色底 + sys.symbol 前景；离线整体灰显）
        Row() {
          SymbolGlyph(this.card.icon)
            .fontSize(22)
            .fontWeight(FontWeight.Medium)
            .fontColor([this.card.online ? this.card.iconFg : $r('app.color.text_disabled')])
        }
        .width(40)
        .height(40)
        .borderRadius(12)
        .backgroundColor(this.card.online ? this.card.iconBg : $r('app.color.icon_bg_offline'))
        .justifyContent(FlexAlign.Center)

        Blank()

        // 只有一键快控放行的设备才给圆钮；其余（路由器/网关等）保持在线徽标，
        // 控制仍进二级弹层按明确 label 逐项操作
        if (this.card.quickSwitch) {
          // 圆形电源钮：一次点击 = 按服务端归一化的默认预设直接下发。
          // Block 吃掉触摸不外溢（不会连带打开弹层）；联动禁用时穿透回卡片。
          Row() {
            if (this.card.switchBusy) {
              LoadingProgress()
                .width(19)
                .height(19)
                .color(this.card.onState ? Color.White : $r('app.color.brand'))
            } else {
              SymbolGlyph($r('sys.symbol.power'))
                .fontSize(18)
                .fontWeight(FontWeight.Medium)
                .fontColor([this.card.onState ? Color.White : $r('app.color.brand')])
            }
          }
          .width(40)
          .height(40)
          .borderRadius(20)
          .backgroundColor(this.card.onState ? $r('app.color.brand') : $r('app.color.power_off_bg'))
          .border({
            width: 1.2,
            color: this.card.switchEnabled ?
              (this.card.onState ? Color.Transparent : $r('app.color.power_off_ring')) :
              $r('app.color.separator')
          })
          .shadow(this.card.onState && this.card.switchEnabled ?
            { radius: 12, color: $r('app.color.power_glow'), offsetX: 0, offsetY: 3 } :
            { radius: 0, color: $r('app.color.card_shadow'), offsetX: 0, offsetY: 0 })
          .justifyContent(FlexAlign.Center)
          .opacity(this.card.switchEnabled ? 1 : 0.5)
          .scale({ x: this.pwPressed ? 0.88 : 1, y: this.pwPressed ? 0.88 : 1 })
          .animation({ duration: 150, curve: Curve.FastOutSlowIn })
          .hitTestBehavior(this.card.switchEnabled ? HitTestMode.Block : HitTestMode.None)
          .onClick((): void => {
            this.onSwitchTap(this.card);
          })
          .onTouch((event: TouchEvent): void => {
            if (event.type === TouchType.Down) {
              this.pwPressed = true;
            } else if (event.type === TouchType.Up || event.type === TouchType.Cancel) {
              this.pwPressed = false;
            }
          })
        } else {
          Row({ space: 4 }) {
            Row()
              .width(6)
              .height(6)
              .borderRadius(3)
              .backgroundColor(this.card.online ? $r('app.color.online_green') : $r('app.color.dot_offline'))
            Text(this.card.online ? '在线' : '离线')
              .fontSize(11)
              .fontColor(this.card.online ? $r('app.color.online_green') : $r('app.color.text_faded'))
          }
        }
      }
      .width('100%')

      Blank()

      Text(this.card.name)
        .fontSize(16)
        .fontWeight(FontWeight.Medium)
        .fontColor(this.card.online ? $r('app.color.text_primary') : $r('app.color.text_faded'))
        .maxLines(1)
        .textOverflow({ overflow: TextOverflow.Ellipsis })

      // 可视化指标行（🌡23.5℃ 💧45%）优先；无已知图标类型时回落纯文本状态
      if (this.card.metrics.length > 0) {
        Row({ space: 14 }) {
          ForEach(this.card.metrics, (m: CardMetric) => {
            Row({ space: 5 }) {
              SymbolGlyph(m.icon)
                .fontSize(16)
                .fontColor([m.color])
              Text(m.text)
                .fontSize(14)
                .fontWeight(FontWeight.Medium)
                .fontColor($r('app.color.text_primary'))
                .maxLines(1)
                .textOverflow({ overflow: TextOverflow.Ellipsis })
              if (m.local) {
                Text('蓝牙')
                  .fontSize(9)
                  .fontColor($r('app.color.brand'))
                  .backgroundColor($r('app.color.brand_tint'))
                  .padding({ left: 5, right: 5, top: 2, bottom: 2 })
                  .borderRadius(999)
              }
            }
            .alignItems(VerticalAlign.Center)
          }, (m: CardMetric): string => `${m.key}_${m.text}`)
        }
        .margin({ top: 3 })
      } else {
        Text(this.card.status === '' ? (this.card.online ? '···' : '离线') : this.card.status)
          .fontSize(12)
          .fontColor($r('app.color.text_secondary'))
          .maxLines(1)
          .textOverflow({ overflow: TextOverflow.Ellipsis })
          .margin({ top: 2 })
      }

      // 控制预览 chips：只读；联动禁用的项灰显（如空调关机时 mode/温度）
      if (this.card.chips.length > 0) {
        Row({ space: 6 }) {
          ForEach(this.card.chips, (chip: CardChip) => {
            Text(chip.text)
              .fontSize(10)
              .fontColor(chip.enabled ? $r('app.color.brand') : $r('app.color.text_disabled'))
              .backgroundColor(chip.enabled ? $r('app.color.brand_tint') : $r('app.color.neutral_tint'))
              .padding({ left: 8, right: 8, top: 3, bottom: 3 })
              .borderRadius(999)
              .maxLines(1)
              .textOverflow({ overflow: TextOverflow.Ellipsis })
          }, (chip: CardChip): string => `${chip.key}_${chip.text}`)
        }
        .width('100%')
        .margin({ top: 6 })
      }
    }
    .alignItems(HorizontalAlign.Start)
    .justifyContent(FlexAlign.SpaceBetween)
    .width('100%')
    .height(CARD_H_VP)
    .padding(14)
    .borderRadius(20)
    .backgroundColor(this.card.online ? $r('app.color.card_bg') : $r('app.color.card_bg_offline'))
    .shadow({
      radius: this.card.online ? 14 : 0,
      color: $r('app.color.card_shadow'),
      offsetX: 0,
      offsetY: 3
    })
    .scale({ x: this.pressed ? 0.97 : 1, y: this.pressed ? 0.97 : 1 })
    .animation({ duration: 150, curve: Curve.FastOutSlowIn })
    .onClick((): void => {
      this.onTap();
    })
    .onTouch((event: TouchEvent): void => {
      if (event.type === TouchType.Down) {
        this.pressed = true;
      } else if (event.type === TouchType.Up || event.type === TouchType.Cancel) {
        this.pressed = false;
      }
    })
  }
}

@Entry
@Component
struct Index {
  @State loggedIn: boolean = false;
  @State statusText: string = '未登录';
  @State cards: DeviceCard[] = [];
  /** 首页房间分组（由 cards 派生，卡片实例按 did 复用，不新建对象） */
  @State groups: RoomGroup[] = [];
  /** 正在编辑首页显示项的那台设备（'' = 没在编辑） */
  @State layoutEditDid: string = '';
  /** 编辑器里的候选项（顺序 = 用户拖动后的显示顺序） */
  @State layoutOpts: MiotLayoutOption[] = [];
  /** 编辑器：保存/载入中（防重复点） */
  @State layoutBusy: boolean = false;
  /** 编辑器提示文案（含新项数量等） */
  @State layoutMsg: string = '';
  /** 档案指纹逐项校验的拒绝原因（文案由 Rust 给出；ArkTS 不拼校验语义） */
  @State layoutWarn: string[] = [];
  @State busy: boolean = false;
  /** 栅格列数：按可用宽度自适应（手机 2 列，平板横屏 3~4 列） */
  @State gridCols: number = 2;
  @State loginUserName: string = '';
  @State loginUserPass: string = '';
  // ---------- 底部切换条（API 26 Tabs.barFloatingStyle 悬浮条） ----------
  @State currentTab: number = TAB_HOME;
  private tabsController: TabsController = new TabsController();
  /** 最近一次设备列表刷新时刻（设备页 / 监控页共用） */
  @State listUpdatedAt: string = '';
  /** 备用「账号密码登录」折叠态（主推小米登录页直跳） */
  @State showPasswordLogin: boolean = false;
  /** did → 槽位锚（刷新时钉住栅格位置，不随云端返回顺序跳动） */
  private orderById: Map<string, number> = new Map();
  /** did → 自定义显示配置；不在表里 = 没自定义过（按模板原样渲染） */
  private layoutById: Map<string, MiotCardLayout> = new Map();
  /** 圆钮下发后的确认回读定时器 */
  private quickTimers: number[] = [];
  /** 一键快控放行台数（诊断 + 「我的」页文案） */
  @State quickAllowedCount: number = 0;
  // ---------- 二级卡片弹层（bindSheet 半模态） ----------
  @State showSheet: boolean = false;
  /** 当前弹层设备（值拷贝，避免 @ObjectLink 生命周期问题） */
  @State sheetDid: string = '';
  @State sheetName: string = '';
  @State sheetModel: string = '';
  /** 弹层头/离线态共用分类图标（与首页卡片同源） */
  @State sheetIcon: Resource = $r('sys.symbol.house_equipment');
  @State sheetIconFg: string = '#9899a0';
  @State sheetIconBg: string = '#f0f0f2';
  @State sheetOnline: boolean = false;
  @State sheetSpecType: string = '';
  /** BLE 本地直收：当前弹层设备的 mac/token（非蓝牙设备为空） */
  @State sheetMac: string = '';
  @State sheetToken: string = '';
  /** cardDetail 全量 DTO（含 range/paramDelta/isAction） */
  @State ctlItems: MiotCardItem[] = [];
  @State obsItems: ObsRow[] = [];
  @State detailLoading: boolean = false;
  @State detailError: string = '';
  @State updatedAt: string = '';
  /** 下发互斥：一次只允许一个控制事务（set + 回读） */
  @State controlBusy: boolean = false;
  /** 滑条拖动中的临时值（idx → value） */
  @State dragIdx: number = -1;
  @State dragVal: number = 0;
  private monitorTimer: number = -1;
  /** 乐观更新表：`siid_piid` → 目标值（确认窗口内压制缓存旧值回显） */
  private pendingOptimistic: Map<string, OptPatch> = new Map();
  private burstTimers: number[] = [];
  /** 档位 A：本地 BLE 广播读数文案（空=未启用/未收到） */
  @State bleText: string = '';
  /** 上次弹层 DTO 重载时间（本地读数节流刷新用） */
  private lastDetailReload: number = 0;
  /** 首页卡片本地读数节流刷新记录（did → 上次刷新 ms） */
  private bleCardRefresh: Map<string, number> = new Map();

  aboutToAppear(): void {
    this.loggedIn = miot.isLoggedIn();
    if (this.loggedIn) {
      this.statusText = '已登录（本地会话）';
      // 冷启动直接出卡片（旧版需手点一次「刷新」）；先恢复排序锚再拉列表
      this.restoreOrder().then((): void => {
        // 自定义显示项必须在第一次 loadDevices 之前就位，
        // 否则首屏会先按模板全量渲染、再闪一次成用户的样子
        this.loadLayouts().then((): void => {
          this.loadDevices();
        });
      });
    }
    // 悬浮条材质依赖设备算力档位；不支持时系统自动降级为普通条
    hilog.info(DOMAIN, 'miot', 'immersive material supported=%{public}s level=%{public}d',
      String(uiMaterial.isImmersiveMaterialSupported()), uiMaterial.getGlobalMaterialLevel());
    // 档位 A：BLE 广播收数所需权限（user_grant，拒绝也不阻断主流程）
    try {
      const ctx = this.getUIContext().getHostContext() as common.UIAbilityContext;
      // 本地优先基础：全局 BLE 收数服务初始化（beaconkey 持久化在应用沙盒）
      BleIngest.init(ctx);
      abilityAccessCtrl.createAtManager()
        .requestPermissionsFromUser(ctx, ['ohos.permission.ACCESS_BLUETOOTH'])
        .then((r): void => {
          const codes: number[] = r.authResults ?? [];
          hilog.info(DOMAIN, 'miot', 'ACCESS_BLUETOOTH grant codes=%{public}s',
            JSON.stringify(codes));
        })
        .catch((e: Object): void => {
          hilog.warn(DOMAIN, 'miot', 'request ACCESS_BLUETOOTH rejected: %{public}s', String(e));
        });
    } catch (e) {
      hilog.warn(DOMAIN, 'miot', 'request ACCESS_BLUETOOTH: %{public}s', String(e));
    }
  }

  /** 页面销毁：停掉二级弹层轮询，防定时器泄漏 */
  aboutToDisappear(): void {
    this.stopMonitor();
    this.clearQuickTimers();
  }

  /** 从 ConfirmPage 确认完成返回后刷新登录态与设备列表 */
  onPageShow(): void {
    const nowLoggedIn: boolean = miot.isLoggedIn();
    if (nowLoggedIn === this.loggedIn && !nowLoggedIn) {
      return;
    }
    this.loggedIn = nowLoggedIn;
    if (nowLoggedIn) {
      this.statusText = '已登录（本地会话）';
      this.loadDevices();
    }
  }

  private toCard(d: Device): DeviceCard {
    const c = new DeviceCard();
    c.did = d.did;
    c.name = d.name;
    c.model = d.model;
    c.online = d.online;
    c.specType = d.specType ?? '';
    const icon: DeviceIconTint = iconFor(c.specType, d.model);
    c.icon = icon.res;
    c.iconFg = icon.fg;
    c.iconBg = icon.bg;
    c.mac = d.mac ?? '';
    c.token = d.token ?? '';
    c.room = d.room ?? '';
    c.roomRank = d.roomRank ?? -1;
    return c;
  }

  /**
   * 增量合并设备列表：同 did 复用原 @Observed 实例（只刷可变字段），
   * 新设备追加、云端已消失的设备剔除，最后按锚重排。
   * 这样卡片对象身份与 ForEach key 全程一致：位置不跳、显示不脱节。
   */
  private mergeCards(devices: Device[]): void {
    const previous: Map<string, DeviceCard> = new Map();
    for (const c of this.cards) {
      previous.set(c.did, c);
    }
    const next: Array<DeviceCard> = [];
    for (const d of devices) {
      const old: DeviceCard | undefined = previous.get(d.did);
      if (old === undefined) {
        next.push(this.toCard(d));
        continue;
      }
      old.name = d.name;
      old.model = d.model;
      old.online = d.online;
      old.specType = d.specType ?? '';
      old.mac = d.mac ?? '';
      old.token = d.token ?? '';
      // 房间归属：云端拉失败时 Rust 给 null，这里保留上一次的值 → 不会因为一次
      // 失败把已有分组打散（分组打散等于卡片整体换位，正是用户要求「不许跳」的场景）
      if (d.room !== undefined) {
        old.room = d.room;
        old.roomRank = d.roomRank ?? -1;
      }
      // on 属性地址判定已下沉 Rust（safety.rs），这里不再解析内嵌 spec
      const icon: DeviceIconTint = iconFor(old.specType, d.model);
      old.icon = icon.res;
      old.iconFg = icon.fg;
      old.iconBg = icon.bg;
      if (!d.online) {
        // 离线：清开关项与乐观压制，卡片回落「离线」态
        old.switchPending = null;
        old.switchItem = null;
        old.hasSwitch = false;
        old.switchEnabled = false;
        old.quickSwitch = false;
        old.metrics = [];
        old.chips = [];
        old.status = '离线';
      }
      next.push(old);
    }
    this.stabilizeOrder(next);
    this.cards = next;
    this.rebuildGroups();
  }

  /** 控制类组件（首页用于联动灰显；二级弹层内可交互下发） */
  private isControlKind(kind: string): boolean {
    return kind === 'toggle' || kind === 'action' || kind === 'enum_choice' ||
      kind === 'stepper' || kind === 'seek_bar' || kind === 'gear';
  }

  /** 观察/状态类组件（进入卡片状态文案） */
  private isObserveKind(kind: string): boolean {
    return kind === 'observe_number' || kind === 'status_text' ||
      kind === 'sensor_status' || kind === 'firmware_text';
  }

  /**
   * 卡片刷新（方案 A，只读）：调 Rust `card_items` 拿归一化 DTO，
   * 据此派生开关状态、观察值文案、控制预览 chips（联动禁用灰显）。
   * 不拉 spec、不解析模板、不下发任何 setProperty。
   */
  private async refreshCard(c: DeviceCard): Promise<void> {
    // 云端离线但蓝牙活着（c.mac 非空）的设备仍然刷新：云端读失败时
    // Rust 侧本地快照兜底（本地优先）。specType 空 = 无卡片可渲染，照旧跳过。
    if (c.specType === '' || (!c.online && c.mac === '')) {
      c.status = c.online ? '' : '离线';
      c.items = [];
      c.chips = [];
      c.metrics = [];
      c.hasSwitch = false;
      c.linkDisabled = false;
      c.switchItem = null;
      c.switchPending = null;
      c.switchEnabled = false;
      c.quickSwitch = false;
      this.commit();
      return;
    }
    try {
      const items: MiotCardItem[] = await miot.cardItems(c.did, c.specType,
        this.layoutById.get(c.did), c.mac === '' ? undefined : c.mac);
      c.items = items;

      // 主开关（第一个 toggle 的当前读值）：值形态可能是 bool / 0-1 数值 / "true"，
      // 统一按 isOnValue 判定（旧写法只认 boolean，0-1 型开关的设备拿不到圆钮）
      let hasSwitch = false;
      let onState = false;
      let switchItem: MiotCardItem | null = null;
      let switchEnabled: boolean = true;
      let swSiid: number = -1;
      let swPiid: number = -1;
      for (const it of items) {
        if (it.kind === 'toggle') {
          hasSwitch = true;
          onState = this.isOnValue(it.value);
          switchItem = it;
          switchEnabled = it.enabled;
          swSiid = it.siid;
          swPiid = it.piid;
          break;
        }
      }
      // 圆钮乐观窗口：回读追上目标即撤销压制；超窗未追上则接受设备真值（不替设备撒谎）
      if (switchItem !== null && c.switchPending !== null) {
        if (onState === c.switchPending) {
          c.switchPending = null;
        } else if (Date.now() < c.switchDeadline) {
          onState = c.switchPending;
        } else {
          c.switchPending = null;
        }
      }
      // 先定快控放行，再派生文案（非电源语义的 toggle 不能写成「已开启」）。
      // 判定在 Rust：把模板主 toggle 的地址与 label 交进去，换回结论 + 理由。
      c.switchItem = switchItem;
      c.switchEnabled = switchEnabled;
      if (switchItem === null) {
        c.quickSwitch = false;
        c.switchCode = '';
        c.switchHint = '';
      } else {
        try {
          const d: MiotSwitchDecision = await miot.quickSwitch(c.model, c.specType,
            switchItem.siid, switchItem.piid, switchItem.label);
          c.quickSwitch = d.allowed;
          c.switchCode = d.code;
          c.switchHint = d.hint;
          if (!d.allowed) {
            hilog.info(DOMAIN, 'miot', 'quick switch denied did=%{public}s code=%{public}s',
              c.did, d.code);
          }
        } catch (e) {
          // 拿不到结论就不给圆钮（宁缺勿滥），不沿用上一次的正结论
          c.quickSwitch = false;
          c.switchCode = 'decision_failed';
          c.switchHint = '暂时无法确认该开关的电源语义，本次不提供一键';
          hilog.warn(DOMAIN, 'miot', 'quick switch decision failed did=%{public}s: %{public}s',
            c.did, String(e));
        }
      }

      // 可视化指标：带已知图标的观察值（温度计/水滴等，最多 2 个）
      const metrics: CardMetric[] = [];
      for (const it of items) {
        if (metrics.length >= 2) {
          break;
        }
        if (!this.isObserveKind(it.kind)) {
          continue;
        }
        const text = this.formatItemValue(it);
        if (text === '') {
          continue;
        }
        const mi = metricIcon(it.label, it.unit);
        if (mi === null) {
          continue;
        }
        const m = new CardMetric();
        m.key = `${it.siid}_${it.piid}`;
        m.text = text;
        m.icon = mi.res;
        m.color = mi.color;
        m.local = it.src === 'local';
        metrics.push(m);
      }

      // 状态文案：主开关状态 +（无可视化指标时的）观察/状态值（最多 3 段）
      const parts: Array<string> = [];
      if (hasSwitch) {
        if (c.quickSwitch) {
          parts.push(onState ? '已开启' : '已关闭');
        } else if (switchItem !== null && switchItem.label !== '') {
          // 路由器/网关那类非电源 toggle：带上 label 说清楚是什么，
          // 免得把「网络配置项关掉」读成「设备关了」
          parts.push(`${switchItem.label} ${onState ? '开' : '关'}`);
        }
      }
      if (metrics.length === 0) {
        for (const it of items) {
          if (parts.length >= 3) {
            break;
          }
          // 与主开关同地址的观察项（模板把 on 又当状态输出一遍）不再重复拼「开」
          if (hasSwitch && it.siid === swSiid && it.piid === swPiid) {
            continue;
          }
          if (this.isObserveKind(it.kind)) {
            const text = this.formatItemValue(it);
            if (text !== '' && !miot.textIsSpecToken(text)) {
              parts.push(it.nameIgnore || it.label === '' ? text : `${it.label} ${text}`);
            }
          }
        }
      }

      // 控制预览 chips（跳过主 Toggle，最多 3 个），enabled=false → 灰显
      const chips: CardChip[] = [];
      let linkDisabled = false;
      for (const it of items) {
        if (chips.length >= 3) {
          break;
        }
        if (!this.isControlKind(it.kind) || it.kind === 'toggle') {
          continue;
        }
        const chip = new CardChip();
        // 同一模板可能出现重复 (kind,siid,piid)（如风扇 pi:2:2 与 p:fan:fan-level
        // 都是 gear_2_2），加序号保证 ForEach key 唯一，避免 AceStateMgmt 重复 id 错误。
        chip.key = `${chips.length}_${it.kind}_${it.siid}_${it.piid}`;
        const val = this.formatItemValue(it);
        chip.text = val === '' ? it.label : `${it.label} ${val}`;
        chip.enabled = it.enabled;
        chips.push(chip);
        if (!it.enabled) {
          linkDisabled = true;
        }
      }
      for (const it of items) {
        if (it.kind === 'toggle' && !it.enabled) {
          linkDisabled = true;
        }
      }

      c.hasSwitch = hasSwitch;
      c.onState = onState;
      c.status = parts.length > 0 ? parts.join(' · ') : (metrics.length > 0 ? '' : '在线');
      this.quickAllowedCount = this.countQuickAllowed();
      c.metrics = metrics;
      c.chips = chips;
      c.linkDisabled = linkDisabled;
      this.commit();
    } catch (e) {
      const msg = String(e);
      hilog.error(DOMAIN, 'miot', 'refreshCard %{public}s failed: %{public}s', c.did, msg);
      c.status = this.degradeStatus(msg);
      this.commit();
    }
  }

  /** 设备/通道不支持云控读取时的卡片文案降级 */
  private degradeStatus(msg: string): string {
    if (msg.indexOf('bluetooth') >= 0) {
      return '蓝牙设备（暂不支持云控）';
    }
    if (msg.indexOf('3040') >= 0 || msg.indexOf('ack timeout') >= 0) {
      return '设备无响应';
    }
    return '状态读取失败';
  }

  /** 用 DTO 的当前值 + choices/format/unit/ratio 生成显示文本 */
  private formatItemValue(it: MiotCardItem): string {
    const v = it.value;
    if (v === null || v === undefined || it.code !== 0) {
      return '';
    }
    // 枚举/状态：数值 → choices 的中文 label
    if (typeof v === 'number' && it.choices.length > 0) {
      for (const c of it.choices) {
        if (c.value === v) {
          return c.label;
        }
      }
    }
    if (typeof v === 'boolean') {
      // 人话状态词（如门磁「门已关/门已开」）：choices 命中优先
      if (it.choices.length > 0) {
        for (const c of it.choices) {
          if (c.value === v) {
            return c.label;
          }
        }
      }
      return v ? '开' : '关';
    }
    if (typeof v === 'number') {
      const n = v * it.ratio;
      return this.applyFormat(n, it.format) + it.unit;
    }
    return String(v);
  }

  private applyFormat(n: number, fmt: string): string {
    if (fmt === '%.1f') {
      return n.toFixed(1);
    }
    if (fmt === '%.0f') {
      return n.toFixed(0);
    }
    if (fmt.length >= 4 && fmt.charAt(0) === '%' && fmt.charAt(1) === '.') {
      const d = Number(fmt.charAt(2));
      if (!Number.isNaN(d) && d >= 0 && d <= 6) {
        return n.toFixed(d);
      }
    }
    return Math.floor(n) === n ? n.toFixed(0) : n.toFixed(1);
  }

  // 协议 token 文案过滤已于本轮移到 Rust（`safety::looks_like_spec_token`，
  // 经 `miot.textIsSpecToken` 调用）。原先这里有一份正则副本，两份实现迟早漂移。

  private commit(): void {
    this.cards = this.cards.slice();
  }

  /** 槽位权重：已锚定 did 用旧槽位，新设备落到尾部（此后固定不动） */
  private rankOf(did: string): number {
    const v: number | undefined = this.orderById.get(did);
    return v === undefined ? Number.MAX_SAFE_INTEGER : v;
  }

  /**
   * 锚定排序：`/v2/home/device_list_page` 的返回顺序云端不保证稳定，
   * 直接照渲就会出现「每次刷新卡片位置都变」。这里以 did 为锚——
   * 老设备守旧槽位，新设备按名称/ID 追加尾部，同权重可复现。
   */
  private stabilizeOrder(next: DeviceCard[]): void {
    next.sort((a: DeviceCard, b: DeviceCard): number => {
      const ra: number = this.rankOf(a.did);
      const rb: number = this.rankOf(b.did);
      if (ra !== rb) {
        return ra - rb;
      }
      if (a.name !== b.name) {
        return a.name < b.name ? -1 : 1;
      }
      if (a.did === b.did) {
        return 0;
      }
      return a.did < b.did ? -1 : 1;
    });
    this.orderById.clear();
    for (let i = 0; i < next.length; i++) {
      this.orderById.set(next[i].did, i);
    }
  }

  private uiStore(): Promise<preferences.Preferences> {
    const ctx = this.getUIContext().getHostContext() as common.UIAbilityContext;
    return preferences.getPreferences(ctx, UI_PREF_STORE);
  }

  /** 恢复持久化的 did 顺序锚（首启无记录时以云端首帧顺序为准） */
  private async restoreOrder(): Promise<void> {
    try {
      const store: preferences.Preferences = await this.uiStore();
      const raw: string = await store.get(KEY_CARD_ORDER, '') as string;
      if (raw === '') {
        return;
      }
      const list: string[] = JSON.parse(raw) as string[];
      this.orderById.clear();
      for (let i = 0; i < list.length; i++) {
        this.orderById.set(list[i], i);
      }
      hilog.info(DOMAIN, 'miot', 'card order restored slots=%{public}d', list.length);
    } catch (e) {
      hilog.warn(DOMAIN, 'miot', 'restore card order failed: %{public}s', String(e));
    }
  }

  /**
   * 落盘当前锚（栅格顺序从此与云端返回顺序解耦）。
   * 空列表一律不写：换到空账号、或云端偶发返回空，都不该把原账号的锚抹掉，
   * 否则切回原账号时布局会白重排一次。
   */
  private async persistOrder(): Promise<void> {
    const list: string[] = this.cards.map((c: DeviceCard): string => c.did);
    if (list.length === 0) {
      hilog.info(DOMAIN, 'miot', 'card order not persisted (empty device list)');
      return;
    }
    try {
      const store: preferences.Preferences = await this.uiStore();
      await store.put(KEY_CARD_ORDER, JSON.stringify(list));
      await store.flush();
    } catch (e) {
      hilog.warn(DOMAIN, 'miot', 'persist card order failed: %{public}s', String(e));
    }
  }

  /** 「我的」页重置排序：清锚 + 清存储，下一次刷新以云端当前顺序重新钉桩 */
  private async resetCardOrder(): Promise<void> {
    this.orderById.clear();
    try {
      const store: preferences.Preferences = await this.uiStore();
      await store.delete(KEY_CARD_ORDER);
      await store.flush();
    } catch (e) {
      hilog.warn(DOMAIN, 'miot', 'reset card order failed: %{public}s', String(e));
    }
    await this.loadDevices();
    this.getUIContext().getPromptAction().showToast({ message: '已按当前顺序重新锚定' });
  }

  /** 切换底部 Tab（空态「去登录」等处用） */
  private goToTab(idx: number): void {
    this.currentTab = idx;
    this.tabsController.changeIndex(idx);
  }

  private countQuickAllowed(): number {
    let n: number = 0;
    for (const c of this.cards) {
      if (c.quickSwitch) {
        n++;
      }
    }
    return n;
  }

  // 一键快控放行判定（三重口径：基础设施黑名单 → spec 规范 on 同址 → label 兜底）
  // 已于 2026-11-17 下沉到 Rust：`mijia-core/src/safety.rs::quick_switch_ref`，
  // 经 `miot.quickSwitch(...)` 调用。搬走的原因是要让这条安全边界能被纯 CPU 测试拦住
  // （cargo test 里 safety 的用例），而不是只能靠真机肉眼。口径没有放宽。

  /**
   * 调起系统文件管理器选择 session.json：
   * 用户在 Picker 中手动选中即视为授权，应用获得该 URI 的临时读权限，
   * 无需在 module.json5 声明任何文件类权限。
   */
  private async doImport(): Promise<void> {
    this.busy = true;
    try {
      const ctx = this.getUIContext().getHostContext() as common.UIAbilityContext;
      const opts: picker.DocumentSelectOptions = {
        maxSelectNumber: 1,
        fileSuffixFilters: ['session.json|.json']
      };
      const docPicker = new picker.DocumentViewPicker(ctx);
      const uris: Array<string> = await docPicker.select(opts);
      if (uris.length === 0) {
        this.statusText = '已取消选择';
        return;
      }
      this.statusText = '读取会话文件…';
      const text: string = this.readUriText(uris[0]);
      const ok: boolean = miot.importSession(text, ctx.filesDir);
      this.loggedIn = miot.isLoggedIn();
      this.statusText = ok && this.loggedIn ? '导入成功' : '导入失败（会话非法）';
      if (this.loggedIn) {
        await this.loadDevices();
      }
    } catch (e) {
      this.statusText = `导入异常：${String(e)}`;
    } finally {
      this.busy = false;
    }
  }

  /** picker URI → 文本（只读一次性拷入应用沙盒，权限随读取结束失效，无需持久授权） */
  private readUriText(uri: string): string {
    const file = fs.openSync(uri, fs.OpenMode.READ_ONLY);
    try {
      const size: number = fs.statSync(file.fd).size;
      const buf: ArrayBuffer = new ArrayBuffer(size);
      fs.readSync(file.fd, buf);
      return util.TextDecoder.create('utf-8').decodeToString(new Uint8Array(buf));
    } finally {
      fs.closeSync(file);
    }
  }

  private async doLogin(): Promise<void> {
    this.busy = true;
    this.statusText = '登录中…';
    try {
      const r: LoginResult = await miot.login(this.loginUserName, this.loginUserPass);
      if (r === 'ok') {
        hilog.info(DOMAIN, 'miot', 'login: direct ok');
        this.loggedIn = true;
        this.statusText = '登录成功';
        await this.loadDevices();
      } else {
        const nc = r as NeedConfirmResult;
        hilog.info(DOMAIN, 'miot', 'login: needConfirm url[:64]=%{public}s',
          nc.needConfirm.substring(0, 64));
        this.statusText = `需通知确认：${nc.needConfirm.substring(0, 48)}…`;
        // @ohos.router 自 API 18 起废弃，统一走 UIContext 路由
        this.getUIContext().getRouter().pushUrl({ url: 'miot/ConfirmPage' });
      }
    } catch (e) {
      hilog.error(DOMAIN, 'miot', 'login failed: %{public}s', String(e));
      this.statusText = `登录失败：${String(e)}`;
    } finally {
      this.busy = false;
    }
  }

  private async loadDevices(): Promise<void> {
    this.busy = true;
    try {
      const devices: Device[] = await miot.getDevices();
      // did 锚定槽位 + 实例复用，栅格位置从此与云端返回顺序解耦
      this.mergeCards(devices);
      // 本地优先：BLE 目标登记 + beaconkey 预取（持久化）+ 常驻扫描
      BleIngest.syncDevices(devices);
      this.persistOrder();
      this.listUpdatedAt = this.stampNow();
      this.statusText = devices.length === 0 ?
        '这个账号下还没有设备 · 需先在所属平台添加' :
        `设备 ${devices.length} 台 · 圆钮直接开关 · 点卡片看全部`;
      // 逐台只读刷卡片（offline 直接标记；online 调 Rust card_items 拿 DTO）
      for (const c of this.cards) {
        this.refreshCard(c);
      }
    } catch (e) {
      this.statusText = `设备获取失败：${String(e)}`;
    } finally {
      this.busy = false;
    }
  }

  /** 点击卡片：不再跳详情页，bindSheet 流畅弹出二级卡片 */
  private openDevice(c: DeviceCard): void {
    this.sheetDid = c.did;
    this.sheetName = c.name;
    this.sheetModel = c.model;
    this.sheetIcon = c.icon;
    this.sheetIconFg = c.iconFg;
    this.sheetIconBg = c.iconBg;
    this.sheetOnline = c.online;
    this.sheetSpecType = c.specType;
    this.sheetMac = c.mac;
    this.sheetToken = c.token;
    this.ctlItems = [];
    this.obsItems = [];
    this.detailError = '';
    this.updatedAt = '';
    this.dragIdx = -1;
    this.pendingOptimistic.clear();
    this.clearBurst();
    this.showSheet = true;
    this.loadDetail();
    // 本地优先：全局收数服务常驻扫描（不随弹层开闭启停）。这里确保该设备
    // 已登记 + beaconkey 预取（blt.* V5 加密帧需 32-hex beaconkey），并订阅
    // 读数事件做弹层横幅与节流刷新。
    this.bleText = '';
    hilog.info(DOMAIN, 'miot', 'sheet open did=%{public}s mac=%{public}s tokenLen=%{public}d btOn=%{public}s',
      c.did, this.sheetMac, this.sheetToken.length, String(BleIngest.didForMac(this.sheetMac) !== ''));
    if (this.sheetMac !== '') {
      BleIngest.ensureDevice(c.did, this.sheetMac, this.sheetToken);
      BleIngest.subscribe(this.onBleReading);
      this.lastDetailReload = Date.now();
      // 档位 B 形态 A（2026-11-24 拍板）：弹层打开即建 GATT 会话并**保活**
      // （串行：首读 → 读电量 → 订阅 ebe0ccc1 notify，变化秒推；订阅失败退化轮询）。
      // 只对支持免登录 ebe0ccc1 的型号发起（其余型号连上也是白连，见 BleGatt.supports）。
      // 设备常态广播不可连（固件分析 §4.5）：连不上按 not_connectable 提示长按；
      // 关弹层或 5min 到点即断（耗电 + 连接独占会挡米家）。
      if (BleGatt.supports(c.model, this.sheetMac)) {
        BleGatt.open(this.sheetMac,
          (v: GattValues): void => this.onGattPush(c.did, v),
          (reason: string): void => this.onGattClosed(c.did, reason)
        ).then((r: GattResult): void => this.onGattResult(c.did, r));
      }
    }
  }

  /** 会话建立结果 → 弹层横幅；失败给可行动文案，成功但首读未回先占位（读数走 push） */
  private onGattResult(did: string, r: GattResult): void {
    if (did !== this.sheetDid || !this.showSheet) {
      return;
    }
    if (!r.ok) {
      if (r.fail === 'unsupported') {
        // 这台设备没有免登录 GATT 通道：不打扰用户，弹层继续用广播/云缓存
        this.bleText = '';
        return;
      }
      if (r.detail !== undefined && r.detail.indexOf('stale') >= 0) {
        return; // 会话已被主动关闭，旧建链的收尾不是错误
      }
      if (r.fail === 'not_connectable') {
        this.bleText = '设备当前不可连（或正被其它端占用）：长按设备 ~5s 至屏幕闪烁后再打开弹层';
      } else if (r.fail === 'bt_off') {
        this.bleText = '蓝牙未开启，GATT 直读不可用';
      } else {
        const d = r.detail === undefined ? '' : r.detail.substring(0, 40);
        this.bleText = `GATT 直读失败：${d}`;
      }
      return;
    }
    if (r.values === undefined) {
      this.bleText = 'GATT 已连接，读取中…';
      return;
    }
    this.applyGatt(did, r.values, r.live);
  }

  /** notify 推送 / 轮询读数（会话期间持续到达） */
  private onGattPush(did: string, v: GattValues): void {
    if (did !== this.sheetDid || !this.showSheet) {
      return;
    }
    this.applyGatt(did, v, true);
  }

  /** 会话被设备/系统终止或保活到期（弹层不能再静默显示旧值） */
  private onGattClosed(did: string, reason: string): void {
    if (did !== this.sheetDid || !this.showSheet) {
      return;
    }
    this.bleText = `GATT 会话结束（${reason}）`;
    hilog.info(DOMAIN, 'miot', 'gatt session ended did=%{public}s reason=%{public}s',
      did, reason);
  }

  /** 读数上屏：横幅 + 弹层 DTO 合并 + 首页卡片追平 */
  private applyGatt(did: string, v: GattValues, live: boolean): void {
    const parts: string[] = [`${v.temperature.toFixed(1)}℃`, `${Math.round(v.humidity)}%`];
    if (v.battery !== undefined) {
      parts.push(`电量 ${Math.round(v.battery)}%`);
    }
    parts.push(`${v.voltage_v.toFixed(2)}V`);
    this.bleText = `${parts.join(' · ')} · ${this.stampNow()}（${live ? 'GATT实时' : 'GATT直读'}）`;
    this.reloadDetail(false);
    // 首页卡片同步追平快照（与广播读数同一 throttle 口径，弹层关闭后立即是新值）
    const last: number = this.bleCardRefresh.get(did) ?? 0;
    if (Date.now() - last > 5000) {
      this.bleCardRefresh.set(did, Date.now());
      for (const card of this.cards) {
        if (card.did === did) {
          this.refreshCard(card);
          break;
        }
      }
    }
  }

  private async loadDetail(): Promise<void> {
    // 云端离线但蓝牙活着（sheetMac 非空）的设备也加载：云端读失败 → 本地快照兜底
    if (this.sheetSpecType === '' || (!this.sheetOnline && this.sheetMac === '')) {
      return;
    }
    this.detailLoading = true;
    await this.reloadDetail(true);
    this.detailLoading = false;
  }

  /** 重载二级 DTO（cardDetail：Rust 读值 + 联动求值）并按控制/状态分区。
   * realtime=true 走 rpcv2 穿透实时读；乐观更新窗口内压制未追上的旧回显。 */
  private async reloadDetail(realtime: boolean): Promise<void> {
    if (this.sheetSpecType === '' || (!this.sheetOnline && this.sheetMac === '')) {
      return;
    }
    const did: string = this.sheetDid;
    try {
      const items: MiotCardItem[] = await miot.cardDetail(did, this.sheetSpecType, realtime,
        this.sheetMac === '' ? undefined : this.sheetMac);
      this.lastDetailReload = Date.now();
      if (this.sheetDid !== did) {
        return; // 弹层已切换设备，丢弃过期结果
      }
      const ctl: MiotCardItem[] = [];
      const obs: ObsRow[] = [];
      for (const raw of items) {
        let it: MiotCardItem = raw;
        if (!raw.isAction) {
          const key: string = `${raw.siid}_${raw.piid}`;
          const patch: OptPatch | undefined = this.pendingOptimistic.get(key);
          if (patch !== undefined) {
            if (patch.value === raw.value) {
              // 真实回读已追上目标：确认到达，撤销乐观压制
              this.pendingOptimistic.delete(key);
            } else if (Date.now() < patch.deadline) {
              // 确认窗口内：缓存滞后不代表失败，保持目标值显示
              it = this.cloneWithValue(raw, patch.value);
            } else {
              // 超窗仍未确认：接受设备真实值（不替设备撒谎）
              this.pendingOptimistic.delete(key);
            }
          }
        }
        if (this.isControlKind(it.kind)) {
          // 设备实际不支持的“可写”属性（读取 code!=0，如网关布防调参项）隐藏
          if (it.code !== 0 && !it.isAction) {
            continue;
          }
          ctl.push(it);
        } else {
          obs.push(new ObsRow(it, metricIcon(it.label, it.unit)));
        }
      }
      this.ctlItems = ctl;
      this.obsItems = obs;
      this.updatedAt = this.stampNow();
    } catch (e) {
      const msg = String(e);
      hilog.error(DOMAIN, 'miot', 'cardDetail failed: %{public}s', msg);
      if (this.sheetDid === did && this.detailError === '') {
        this.detailError = '状态读取失败';
      }
    }
  }

  /** 浅拷贝一个卡片项并替换 value（乐观更新用，其余字段保持引擎结果） */
  private cloneWithValue(it: MiotCardItem, v: object | boolean | number | string | null): MiotCardItem {
    const cp: MiotCardItem = {
      kind: it.kind,
      siid: it.siid,
      piid: it.piid,
      label: it.label,
      unit: it.unit,
      format: it.format,
      ratio: it.ratio,
      value: v,
      choices: it.choices,
      enabled: it.enabled,
      code: it.code,
      nameIgnore: it.nameIgnore,
      isAction: it.isAction
    };
    if (it.min !== undefined) {
      cp.min = it.min;
    }
    if (it.max !== undefined) {
      cp.max = it.max;
    }
    if (it.step !== undefined) {
      cp.step = it.step;
    }
    if (it.paramDelta !== undefined) {
      cp.paramDelta = it.paramDelta;
    }
    return cp;
  }

  /** 下发 burst 确认读：延迟内多次实时回读，全部命中目标即提前收工 */
  private scheduleBurst(): void {
    this.clearBurst();
    const delays: number[] = [1200, 2800, 5000];
    for (const ms of delays) {
      const t: number = setTimeout((): void => {
        this.reloadDetail(true);
        if (this.pendingOptimistic.size === 0) {
          this.clearBurst();
        }
      }, ms);
      this.burstTimers.push(t);
    }
  }

  private clearBurst(): void {
    for (const t of this.burstTimers) {
      clearTimeout(t);
    }
    this.burstTimers = [];
  }

  /** 下发成功后同步刷新首页对应卡片（延迟到云端缓存基本追平后） */
  private refreshCardByDid(did: string): void {
    for (const c of this.cards) {
      if (c.did === did) {
        this.refreshCard(c);
        return;
      }
    }
  }

  /**
   * 圆形电源钮：一次点击直接下发，不进弹层、不要用户再选参数。
   * 写值取服务端归一化 DTO 的默认预设形态（bool→true/false、0-1→1/0、
   * 字符串→"true"/"false"）；模板给的是动作地址（isAction）时走 callAction。
   * 受理后本地乐观翻转，按 QUICK_CONFIRM_MS 三次刷卡片确认，超窗未确认回落真值。
   */
  private async quickToggle(c: DeviceCard): Promise<void> {
    const it: MiotCardItem | null = c.switchItem;
    if (it === null || !c.online || !c.quickSwitch || !c.switchEnabled || c.switchBusy || this.busy) {
      return;
    }
    const target: boolean = !c.onState;
    c.switchBusy = true;
    this.commit();
    let ok: boolean = false;
    try {
      if (it.isAction) {
        await miot.callAction(c.did, it.siid, it.piid);
      } else {
        await miot.setProperty(c.did, it.siid, it.piid, this.toggleWriteValue(it, target));
      }
      ok = true;
    } catch (e) {
      const msg: string = this.controlDegrade(String(e));
      hilog.error(DOMAIN, 'miot', 'quickToggle did=%{public}s s%{public}d/p%{public}d failed: %{public}s',
        c.did, it.siid, it.piid, String(e));
      this.getUIContext().getPromptAction().showToast({ message: msg });
    }
    c.switchBusy = false;
    if (ok && !it.isAction) {
      c.onState = target;
      c.switchPending = target;
      c.switchDeadline = Date.now() + OPT_SETTLE_MS;
      c.status = target ? '已开启' : '已关闭';
      this.scheduleQuickConfirm(c.did);
    }
    this.commit();
  }

  /** 圆钮下发后的确认回读节拍（只刷首页卡片，不动弹层 burst） */
  private scheduleQuickConfirm(did: string): void {
    for (const ms of QUICK_CONFIRM_MS) {
      const t: number = setTimeout((): void => {
        this.refreshCardByDid(did);
      }, ms);
      this.quickTimers.push(t);
    }
  }

  private clearQuickTimers(): void {
    for (const t of this.quickTimers) {
      clearTimeout(t);
    }
    this.quickTimers = [];
  }

  /**
   * 控制下发：setProperty / callAction（用户已授权真机控制）。
   * 受理后 UI 立刻乐观更新到目标值，burst 实时回读确认（1.2/2.8/5s），
   * 云端缓存滞后不再打回显示；无需退出弹层重进。
   */
  private async dispatch(it: MiotCardItem, v: boolean | number | string): Promise<void> {
    if (this.controlBusy || !it.enabled) {
      return;
    }
    this.controlBusy = true;
    this.dragIdx = -1;
    this.detailError = '';
    let ok: boolean = false;
    try {
      if (it.isAction) {
        await miot.callAction(this.sheetDid, it.siid, it.piid);
      } else {
        await miot.setProperty(this.sheetDid, it.siid, it.piid, v);
      }
      ok = true;
    } catch (e) {
      this.detailError = this.controlDegrade(String(e));
      hilog.error(DOMAIN, 'miot', 'dispatch did=%{public}s siid=%{public}d piid=%{public}d failed: %{public}s',
        this.sheetDid, it.siid, it.piid, String(e));
      this.getUIContext().getPromptAction().showToast({ message: this.detailError });
    } finally {
      this.controlBusy = false;
    }
    if (ok && !it.isAction) {
      const patch = new OptPatch();
      patch.value = v;
      patch.deadline = Date.now() + OPT_SETTLE_MS;
      this.pendingOptimistic.set(`${it.siid}_${it.piid}`, patch);
      this.ctlItems = this.ctlItems.map((x: MiotCardItem): MiotCardItem =>
        x.siid === it.siid && x.piid === it.piid ? this.cloneWithValue(x, v) : x);
      this.scheduleBurst();
      setTimeout((): void => {
        this.refreshCardByDid(this.sheetDid);
      }, 4500);
    }
    await this.reloadDetail(true);
  }

  /** 模板动作按钮（cardType 2）：动作地址走 callAction；属性地址按形态翻转/轮换 */
  private dispatchActionItem(it: MiotCardItem): void {
    if (it.isAction) {
      this.dispatch(it, 0);
      return;
    }
    if (typeof it.value === 'boolean') {
      this.dispatch(it, !(it.value as boolean));
      return;
    }
    if (it.choices.length > 0) {
      let ni: number = 0;
      for (let i = 0; i < it.choices.length; i++) {
        if (it.choices[i].value === it.value) {
          ni = (i + 1) % it.choices.length;
        }
      }
      this.dispatch(it, it.choices[ni].value);
      return;
    }
    this.reloadDetail(true);
  }

  /** 下发失败降级提示（不阻断弹层，仍可读监控） */
  private controlDegrade(msg: string): string {
    if (msg.indexOf('does not support sending rpc') >= 0 || msg.indexOf('bluetooth') >= 0) {
      return '蓝牙设备暂不支持云控下发（仅实时监测）';
    }
    if (msg.indexOf('timeout') >= 0 || msg.indexOf('3040') >= 0) {
      return '设备无响应，稍后自动回读确认';
    }
    if (msg.indexOf('-5000') >= 0) {
      return '设备拒绝了该操作（可能处于联动禁用状态）';
    }
    return '控制下发失败';
  }

  private numOf(it: MiotCardItem): number {
    return typeof it.value === 'number' ? it.value as number : 0;
  }

  /** Toggle 显示态：bool 型看 true；uint8 0/1 型看数值 1；字符串型看 "true"/"1" */
  private isOnValue(v: object | boolean | number | string | null): boolean {
    if (typeof v === 'boolean') {
      return v as boolean;
    }
    if (typeof v === 'number') {
      return (v as number) === 1;
    }
    if (typeof v === 'string') {
      return v === 'true' || v === '1';
    }
    return false;
  }

  /** Toggle 写回值：必须匹配属性形态（spec bool→boolean；数值 0/1→number；字符串→"true"/"false"） */
  private toggleWriteValue(it: MiotCardItem, on: boolean): boolean | number | string {
    const v = it.value;
    if (it.format !== 'bool' && typeof v === 'number') {
      return on ? 1 : 0;
    }
    if (typeof v === 'string') {
      return on ? 'true' : 'false';
    }
    if (it.format !== 'bool' && it.choices.length > 0) {
      // value-list 0/1 枚举开关：优先取列表里的 0/1 数值
      return on ? 1 : 0;
    }
    return on;
  }

  /** 步进：param_delta 优先，其次 spec step；min/max 钳制 */
  private clampStep(it: MiotCardItem, dir: number): void {
    const delta: number = it.paramDelta ?? it.step ?? 1;
    const min: number = it.min ?? 0;
    const max: number = it.max ?? 100;
    const cur: number = this.numOf(it);
    let next: number = cur + dir * delta;
    if (next < min) {
      next = min;
    }
    if (next > max) {
      next = max;
    }
    next = Math.round(next * 100) / 100;
    if (next === cur) {
      return;
    }
    this.dispatch(it, next);
  }

  /** 控制/观察项当前值文案（数值→choices 中文/格式；bool→开关；无值→--） */
  private ctlValueText(it: MiotCardItem): string {
    if (it.code !== 0 || it.value === null || it.value === undefined) {
      return '--';
    }
    if (typeof it.value === 'boolean') {
      if (it.choices.length > 0) {
        for (const c of it.choices) {
          if (c.value === it.value) {
            return c.label;
          }
        }
      }
      return it.value ? '开' : '关';
    }
    if (typeof it.value === 'number') {
      if (it.choices.length > 0) {
        for (const c of it.choices) {
          if (c.value === it.value) {
            return c.label === '' ? String(c.value) : c.label;
          }
        }
      }
      const n: number = it.value as number * it.ratio;
      let fmt: string = it.format;
      if (fmt.length < 4 || fmt.charAt(0) !== '%') {
        const d: number = it.paramDelta ?? it.step ?? 1;
        fmt = d % 1 === 0 ? '%.0f' : '%.1f';
      }
      return this.applyFormat(n, fmt) + it.unit;
    }
    return String(it.value);
  }

  private sliderMin(it: MiotCardItem): number {
    return it.min ?? 0;
  }

  private sliderMax(it: MiotCardItem): number {
    return it.max ?? 100;
  }

  private sliderValue(it: MiotCardItem, idx: number): number {
    if (this.dragIdx === idx) {
      return this.dragVal;
    }
    const v: number = this.numOf(it);
    return Math.min(Math.max(v, this.sliderMin(it)), this.sliderMax(it));
  }

  private startMonitor(): void {
    // …现有轮询逻辑不变（见下）
    this.stopMonitor();
    // 云端离线但蓝牙活着（sheetMac 非空）也轮询：本地快照兜底 + 广播持续推进
    if (this.sheetSpecType === '' || (!this.sheetOnline && this.sheetMac === '')) {
      return;
    }
    this.monitorTimer = setInterval((): void => {
      if (!this.controlBusy) {
        this.reloadDetail(true);
      }
    }, SHEET_POLL_MS);
  }

  private stopMonitor(): void {
    if (this.monitorTimer >= 0) {
      clearInterval(this.monitorTimer);
      this.monitorTimer = -1;
    }
  }

  /** 档位 A：本地 BLE 广播读数 → 人话文案（比云端更实时、不依赖网关） */
  private onBleReading(r: BleReading): void {
    const v = r.adv.values;
    const parts: string[] = [];
    if (v.temperature !== undefined) {
      parts.push(`${v.temperature.toFixed(1)}℃`);
    }
    if (v.humidity !== undefined) {
      parts.push(`${Math.round(v.humidity)}%`);
    }
    if (v.battery !== undefined) {
      parts.push(`电量 ${v.battery}%`);
    }
    if (v.door_closed !== undefined) {
      parts.push(v.door_closed ? '门已关' : '门已开');
    }
    if (parts.length === 0) {
      return;
    }
    const d = new Date(r.ts);
    const hh: string = d.getHours().toString().padStart(2, '0');
    const mm: string = d.getMinutes().toString().padStart(2, '0');
    const ss: string = d.getSeconds().toString().padStart(2, '0');
    this.bleText = `${parts.join(' · ')} · ${hh}:${mm}:${ss}（蓝牙直收）`;
    // 本地优先联动：读数已落 Rust 快照，节流刷新——弹层当前设备 20s 一次、
    // 首页卡片 60s 一次（读值合并都在 Rust，这里只触发 DTO 重载）
    if (r.adv.mac === '') {
      return;
    }
    const did: string = BleIngest.didForMac(r.adv.mac);
    if (did === '') {
      return;
    }
    if (this.showSheet && did === this.sheetDid) {
      if (Date.now() - this.lastDetailReload > 20000 && !this.controlBusy && !this.detailLoading) {
        this.reloadDetail(false);
      }
      return;
    }
    const last: number = this.bleCardRefresh.get(did) ?? 0;
    if (Date.now() - last > 60000) {
      this.bleCardRefresh.set(did, Date.now());
      for (const card of this.cards) {
        if (card.did === did) {
          this.refreshCard(card);
          break;
        }
      }
    }
  }

  private stampNow(): string {
    const d = new Date();
    const hh: string = d.getHours().toString().padStart(2, '0');
    const mm: string = d.getMinutes().toString().padStart(2, '0');
    const ss: string = d.getSeconds().toString().padStart(2, '0');
    return `${hh}:${mm}:${ss}`;
  }

  /**
   * ForEach key：索引 + 业务标识 + 值指纹。
   * 键未变时框架复用旧组件不重渲染，纯值更新（轮询回读）必须进 key 才能刷新 UI。
   */
  private itemKey(it: MiotCardItem, idx: number, tag: string): string {
    const v = it.value;
    let fp: string;
    if (v === null || v === undefined) {
      fp = 'null';
    } else if (typeof v === 'object') {
      fp = JSON.stringify(v);
    } else {
      fp = String(v);
    }
    return `${idx}_${it.kind}_${it.siid}_${it.piid}_${it.isAction ? 'a' : 'p'}_${tag}_${fp}`;
  }

  /** 分区小标题 */
  @Builder
  sectionLabel(text: string) {
    Text(text)
      .fontSize(13)
      .fontColor($r('app.color.text_tertiary'))
      .width('100%')
  }

  /** 错误横幅（图标 + 文案，红色浅底） */
  @Builder
  errorBanner() {
    Row({ space: 6 }) {
      SymbolGlyph($r('sys.symbol.exclamationmark_triangle'))
        .fontSize(14)
        .fontColor([$r('app.color.error_red')])
      Text(this.detailError)
        .fontSize(12)
        .fontColor($r('app.color.error_red'))
        .layoutWeight(1)
    }
    .width('100%')
    .padding(12)
    .backgroundColor($r('app.color.red_tint'))
    .borderRadius(12)
  }

  /** 蓝牙直收横幅（图标 + 实时读数，绿色浅底） */
  @Builder
  bleBanner() {
    Row({ space: 6 }) {
      SymbolGlyph($r('sys.symbol.bluetooth'))
        .fontSize(14)
        .fontColor([$r('app.color.green_text')])
      Text(this.bleText)
        .fontSize(12)
        .fontColor($r('app.color.green_text'))
        .layoutWeight(1)
    }
    .width('100%')
    .padding(12)
    .backgroundColor($r('app.color.green_tint'))
    .borderRadius(12)
  }

  /** 二级卡片弹层内容 */
  @Builder
  sheetBuilder() {
    Column() {
      if (!this.sheetOnline) {
        // 离线态：图标圆角容器 + 主/副文案
        Column({ space: 12 }) {
          Row() {
            SymbolGlyph(this.sheetIcon)
              .fontSize(32)
              .fontWeight(FontWeight.Medium)
              .fontColor([$r('app.color.text_disabled')])
          }
          .width(72)
          .height(72)
          .borderRadius(24)
          .backgroundColor($r('app.color.neutral_tint'))
          .justifyContent(FlexAlign.Center)

          Text('设备离线，暂无法控制')
            .fontSize(15)
            .fontWeight(FontWeight.Medium)
            .fontColor($r('app.color.text_medium'))
          Text('设备恢复在线后即可远程控制')
            .fontSize(12)
            .fontColor($r('app.color.text_secondary'))
        }
        .width('100%')
        .height('100%')
        .justifyContent(FlexAlign.Center)
      } else {
        Scroll() {
          Column({ space: 12 }) {
            // 编辑态下这块在最前面：否则用户点了标题栏图标，还得往下滑两屏才看到行
            this.layoutEditorSection()
            // 头部：分类图标 + 同步状态 + 加载指示
            Row({ space: 10 }) {
              Row() {
                SymbolGlyph(this.sheetIcon)
                  .fontSize(18)
                  .fontWeight(FontWeight.Medium)
                  .fontColor([this.sheetIconFg])
              }
              .width(36)
              .height(36)
              .borderRadius(12)
              .backgroundColor(this.sheetIconBg)
              .justifyContent(FlexAlign.Center)

              Column({ space: 2 }) {
                Text(this.detailLoading ? '正在读取设备状态…' :
                  (this.updatedAt === '' ? ' ' : `已同步 ${this.updatedAt}`))
                  .fontSize(13)
                  .fontColor($r('app.color.text_medium'))
                Text(`每 ${SHEET_POLL_MS / 1000}s 自动监控刷新`)
                  .fontSize(11)
                  .fontColor($r('app.color.text_secondary'))
              }
              .alignItems(HorizontalAlign.Start)
              .layoutWeight(1)

              if (this.detailLoading) {
                LoadingProgress()
                  .width(18)
                  .height(18)
                  .color($r('app.color.brand'))
              }
            }
            .width('100%')

            if (this.detailError !== '') {
              this.errorBanner()
            }

            if (this.ctlItems.length > 0) {
              this.sectionLabel('控制')
              ForEach(this.ctlItems, (it: MiotCardItem, idx: number) => {
                this.ctlRow(it, idx)
              }, (it: MiotCardItem, idx: number): string =>
                this.itemKey(it, idx, 'ctl'))
            } else if (!this.detailLoading) {
              Text('该设备暂无可下发控制项（仅状态监测）')
                .fontSize(12).fontColor($r('app.color.text_secondary')).width('100%')
            }

            if (this.bleText !== '') {
              this.bleBanner()
            }

            if (this.obsItems.length > 0) {
              this.sectionLabel('状态监控')
              // 可视化指标磁贴（2 列）：大图标 + 数值 + 标签，底色 = 指标色 12% alpha
              Flex({ wrap: FlexWrap.Wrap }) {
                ForEach(this.obsItems, (row: ObsRow, idx: number) => {
                  Column({ space: 6 }) {
                    if (row.hasIcon) {
                      SymbolGlyph(row.iconRes)
                        .fontSize(30)
                        .fontColor([row.iconColor])
                    }
                    Text(this.ctlValueText(row.it))
                      .fontSize(row.hasIcon ? 18 : 15)
                      .fontWeight(FontWeight.Medium)
                      .fontColor($r('app.color.text_primary'))
                      .maxLines(1)
                      .textOverflow({ overflow: TextOverflow.Ellipsis })
                    Text(row.it.label)
                      .fontSize(11)
                      .fontColor($r('app.color.text_secondary'))
                      .maxLines(1)
                      .textOverflow({ overflow: TextOverflow.Ellipsis })
                    // 来源徽标：本地蓝牙直收（比云端缓存新鲜）才标
                    if (row.it.src === 'local') {
                      Text('蓝牙直收')
                        .fontSize(9)
                        .fontColor($r('app.color.green_text'))
                        .backgroundColor($r('app.color.green_tint'))
                        .padding({ left: 6, right: 6, top: 2, bottom: 2 })
                        .borderRadius(999)
                    }
                  }
                  .width('48%')
                  .padding({ left: 6, right: 6, top: 14, bottom: 12 })
                  .borderRadius(14)
                  .backgroundColor(row.hasIcon ?
                    `#1f${row.iconColor.substring(1)}` : $r('app.color.neutral_tint'))
                  .margin({ right: '2%', bottom: 8 })
                  .alignItems(HorizontalAlign.Center)
                  .justifyContent(FlexAlign.Center)
                }, (row: ObsRow, idx: number): string =>
                  this.itemKey(row.it, idx, 'obs'))
              }
              .width('100%')
            }
            Row().height(20)
          }
          .width('100%')
          .padding({ left: 16, right: 16, top: 4 })
        }
        .layoutWeight(1)
        .scrollBar(BarState.Off)
        .align(Alignment.Top)
        .nestedScroll({
          scrollForward: NestedScrollMode.PARENT_FIRST,
          scrollBackward: NestedScrollMode.SELF_FIRST
        })
      }
    }
    .width('100%')
    .height('100%')
  }

  // ============ 首页自定义显示项（轻档拼卡）============
  // 名单存 preferences，过滤与排序在 Rust（`layout::apply`）；本层只传名单、只渲染。

  /** 读自定义名单。必须在第一次 loadDevices 之前完成，否则首屏会闪一次全量。 */
  private async loadLayouts(): Promise<void> {
    try {
      const store: preferences.Preferences = await this.uiStore();
      const raw: string = await store.get(KEY_CARD_LAYOUT, '') as string;
      if (raw === '') {
        return;
      }
      const obj: Record<string, Object> = JSON.parse(raw) as Record<string, Object>;
      for (const did of Object.keys(obj)) {
        const rec: Record<string, Object> = obj[did] as Record<string, Object>;
        const vis: string[] = rec['visible'] as string[];
        const kn: string[] = rec['known'] as string[];
        const pf: string = rec['profile'] as string;
        const lay: MiotCardLayout = {
          visible: vis === undefined || vis === null ? [] : vis,
          known: kn === undefined || kn === null ? [] : kn
        };
        // 档案对上层是不透明文本，本层只捎带存取；缺失 = 退回名单通道（无指纹保护）
        if (pf !== undefined && pf !== null && pf !== '') {
          lay.profile = pf;
        }
        this.layoutById.set(did, lay);
      }
      hilog.info(DOMAIN, 'miot', 'card layouts restored entries=%{public}d',
        this.layoutById.size);
    } catch (e) {
      hilog.warn(DOMAIN, 'miot', 'load card layouts failed: %{public}s', String(e));
    }
  }

  /** 整表回写（条目很少，不做增量） */
  private async saveLayouts(): Promise<void> {
    try {
      const rec: Record<string, MiotCardLayout> = {};
      this.layoutById.forEach((v: MiotCardLayout, k: string): void => {
        rec[k] = v;
      });
      const store: preferences.Preferences = await this.uiStore();
      await store.put(KEY_CARD_LAYOUT, JSON.stringify(rec));
      await store.flush();
    } catch (e) {
      hilog.warn(DOMAIN, 'miot', 'persist card layouts failed: %{public}s', String(e));
    }
  }

  /** 打开编辑器：候选项与 key 全部来自 Rust，避免两边拼法不一致 */
  private openLayoutEditor(): void {
    const did: string = this.sheetDid;
    if (did === '') {
      return;
    }
    this.layoutBusy = true;
    this.layoutMsg = '';
    this.layoutWarn = [];
    const lay: MiotCardLayout | undefined = this.layoutById.get(did);
    if (lay !== undefined && lay.profile !== undefined && lay.profile !== '') {
      // 有档案走档案通道：勾选状态由 Rust 按当前固件逐项校验指纹后给出，
      // 与首页渲染同源，避免"编辑器说一套、卡片渲染另一套"
      miot.cardLayoutPreview(did, this.sheetSpecType, lay.profile)
        .then((p: MiotLayoutPreview): void => {
          this.layoutOpts = p.options;
          this.layoutWarn = p.warnings;
          this.layoutEditDid = did;
          this.layoutBusy = false;
        })
        .catch((e: Error): void => {
          this.layoutMsg = '档案校验失败：' + e.message + '；已退回名单显示';
          this.loadEditorFromList(did);
        });
      return;
    }
    this.loadEditorFromList(did);
  }

  /** 名单通道的编辑器数据源（还没生成过档案的老条目走这条） */
  private loadEditorFromList(did: string): void {
    miot.cardLayoutOptions(did, this.sheetSpecType, this.layoutById.get(did))
      .then((list: MiotLayoutOption[]): void => {
        this.layoutOpts = list;
        this.layoutEditDid = did;
        this.layoutBusy = false;
      })
      .catch((e: Error): void => {
        this.layoutMsg = '载入候选项失败：' + e.message;
        this.layoutBusy = false;
      });
  }

  /** 换整个数组引用驱动 ForEach 刷新（改对象字段不会触发 @State） */
  private setLayoutChecked(idx: number, on: boolean): void {
    const next: MiotLayoutOption[] = this.layoutOpts.slice();
    const cur: MiotLayoutOption = next[idx];
    const copy: MiotLayoutOption = {
      key: cur.key,
      label: cur.label,
      kind: cur.kind,
      checked: on
    };
    next[idx] = copy;
    this.layoutOpts = next;
  }

  private moveLayoutItem(idx: number, delta: number): void {
    const to: number = idx + delta;
    if (to < 0 || to >= this.layoutOpts.length) {
      return;
    }
    const next: MiotLayoutOption[] = this.layoutOpts.slice();
    const tmp: MiotLayoutOption = next[idx];
    next[idx] = next[to];
    next[to] = tmp;
    this.layoutOpts = next;
  }

  private async saveLayout(): Promise<void> {
    const did: string = this.sheetDid;
    if (did === '') {
      return;
    }
    this.layoutBusy = true;
    const known: string[] = this.layoutOpts.map((o: MiotLayoutOption): string => o.key);
    const visible: string[] = this.layoutOpts
      .filter((o: MiotLayoutOption): boolean => o.checked)
      .map((o: MiotLayoutOption): string => o.key);
    const lay: MiotCardLayout = { visible: visible, known: known };
    // 顺手生成可共享档案（指纹校验全在 Rust 侧）。生成失败退回裸名单：
    // 代价只是没有"固件改版后重新校验"这一层，不阻断用户保存本次勾选。
    let profileNote: string = '';
    try {
      const text: string = await miot.exportCardProfile(did, this.sheetSpecType,
        this.sheetModel, visible, known);
      if (text !== '') {
        lay.profile = text;
      }
    } catch (e) {
      hilog.warn(DOMAIN, 'miot', 'build card profile failed did=%{public}s: %{public}s',
        did, String(e));
      profileNote = '；档案生成失败，本次仅保存名单';
    }
    this.layoutById.set(did, lay);
    await this.saveLayouts();
    this.layoutBusy = false;
    this.layoutEditDid = '';
    this.layoutWarn = [];
    // 一项都没勾时 Rust 按「没自定义」处理（宁全显不空白），这里如实说明而不是假装生效
    this.layoutMsg = (visible.length === 0 ? '未勾选任何项：已按默认全量显示' : '') + profileNote;
    this.refreshCardByDid(did);
  }

  private cancelLayoutEdit(): void {
    this.layoutEditDid = '';
    this.layoutOpts = [];
    this.layoutMsg = '';
    this.layoutWarn = [];
  }

  /**
   * 导出当前设备的档案（可共享：含 spec 语义指纹，不含 did / 设备名）。
   * 已存档案原样导出；只有名单时按名单现建；从没自定义过按"模板全量"建。
   */
  private async exportProfile(): Promise<void> {
    const did: string = this.sheetDid;
    if (did === '' || this.layoutBusy) {
      return;
    }
    this.layoutBusy = true;
    try {
      const lay: MiotCardLayout | undefined = this.layoutById.get(did);
      let text: string = lay !== undefined && lay.profile !== undefined ? lay.profile : '';
      if (text === '') {
        let visible: string[] = [];
        let known: string[] = [];
        if (lay !== undefined) {
          visible = lay.visible;
          known = lay.known;
        } else {
          const all: MiotLayoutOption[] = await miot.cardLayoutOptions(did, this.sheetSpecType);
          visible = all.map((o: MiotLayoutOption): string => o.key);
          known = visible;
        }
        text = await miot.exportCardProfile(did, this.sheetSpecType,
          this.sheetModel, visible, known);
      }
      const ctx = this.getUIContext().getHostContext() as common.UIAbilityContext;
      const model: string = this.sheetModel.replace(/[^A-Za-z0-9._-]/g, '_');
      const saveOpts: picker.DocumentSaveOptions = {
        newFileNames: [`minehome-card-${model === '' ? 'device' : model}.json`]
      };
      const docPicker = new picker.DocumentViewPicker(ctx);
      const uris: Array<string> = await docPicker.save(saveOpts);
      if (uris.length === 0) {
        this.layoutMsg = '已取消导出';
        return;
      }
      const file = fs.openSync(uris[0],
        fs.OpenMode.READ_WRITE | fs.OpenMode.CREATE | fs.OpenMode.TRUNC);
      try {
        fs.writeSync(file.fd, text);
      } finally {
        fs.closeSync(file);
      }
      this.getUIContext().getPromptAction().showToast({ message: '档案已导出' });
    } catch (e) {
      hilog.warn(DOMAIN, 'miot', 'export profile failed: %{public}s', String(e));
      this.layoutMsg = '导出失败：' + String(e);
    } finally {
      this.layoutBusy = false;
    }
  }

  /**
   * 导入档案：先让 Rust 按当前固件逐项校验（cardLayoutPreview），
   * 被拒项**逐项显示原因**——不静默丢弃、不降级放行（用户口径）。
   * 通过后把档案原文存进该设备的 layout 并刷新；渲染随即走档案通道。
   */
  private async importProfile(): Promise<void> {
    const did: string = this.sheetDid;
    if (did === '' || this.layoutBusy) {
      return;
    }
    this.layoutBusy = true;
    try {
      const ctx = this.getUIContext().getHostContext() as common.UIAbilityContext;
      const sel: picker.DocumentSelectOptions = {
        maxSelectNumber: 1,
        fileSuffixFilters: ['卡片档案|.json']
      };
      const docPicker = new picker.DocumentViewPicker(ctx);
      const uris: Array<string> = await docPicker.select(sel);
      if (uris.length === 0) {
        this.layoutMsg = '已取消导入';
        return;
      }
      const text: string = this.readUriText(uris[0]);
      const preview: MiotLayoutPreview = await miot.cardLayoutPreview(did, this.sheetSpecType, text);
      // 勾选状态与顺序全部来自 Rust，本层不解析档案 JSON（指纹语义只许一处实现）
      this.layoutOpts = preview.options;
      this.layoutWarn = preview.warnings;
      const visible: string[] = preview.options
        .filter((o: MiotLayoutOption): boolean => o.checked)
        .map((o: MiotLayoutOption): string => o.key);
      const known: string[] = preview.options.map((o: MiotLayoutOption): string => o.key);
      const lay: MiotCardLayout = { visible: visible, known: known };
      lay.profile = text;
      this.layoutById.set(did, lay);
      await this.saveLayouts();
      this.layoutMsg = preview.warnings.length === 0
        ? '档案已导入：全部项通过校验'
        : `档案已导入：${preview.warnings.length} 项被拒绝，原因见下`;
      this.refreshCardByDid(did);
    } catch (e) {
      hilog.warn(DOMAIN, 'miot', 'import profile failed: %{public}s', String(e));
      this.layoutMsg = '导入失败：' + String(e);
    } finally {
      this.layoutBusy = false;
    }
  }

  /**
   * 弹层底部的编辑区。**只做勾选 + 排序**：控件形态一律由模板/spec 决定，
   * 这里刻意不给"把它改成滑条/开关"的入口——选错形态会破坏下发值形态。
   */
  @Builder
  layoutEditorSection() {
    Column({ space: 8 }) {
      // 未编辑：一条入口横幅。这块整体渲染在弹层内容**最前面**，
      // 所以点完立刻看到行，不用再往下滑两屏、也不会压在手势条上。
      if (this.layoutEditDid !== this.sheetDid) {
        Row({ space: 8 }) {
          SymbolGlyph($r('sys.symbol.square_and_pencil'))
            .fontSize(17)
            .fontColor([$r('app.color.brand')])
          Text('编辑首页显示项')
            .fontSize(13)
            .fontColor($r('app.color.brand'))
            .layoutWeight(1)
          Text(this.layoutBusy ? '载入中…' : '选择要显示的项并排序')
            .fontSize(11)
            .fontColor($r('app.color.text_secondary'))
        }
        .width('100%')
        .padding({ left: 12, right: 12, top: 10, bottom: 10 })
        .borderRadius(12)
        .backgroundColor($r('app.color.brand_tint'))
        .onClick((): void => {
          this.openLayoutEditor();
        })
      }
      if (this.layoutEditDid === this.sheetDid) {
        Text('首页显示这些项，从上到下')
          .fontSize(12)
          .fontColor($r('app.color.text_secondary'))
        ForEach(this.layoutOpts, (o: MiotLayoutOption, idx: number) => {
          Row({ space: 8 }) {
            Toggle({ type: ToggleType.Checkbox, isOn: o.checked })
              .onChange((on: boolean): void => {
                this.setLayoutChecked(idx, on);
              })
            Text(o.label === '' ? o.kind : o.label)
              .fontSize(14)
              .fontColor($r('app.color.text_primary'))
              .layoutWeight(1)
              .maxLines(1)
              .textOverflow({ overflow: TextOverflow.Ellipsis })
            // 用 sys.symbol 箭头：上一版用文字 '↑/↓'，真机截图上几乎看不见字形
            this.layoutStepBtn($r('sys.symbol.chevron_up'), idx > 0, idx, -1)
            this.layoutStepBtn($r('sys.symbol.chevron_down'),
              idx < this.layoutOpts.length - 1, idx, 1)
          }
          .width('100%')
          .padding({ left: 10, right: 10, top: 6, bottom: 6 })
          .borderRadius(12)
          .backgroundColor($r('app.color.card_bg'))
        }, (o: MiotLayoutOption): string => o.key)
        if (this.layoutWarn.length > 0) {
          // 逐项列被拒原因（Rust 文本）。有拒绝就明说，不做"悄悄少显示几个"的降级
          Column({ space: 2 }) {
            Text('这些项没通过当前固件校验，已被拒绝：')
              .fontSize(12)
              .fontColor($r('app.color.error_red'))
            ForEach(this.layoutWarn, (w: string) => {
              Text('· ' + w)
                .fontSize(11)
                .fontColor($r('app.color.error_red'))
                .maxLines(2)
                .textOverflow({ overflow: TextOverflow.Ellipsis })
            }, (w: string, idx: number): string => `${idx}:${w}`)
          }
          .width('100%')
          .alignItems(HorizontalAlign.Start)
          .padding({ left: 10, right: 10, top: 8, bottom: 8 })
          .borderRadius(12)
          .backgroundColor($r('app.color.red_tint'))
        }
        if (this.layoutMsg !== '') {
          Text(this.layoutMsg)
            .fontSize(12)
            .fontColor($r('app.color.text_secondary'))
        }
        Row({ space: 10 }) {
          Button('保存')
            .height(36)
            .fontSize(13)
            .borderRadius(12)
            .backgroundColor($r('app.color.brand'))
            .fontColor(Color.White)
            .enabled(!this.layoutBusy)
            .onClick((): void => {
              this.saveLayout();
            })
          Button('取消')
            .height(36)
            .fontSize(13)
            .borderRadius(12)
            .backgroundColor($r('app.color.neutral_tint'))
            .fontColor($r('app.color.text_primary'))
            .onClick((): void => {
              this.cancelLayoutEdit();
            })
        }
        .width('100%')
        Row({ space: 10 }) {
          Button('导出档案')
            .height(36)
            .fontSize(13)
            .borderRadius(12)
            .backgroundColor($r('app.color.neutral_tint'))
            .fontColor($r('app.color.text_primary'))
            .enabled(!this.layoutBusy)
            .onClick((): void => {
              this.exportProfile();
            })
          Button('导入档案')
            .height(36)
            .fontSize(13)
            .borderRadius(12)
            .backgroundColor($r('app.color.neutral_tint'))
            .fontColor($r('app.color.text_primary'))
            .enabled(!this.layoutBusy)
            .onClick((): void => {
              this.importProfile();
            })
        }
        .width('100%')
      }
    }
    .width('100%')
    .alignItems(HorizontalAlign.Start)
  }

  /**
   * 编辑器里的排序按钮。用 `sys.symbol` 箭头而不是文字 `↑/↓`——
   * 上一版用字符，真机截图上几乎看不出字形，等于把功能藏起来了。
   */
  @Builder
  layoutStepBtn(icon: Resource, enabled: boolean, idx: number, delta: number) {
    Button() {
      SymbolGlyph(icon)
        .fontSize(15)
        .fontColor([enabled ? $r('app.color.brand') : $r('app.color.text_disabled')])
    }
    .width(34)
    .height(30)
    .borderRadius(9)
    .backgroundColor($r('app.color.neutral_tint'))
    .enabled(enabled)
    .onClick((): void => {
      this.moveLayoutItem(idx, delta);
    })
  }

  /** 步进圆形按钮：±（sys.symbol），禁用灰显 */
  @Builder
  stepBtn(it: MiotCardItem, dir: number) {
    Row() {
      SymbolGlyph(dir < 0 ? $r('sys.symbol.minus') : $r('sys.symbol.plus'))
        .fontSize(13)
        .fontWeight(FontWeight.Medium)
        .fontColor([it.enabled && !this.controlBusy ? $r('app.color.brand') : $r('app.color.text_disabled')])
    }
    .width(30)
    .height(30)
    .borderRadius(15)
    .backgroundColor(it.enabled && !this.controlBusy ? $r('app.color.brand_tint') : $r('app.color.neutral_tint'))
    .justifyContent(FlexAlign.Center)
    .onClick((): void => {
      this.clampStep(it, dir);
    })
  }

  /** 单个控制项行：按 kind 渲染可交互控件（本期授权：不再 disabled 预览） */
  @Builder
  ctlRow(it: MiotCardItem, idx: number) {
    Column({ space: 8 }) {
      if (it.kind === 'toggle') {
        Row() {
          Text(it.label).fontSize(14).fontColor($r('app.color.text_body')).maxLines(2).textOverflow({ overflow: TextOverflow.Ellipsis })
          Blank()
          Toggle({ type: ToggleType.Switch, isOn: this.isOnValue(it.value) })
            .selectedColor($r('app.color.brand'))
            .enabled(it.enabled && !this.controlBusy)
            .onChange((isOn: boolean) => {
              if (isOn !== this.isOnValue(it.value)) {
                this.dispatch(it, this.toggleWriteValue(it, isOn));
              }
            })
        }
        .width('100%')
      } else if (it.kind === 'stepper') {
        Row({ space: 8 }) {
          Text(it.label).fontSize(14).fontColor($r('app.color.text_body')).maxLines(2).textOverflow({ overflow: TextOverflow.Ellipsis })
            .layoutWeight(1)
          this.stepBtn(it, -1)
          Text(this.ctlValueText(it))
            .fontSize(15).fontWeight(FontWeight.Medium).fontColor($r('app.color.text_primary'))
            .constraintSize({ minWidth: 72 }).textAlign(TextAlign.Center)
          this.stepBtn(it, 1)
        }
        .width('100%')
      } else if (it.kind === 'seek_bar') {
        Column({ space: 2 }) {
          Row() {
            Text(it.label).fontSize(14).fontColor($r('app.color.text_body')).maxLines(2).textOverflow({ overflow: TextOverflow.Ellipsis })
            Blank()
            Text(this.ctlValueText(it))
              .fontSize(14).fontWeight(FontWeight.Medium).fontColor($r('app.color.text_primary'))
          }
          .width('100%')
          Slider({
            value: this.sliderValue(it, idx),
            min: this.sliderMin(it),
            max: this.sliderMax(it),
            step: it.step ?? 1,
            style: SliderStyle.OutSet
          })
            .selectedColor($r('app.color.brand'))
            .enabled(it.enabled && !this.controlBusy)
            .onChange((v: number, mode: SliderChangeMode) => {
              if (mode === SliderChangeMode.Begin || mode === SliderChangeMode.Moving) {
                this.dragIdx = idx;
                this.dragVal = v;
              } else {
                this.dispatch(it, Math.round(v * 100) / 100);
              }
            })
        }
        .width('100%')
      } else if (it.kind === 'enum_choice' || it.kind === 'gear') {
        Column({ space: 6 }) {
          Row() {
            Text(it.label).fontSize(14).fontColor($r('app.color.text_body')).maxLines(2).textOverflow({ overflow: TextOverflow.Ellipsis })
            Blank()
            Text(this.ctlValueText(it)).fontSize(12).fontColor($r('app.color.text_secondary'))
          }
          .width('100%')
          Flex({ wrap: FlexWrap.Wrap }) {
            ForEach(it.choices, (c: MiotCardChoice) => {
              Text(c.label === '' ? String(c.value) : c.label)
                .fontSize(12)
                .fontColor(c.value === it.value && it.enabled ? Color.White :
                  (it.enabled ? $r('app.color.brand') : $r('app.color.text_disabled')))
                .backgroundColor(c.value === it.value && it.enabled ? $r('app.color.brand') :
                  (it.enabled ? $r('app.color.brand_tint') : $r('app.color.neutral_tint')))
                .padding({ left: 14, right: 14, top: 7, bottom: 7 })
                .borderRadius(16)
                .margin({ right: 8, bottom: 4 })
                .onClick((): void => {
                  if (c.value !== it.value) {
                    this.dispatch(it, c.value);
                  }
                })
            }, (c: MiotCardChoice, ci: number): string => `${idx}_${ci}_${String(c.value)}`)
          }
          .width('100%')
        }
        .width('100%')
      } else if (it.kind === 'action') {
        Row() {
          Text(it.label).fontSize(14).fontColor($r('app.color.text_body')).maxLines(2).textOverflow({ overflow: TextOverflow.Ellipsis })
            .layoutWeight(1)
          Button(it.isAction ? '执行' : '切换')
            .height(30)
            .fontSize(13)
            .borderRadius(15)
            .padding({ left: 16, right: 16 })
            .backgroundColor($r('app.color.brand'))
            .enabled(it.enabled && !this.controlBusy)
            .onClick((): void => {
              this.dispatchActionItem(it);
            })
        }
        .width('100%')
      } else {
        Row() {
          Text(it.label).fontSize(14).fontColor($r('app.color.text_body')).maxLines(2).textOverflow({ overflow: TextOverflow.Ellipsis })
            .layoutWeight(1)
          Text(this.ctlValueText(it))
            .fontSize(14).fontWeight(FontWeight.Medium).fontColor($r('app.color.text_primary'))
            .margin({ left: 12 })
        }
        .width('100%')
      }
    }
    .width('100%')
    .padding({ left: 14, right: 14, top: 12, bottom: 12 })
    .backgroundColor($r('app.color.card_bg'))
    .borderRadius(12)
    .opacity(it.enabled ? 1 : 0.55)
  }

  /**
   * 房间分组标题。未分组（room=''）只在真的存在多组时才出现标题；
   * 只有一组时整页退化成原来的平铺栅格（见 isFlatMode）。
   */
  @Builder
  roomHeader(g: RoomGroup) {
    Row({ space: 8 }) {
      Text(g.name === '' ? '未分组' : g.name)
        .fontSize(16)
        .fontWeight(FontWeight.Medium)
        .fontColor($r('app.color.text_primary'))
        .maxLines(1)
        .textOverflow({ overflow: TextOverflow.Ellipsis })
      // 只报台数：在线数是卡片对象的普通字段，组标题没有 @ObjectLink 联动，
      // 写「x 在线」会滞后于实际状态，宁可不写。
      Text(`${g.cards.length} 台`)
        .fontSize(12)
        .fontColor($r('app.color.text_secondary'))
        .maxLines(1)
      Blank()
    }
    .width('100%')
    .padding({ left: 16, right: 16, top: 10, bottom: 6 })
  }

  /** 只有一组且是「未分组」→ 不显示任何房间标题，视觉上等于旧版平铺 */
  private isFlatMode(): boolean {
    return this.groups.length <= 1;
  }

  /**
   * 按房间把卡片分组。卡片实例**不新建**（沿用 mergeCards 复用下来的对象），
   * 组顺序用云端 roomRank 升序、未分组垫底；组内顺序就是已锚定的卡片顺序。
   */
  private rebuildGroups(): void {
    const byName: Map<string, RoomGroup> = new Map();
    const order: RoomGroup[] = [];
    for (const c of this.cards) {
      let g: RoomGroup | undefined = byName.get(c.room);
      if (g === undefined) {
        g = new RoomGroup();
        g.name = c.room;
        g.rank = c.roomRank;
        byName.set(c.room, g);
        order.push(g);
      }
      // 组序号取组内最小值，避免同一房间因个别设备缺 rank 而漂位
      if (g.rank < 0 && c.roomRank >= 0) {
        g.rank = c.roomRank;
      }
      g.cards.push(c);
    }
    order.sort((a: RoomGroup, b: RoomGroup): number => {
      const ra: number = a.rank < 0 ? RANK_UNASSIGNED : a.rank;
      const rb: number = b.rank < 0 ? RANK_UNASSIGNED : b.rank;
      if (ra !== rb) {
        return ra - rb;
      }
      return a.name < b.name ? -1 : (a.name > b.name ? 1 : 0);
    });
    this.groups = order;
  }

  /**
   * 分组后每组是独立 Grid，必须自带高度（Grid 是滚动容器，不约束高度会占满一屏）。
   * 行数 = ceil(台数 / 列数)，高度与 DeviceCardView 的 CARD_H_VP / rowsGap 同源。
   */
  private groupGridHeight(count: number): number {
    const cols: number = this.gridCols > 0 ? this.gridCols : 2;
    const rows: number = Math.ceil(count / cols);
    if (rows <= 0) {
      return 0;
    }
    return rows * CARD_H_VP + (rows - 1) * CARD_GAP_VP;
  }

  /** 「我的 → 卡片排序」用的动态说明（措辞保持平台中性，不出现三方品牌） */
  private groupSummary(): string {
    if (this.cards.length === 0) {
      return '登录后这里会按设备所属房间分组。';
    }
    if (this.isFlatMode()) {
      return `当前 ${this.cards.length} 台设备都没有房间归属（房间是平台侧数据，` +
        '需在平台 App 里分配），首页平铺不分组。';
    }
    let unassigned: number = 0;
    for (const g of this.groups) {
      if (g.name === '') {
        unassigned = g.cards.length;
      }
    }
    return `房间分组 ${this.groups.length} 组 · 未分组 ${unassigned} 台。` +
      '组顺序跟随平台返回的房间顺序，未分组垫底；组内卡片仍按设备 ID 锚定，刷新不跳位。';
  }

  /** Tab 1「设备」：卡片栅格 + 圆形快控（原首页 build 主体） */
  @Builder
  devicesTab() {
    Column() {
      // ---------- 头部 ----------
      Row() {
        Column({ space: 2 }) {
          Text('MineHome')
            .fontSize(24)
            .fontWeight(FontWeight.Bold)
            .fontColor($r('app.color.text_primary'))
          Text(this.statusText)
            .fontSize(12)
            .fontColor($r('app.color.text_secondary'))
        }
        .alignItems(HorizontalAlign.Start)
        .layoutWeight(1)

        // 刷新收成图标钮；会话动作（退出）归到「我的」
        if (this.loggedIn) {
          this.refreshBtn()
        }
      }
      .width('100%')
      .padding({ left: 16, right: 16, top: 14, bottom: 8 })

      // ---------- 已登录：按房间分组的卡片栅格（组内仍 did 锚定，位置不跳） ----------
      if (this.loggedIn && this.cards.length > 0) {
        // 外层改成 Scroll：每组一个自带高度的 Grid（Grid 是可滚动容器，不能直接
        // 塞进 Column，否则每组都想占满一屏）。列数算法与口径保持不变：
        // onAreaChange 量的是内容列宽（= 旧 Grid 外框宽），左右 16 仍留在 Grid 内部。
        Scroll() {
          Column() {
            ForEach(this.groups, (g: RoomGroup) => {
              if (!this.isFlatMode()) {
                this.roomHeader(g)
              }
              Grid() {
                ForEach(g.cards, (c: DeviceCard) => {
                  GridItem() {
                    DeviceCardView({
                      card: c,
                      onTap: (): void => {
                        this.openDevice(c);
                      },
                      onSwitchTap: (card: DeviceCard): void => {
                        this.quickToggle(card);
                      }
                    })
                  }
                // key 只用 did：对象实例本身按 did 复用（见 mergeCards），
                // 身份稳定 → @ObjectLink 持续联动 → 栅格既不重建也不跳位
                }, (c: DeviceCard): string => c.did)
              }
              .columnsTemplate(this.columnsTemplate())
              .rowsGap(CARD_GAP_VP)
              .columnsGap(CARD_GAP_VP)
              .padding({ left: 16, right: 16 })
              .height(this.groupGridHeight(g.cards.length))
              .scrollBar(BarState.Off)
              .enableScrollInteraction(false)
            }, (g: RoomGroup): string => g.name)
          }
          .width('100%')
          // 底部留白：悬浮切换条盖在栅格上，最后一行必须能滚出来
          .padding({ bottom: 108 })
          .onAreaChange((oldValue: Area, newValue: Area): void => {
            this.applyGridCols(Number(newValue.width));
          })
        }
        .layoutWeight(1)
        .scrollBar(BarState.Off)
        .edgeEffect(EdgeEffect.Spring)
        .align(Alignment.Top)
      } else {
        // 两种空态分开说：未登录（去登录）/ 已登录但账号下没设备（去米家加设备）
        Column() {
          if (this.loggedIn) {
            this.emptyHint('这个账号下还没有设备',
              '先在设备所属平台添加设备，回来点右上角刷新即可拉取。')
            Button('重新拉取')
              .height(40)
              .fontSize(14)
              .backgroundColor($r('app.color.brand'))
              .enabled(!this.busy)
              .margin({ top: 6, bottom: 132 })
              .onClick((): void => {
                this.loadDevices();
              })
          } else {
            this.emptyHint('还没有设备', '到「我的」连接三方平台，或导入已导出的会话文件')
            Button('去登录')
              .height(40)
              .fontSize(14)
              .backgroundColor($r('app.color.brand'))
              .margin({ top: 6, bottom: 132 })
              .onClick((): void => {
                this.goToTab(TAB_ME);
              })
          }
        }
        .width('100%')
        .layoutWeight(1)
        .justifyContent(FlexAlign.Center)
        .alignItems(HorizontalAlign.Center)
      }
    }
    .width('100%')
    .height('100%')
  }

  /** Tab 2「监控」：全部设备观察值一屏清单（复用首页已取 DTO，零额外请求） */
  @Builder
  monitorTab() {
    Column() {
      Row() {
        Column({ space: 2 }) {
          Text('状态监控')
            .fontSize(24)
            .fontWeight(FontWeight.Bold)
            .fontColor($r('app.color.text_primary'))
          Text(this.loggedIn ?
            (this.cards.length === 0 ? '账号下暂无设备可监控' :
              `在线 ${this.onlineCount()} / ${this.cards.length} 台 · 刷新于 ${this.listUpdatedAt}`) :
            '登录后这里汇总温湿度、门磁、电量等观察值')
            .fontSize(12)
            .fontColor($r('app.color.text_secondary'))
            .maxLines(2)
        }
        .alignItems(HorizontalAlign.Start)
        .layoutWeight(1)

        if (this.loggedIn) {
          this.refreshBtn()
        }
      }
      .width('100%')
      .padding({ left: 16, right: 16, top: 14, bottom: 8 })

      if (this.loggedIn && this.cards.length > 0) {
        // 与首页共用同一套 groups（mergeCards 后 rebuildGroups 派生，零额外请求）；
        // 用原生 ListItemGroup 分组，标题吸顶，滚动时能一直看到当前房间名。
        List({ space: 8 }) {
          if (this.isFlatMode()) {
            this.monitorRowsOf(this.cards)
          } else {
            ForEach(this.groups, (g: RoomGroup) => {
              ListItemGroup({ space: 8, header: this.monitorGroupHeader(g) }) {
                this.monitorRowsOf(g.cards)
              }
            }, (g: RoomGroup): string => g.name)
          }
        }
        .width('100%')
        .layoutWeight(1)
        .scrollBar(BarState.Off)
        .edgeEffect(EdgeEffect.Spring)
        .sticky(StickyStyle.Header)
        .padding({ left: 16, right: 16, bottom: 120 })
      } else {
        this.emptyHint(this.loggedIn ? '这个账号下还没有设备' : '暂无数据',
          this.loggedIn ? '先在设备所属平台添加设备，刷新后这里会列出每台设备的在线状态与观察值。' :
            '登录后这里列出每台设备的在线状态与观察值')
      }
    }
    .width('100%')
    .height('100%')
  }

  /** 监控页分组标题（吸顶用）：房间名 + 台数，弱化成小节样式，不和设备行抢视觉 */
  @Builder
  monitorGroupHeader(g: RoomGroup) {
    Row({ space: 8 }) {
      Text(g.name === '' ? '未分组' : g.name)
        .fontSize(13)
        .fontWeight(FontWeight.Medium)
        .fontColor($r('app.color.text_secondary'))
        .maxLines(1)
        .textOverflow({ overflow: TextOverflow.Ellipsis })
      Text(`${g.cards.length} 台`)
        .fontSize(11)
        .fontColor($r('app.color.text_disabled'))
      Blank()
    }
    .width('100%')
    .height(32)
    .padding({ left: 12, right: 12 })
    .borderRadius(10)
    .backgroundColor($r('app.color.neutral_tint'))
  }

  /** 监控行主体：平铺与分组两条分支共用，避免复制一份 ForEach */
  @Builder
  monitorRowsOf(list: DeviceCard[]) {
    ForEach(list, (c: DeviceCard) => {
      ListItem() {
        this.monitorRow(c)
      }
    }, (c: DeviceCard): string => c.did)
  }

  /** 监控行：图标 + 名称 + 指标/状态 + 在线胶囊；点击同样打开二级弹层 */
  @Builder
  monitorRow(c: DeviceCard) {
    Row({ space: 10 }) {
      Row() {
        SymbolGlyph(c.icon)
          .fontSize(18)
          .fontWeight(FontWeight.Medium)
          .fontColor([c.online ? c.iconFg : $r('app.color.text_disabled')])
      }
      .width(36)
      .height(36)
      .borderRadius(12)
      .backgroundColor(c.online ? c.iconBg : $r('app.color.icon_bg_offline'))
      .justifyContent(FlexAlign.Center)

      Column({ space: 3 }) {
        Text(c.name)
          .fontSize(14)
          .fontWeight(FontWeight.Medium)
          .fontColor(c.online ? $r('app.color.text_primary') : $r('app.color.text_faded'))
          .maxLines(1)
          .textOverflow({ overflow: TextOverflow.Ellipsis })
        if (c.metrics.length > 0) {
          Row({ space: 12 }) {
            ForEach(c.metrics, (m: CardMetric) => {
              Row({ space: 4 }) {
                SymbolGlyph(m.icon)
                  .fontSize(13)
                  .fontColor([m.color])
                Text(m.text)
                  .fontSize(12)
                  .fontColor($r('app.color.text_secondary'))
                if (m.local) {
                  Text('蓝牙')
                    .fontSize(8)
                    .fontColor($r('app.color.brand'))
                    .backgroundColor($r('app.color.brand_tint'))
                    .padding({ left: 4, right: 4, top: 1, bottom: 1 })
                    .borderRadius(999)
                }
              }
            }, (m: CardMetric): string => `${m.key}_${m.text}`)
          }
        } else {
          Text(c.status === '' ? (c.online ? '在线' : '离线') : c.status)
            .fontSize(12)
            .fontColor($r('app.color.text_secondary'))
            .maxLines(1)
            .textOverflow({ overflow: TextOverflow.Ellipsis })
        }
      }
      .alignItems(HorizontalAlign.Start)
      .layoutWeight(1)

      Text(!c.online ? '离线' : (c.quickSwitch ? (c.onState ? '开' : '关') : '在线'))
        .fontSize(11)
        .fontColor(!c.online ? $r('app.color.text_disabled') :
          (c.hasSwitch && c.onState ? Color.White : $r('app.color.brand')))
        .backgroundColor(!c.online ? $r('app.color.neutral_tint') :
          (c.hasSwitch && c.onState ? $r('app.color.brand') : $r('app.color.brand_tint')))
        .padding({ left: 9, right: 9, top: 4, bottom: 4 })
        .borderRadius(999)
    }
    .width('100%')
    .padding({ left: 12, right: 12, top: 10, bottom: 10 })
    .borderRadius(14)
    .backgroundColor(c.online ? $r('app.color.card_bg') : $r('app.color.card_bg_offline'))
    .onClick((): void => {
      this.openDevice(c);
    })
  }

  /** Tab 3「我的」：会话 + 三种登录方式（主推小米登录页）+ 排序锚 + 说明 */
  @Builder
  accountTab() {
    Column() {
      Row() {
        Column({ space: 2 }) {
          Text('我的')
            .fontSize(24)
            .fontWeight(FontWeight.Bold)
            .fontColor($r('app.color.text_primary'))
          Text(this.loggedIn ?
            `设备 ${this.cards.length} 台 · 在线 ${this.onlineCount()} 台` :
            '未登录 · 任选一种方式建立本地会话')
            .fontSize(12)
            .fontColor($r('app.color.text_secondary'))
            .maxLines(2)
        }
        .alignItems(HorizontalAlign.Start)
        .layoutWeight(1)

        if (this.loggedIn) {
          this.refreshBtn()
        }
      }
      .width('100%')
      .padding({ left: 16, right: 16, top: 14, bottom: 8 })

      Scroll() {
        Column({ space: 12 }) {
          // ---------- 会话 ----------
          Column({ space: 10 }) {
            Row({ space: 10 }) {
              Row() {
                SymbolGlyph(this.loggedIn ? $r('sys.symbol.checkmark_circle_fill') :
                  $r('sys.symbol.lock_shield'))
                  .fontSize(20)
                  .fontColor([this.loggedIn ? $r('app.color.online_green') :
                    $r('app.color.text_secondary')])
              }
              .width(40)
              .height(40)
              .borderRadius(14)
              .backgroundColor(this.loggedIn ? $r('app.color.green_tint') : $r('app.color.neutral_tint'))
              .justifyContent(FlexAlign.Center)

              Column({ space: 2 }) {
                Text(this.loggedIn ? '会话已就绪' : '未登录')
                  .fontSize(15)
                  .fontWeight(FontWeight.Medium)
                  .fontColor($r('app.color.text_primary'))
                Text(this.loggedIn ? `最近刷新 ${this.listUpdatedAt}` : this.statusText)
                  .fontSize(12)
                  .fontColor($r('app.color.text_secondary'))
                  .maxLines(2)
                  .textOverflow({ overflow: TextOverflow.Ellipsis })
              }
              .alignItems(HorizontalAlign.Start)
              .layoutWeight(1)
            }
            .width('100%')

            if (this.loggedIn) {
              Row({ space: 10 }) {
                Button('刷新设备')
                  .layoutWeight(1)
                  .height(36)
                  .fontSize(13)
                  .borderRadius(12)
                  .backgroundColor($r('app.color.brand'))
                  .enabled(!this.busy)
                  .onClick((): void => {
                    this.loadDevices();
                  })
                Button('退出会话')
                  .layoutWeight(1)
                  .height(36)
                  .fontSize(13)
                  .borderRadius(12)
                  .backgroundColor($r('app.color.btn_gray_bg'))
                  .fontColor($r('app.color.btn_gray_text'))
                  .onClick((): void => {
                    this.doLogout();
                  })
              }
              .width('100%')
            }
          }
          .width('100%')
          .padding(14)
          .borderRadius(16)
          .backgroundColor($r('app.color.card_bg'))

          // ---------- 登录方式 ----------
          Column({ space: 10 }) {
            this.sectionLabel('登录')
            // 主路径：跳小米官方登录页（凭据只进小米，本应用只提 cookie 换会话）
            Button('连接三方平台')
              .width('100%')
              .height(44)
              .fontSize(15)
              .borderRadius(14)
              .backgroundColor($r('app.color.brand'))
              .enabled(!this.busy)
              .onClick((): void => {
                this.getUIContext().getRouter().pushUrl({ url: 'pages/PlatformLogin' });
              })
            Text('登录页面由平台方提供，支持密码 / 短信验证码 / 扫码。本应用只在登录完成后建立连接，不接触你的账号密码。')
              .fontSize(11)
              .fontColor($r('app.color.text_secondary'))
              .width('100%')

            Row({ space: 10 }) {
              Button('导入 session.json')
                .layoutWeight(1)
                .height(38)
                .fontSize(13)
                .borderRadius(12)
                .backgroundColor($r('app.color.brand_tint'))
                .fontColor($r('app.color.brand'))
                .enabled(!this.busy)
                .onClick((): void => {
                  this.doImport();
                })
              Button(this.showPasswordLogin ? '收起密码登录' : '密码登录（备用）')
                .layoutWeight(1)
                .height(38)
                .fontSize(13)
                .borderRadius(12)
                .backgroundColor($r('app.color.btn_gray_bg'))
                .fontColor($r('app.color.btn_gray_text'))
                .onClick((): void => {
                  this.showPasswordLogin = !this.showPasswordLogin;
                })
            }
            .width('100%')

            if (this.showPasswordLogin) {
              Column({ space: 8 }) {
                TextInput({ placeholder: '账号（手机号 / 邮箱）' })
                  .width('100%')
                  .height(40)
                  .fontSize(14)
                  .backgroundColor($r('app.color.neutral_tint'))
                  .borderRadius(12)
                  .onChange((v: string) => {
                    this.loginUserName = v;
                  })
                TextInput({ placeholder: '密码' })
                  .type(InputType.Password)
                  .width('100%')
                  .height(40)
                  .fontSize(14)
                  .backgroundColor($r('app.color.neutral_tint'))
                  .borderRadius(12)
                  .onChange((v: string) => {
                    this.loginUserPass = v;
                  })
                Button(this.busy ? '登录中…' : '密码登录')
                  .width('100%')
                  .height(38)
                  .fontSize(14)
                  .borderRadius(12)
                  .backgroundColor($r('app.color.brand'))
                  .enabled(!this.busy)
                  .onClick((): void => {
                    this.doLogin();
                  })
                Text('新设备首次密码登录会走「通知确认」，页面自动打开确认链接。')
                  .fontSize(11)
                  .fontColor($r('app.color.text_secondary'))
                  .width('100%')
              }
              .width('100%')
            }

            // 换账号必备入口：小米 H5 认 cookie，passToken 还在的话登录页会跳过
            // 表单直接回调换会话 —— 不清就永远换不了第二个小米账号
            Button('退出网页登录态并换账号')
              .width('100%')
              .height(38)
              .fontSize(13)
              .borderRadius(12)
              .backgroundColor($r('app.color.btn_gray_bg'))
              .fontColor($r('app.color.btn_gray_text'))
              .margin({ top: 4 })
              .onClick((): void => {
                this.doSwitchAccount();
              })
            Text('实测只清 cookie 不够：平台方页面会把登录态放进站点存储，裸登录页几秒内能静默换回凭据。' +
              '这里先走平台方登出页断会话，再清站点存储与 cookie，然后打开真正的登录界面；已建立的连接不受影响。')
              .fontSize(11)
              .fontColor($r('app.color.text_secondary'))
              .width('100%')
          }
          .width('100%')
          .padding(14)
          .borderRadius(16)
          .backgroundColor($r('app.color.card_bg'))
          .alignItems(HorizontalAlign.Start)

          // ---------- 房间分组 + 栅格排序锚 ----------
          Column({ space: 8 }) {
            this.sectionLabel('卡片分组与排序')
            Text(this.groupSummary())
              .fontSize(11)
              .fontColor($r('app.color.text_secondary'))
              .width('100%')
            Button('重置卡片排序')
              .height(34)
              .fontSize(13)
              .borderRadius(12)
              .backgroundColor($r('app.color.brand_tint'))
              .fontColor($r('app.color.brand'))
              .onClick((): void => {
                this.resetCardOrder();
              })
          }
          .width('100%')
          .padding(14)
          .borderRadius(16)
          .backgroundColor($r('app.color.card_bg'))
          .alignItems(HorizontalAlign.Start)

          // ---------- 行为说明 ----------
          Column({ space: 6 }) {
            this.sectionLabel('说明')
            Text(`一键开关已放行 ${this.quickAllowedCount} 台（只对模板主开关 == spec 电源属性的设备开放；` +
              '路由器/网关等基础设施一律不给圆钮，仍可在弹层里按明确项操作）')
              .fontSize(11)
              .fontColor($r('app.color.text_secondary'))
              .width('100%')
            Text('· 圆钮下发值取服务端卡片模板归一化后的默认预设（bool / 0-1 / 字符串自适应）')
              .fontSize(11)
              .fontColor($r('app.color.text_secondary'))
              .width('100%')
            Text('· 蓝牙直连设备（blt.*）云侧不支持下发，圆钮给出降级提示并保留实时监测')
              .fontSize(11)
              .fontColor($r('app.color.text_secondary'))
              .width('100%')
            Text('· 会话与 spec 缓存只存在本机应用沙盒，退出会话即删除')
              .fontSize(11)
              .fontColor($r('app.color.text_secondary'))
              .width('100%')
          }
          .width('100%')
          .padding(14)
          .borderRadius(16)
          .backgroundColor($r('app.color.card_bg'))
          .alignItems(HorizontalAlign.Start)

          // 悬浮条遮挡补偿
          Row().height(128)
        }
        .width('100%')
        .padding({ left: 16, right: 16 })
      }
      .layoutWeight(1)
      .scrollBar(BarState.Off)
      .align(Alignment.Top)
    }
    .width('100%')
    .height('100%')
  }

  /** 引导空态（图标 + 主副文案） */
  @Builder
  emptyHint(title: string, desc: string) {
    Column({ space: 10 }) {
      Row() {
        SymbolGlyph($r('sys.symbol.house_equipment'))
          .fontSize(30)
          .fontColor([$r('app.color.text_secondary')])
      }
      .width(64)
      .height(64)
      .borderRadius(22)
      .backgroundColor($r('app.color.neutral_tint'))
      .justifyContent(FlexAlign.Center)

      Text(title)
        .fontSize(16)
        .fontWeight(FontWeight.Medium)
        .fontColor($r('app.color.text_primary'))
      Text(desc)
        .fontSize(12)
        .fontColor($r('app.color.text_secondary'))
        .textAlign(TextAlign.Center)
        .maxLines(3)
        .padding({ left: 20, right: 20 })
    }
    .width('100%')
    .justifyContent(FlexAlign.Center)
  }

  /** 刷新图标钮（三个 Tab 头部共用） */
  @Builder
  refreshBtn() {
    Row() {
      SymbolGlyph($r('sys.symbol.arrow_2_circlepath'))
        .fontSize(18)
        .fontColor([this.busy ? $r('app.color.text_disabled') : $r('app.color.brand')])
    }
    .width(36)
    .height(36)
    .borderRadius(18)
    .backgroundColor($r('app.color.brand_tint'))
    .justifyContent(FlexAlign.Center)
    .opacity(this.busy ? 0.6 : 1)
    .onClick((): void => {
      this.loadDevices();
    })
  }

  /** 悬浮切换条单项：图标 + 文案，选中态品牌色 */
  @Builder
  tabItemView(idx: number, label: string, icon: Resource) {
    Column({ space: 3 }) {
      SymbolGlyph(icon)
        .fontSize(21)
        .fontWeight(this.currentTab === idx ? FontWeight.Medium : FontWeight.Regular)
        .fontColor([this.currentTab === idx ? $r('app.color.brand') : $r('app.color.text_secondary')])
      Text(label)
        .fontSize(10)
        .fontWeight(this.currentTab === idx ? FontWeight.Medium : FontWeight.Regular)
        .fontColor(this.currentTab === idx ? $r('app.color.brand') : $r('app.color.text_secondary'))
    }
    .width('100%')
    .height('100%')
    .justifyContent(FlexAlign.Center)
  }

  private onlineCount(): number {
    let n: number = 0;
    for (const c of this.cards) {
      if (c.online) {
        n++;
      }
    }
    return n;
  }

  /** 栅格列模板：按当前列数生成 "1fr 1fr ..." */
  private columnsTemplate(): string {
    let s: string = '1fr';
    for (let i = 1; i < this.gridCols; i++) {
      s = `${s} 1fr`;
    }
    return s;
  }

  /** 目标卡宽 ≈260vp：手机 2 列，平板/折叠展开最多 5 列 */
  private applyGridCols(widthVp: number): void {
    if (Number.isNaN(widthVp) || widthVp <= 0) {
      return;
    }
    const n: number = Math.round((widthVp + 12) / 272);
    const cols: number = n < 2 ? 2 : (n > 5 ? 5 : n);
    if (cols !== this.gridCols) {
      this.gridCols = cols;
    }
  }

  /** 退出本地会话（原「退出会话（调试）」，从首页头部迁到「我的」） */
  private doLogout(): void {
    miot.logout();
    this.loggedIn = false;
    this.cards = [];
    this.listUpdatedAt = '';
    this.statusText = '已退出会话';
    this.getUIContext().getPromptAction().showToast({ message: '已清除本地会话' });
  }

  /**
   * 换账号：带 logoutFirst 打开登录页 —— 由登录页依次做「小米官方登出 →
   * 清站点存储与 cookie → 重新加载登录表单」。米家会话（session.json）与排序锚不动。
   */
  private doSwitchAccount(): void {
    const p: LoginRouteParam = new LoginRouteParam();
    p.logoutFirst = true;
    this.getUIContext().getRouter().pushUrl({ url: 'pages/PlatformLogin', params: p });
  }

  /**
   * 悬浮条沉浸材质（API 26 uiMaterial）。
   * 设备不支持沉浸材质时系统自动忽略该设置，退化为普通底色条。
   */
  private barMaterial(): uiMaterial.ImmersiveMaterial {
    return new uiMaterial.ImmersiveMaterial({
      style: uiMaterial.ImmersiveStyle.REGULAR,
      materialColor: $r('app.color.bar_material_tint'),
      interactive: true
    });
  }

  /**
   * 根导航：底部 API 26 悬浮切换条（Tabs.barFloatingStyle）。
   * 二级弹层仍绑在根容器上，弹出时覆盖整屏宽度，与切换条互不干扰。
   */
  build() {
    Tabs({ barPosition: BarPosition.End, index: this.currentTab, controller: this.tabsController }) {
      TabContent() {
        this.devicesTab()
      }
      .tabBar(this.tabItemView(TAB_HOME, '设备', $r('sys.symbol.house_fill')))

      TabContent() {
        this.monitorTab()
      }
      .tabBar(this.tabItemView(TAB_MONITOR, '监控', $r('sys.symbol.waveform')))

      TabContent() {
        this.accountTab()
      }
      .tabBar(this.tabItemView(TAB_ME, '我的', $r('sys.symbol.person_2_fill')))
    }
    .barMode(BarMode.Fixed)
    // 悬浮条：内容延伸到条下方，靠 maskColor/maskHeight 渐变收尾（API 26 barFloatingStyle）
    .barOverlap(true)
    .barHeight(58)
    .scrollable(true)
    .animationDuration(240)
    .barFloatingStyle({
      barWidth: { smallBarWidth: 216, mediumBarWidth: 268, largeBarWidth: 320 },
      barSideMargin: 16,
      barBottomMargin: 10,
      maskColor: $r('app.color.page_bg'),
      maskHeight: 120,
      systemMaterial: this.barMaterial()
    })
    .onChange((idx: number) => {
      this.currentTab = idx;
    })
    .width('100%')
    .height('100%')
    .backgroundColor($r('app.color.page_bg'))
    // 二级卡片半模态弹层（API 26 bindSheet：$$ 双向绑定 + 挡位 + 生命周期）
    .bindSheet($$this.showSheet, this.sheetBuilder(), {
      // 宽屏（平板/折叠展开）默认会切成无蒙层的居中面板，外部点击无法收回；
      // 强制底部半模态恢复系统蒙层（DismissReason.TOUCH_OUTSIDE）行为
      preferType: SheetType.BOTTOM,
      detents: [SheetSize.MEDIUM, SheetSize.LARGE],
      blurStyle: BlurStyle.Thick,
      dragBar: true,
      // 编辑器入口做成弹层内容顶部的横幅（见 layoutEditorSection）。
      // 标题区不放按钮：CustomBuilder 形态在本机 SDK 上实测渲染不出内容，
      // 换它不如把入口放在一定渲染得出来的内容区。
      title: { title: this.sheetName, subtitle: this.sheetModel },
      onAppear: (): void => {
        this.startMonitor();
      },
      onDisappear: (): void => {
        this.stopMonitor();
        // 全局扫描继续跑（首页卡片实时刷新用）；这里只退订弹层的读数事件
        BleIngest.unsubscribe(this.onBleReading);
        // 档位 B：弹层关闭即结束 GATT 保活会话（省电 + 把连接还给米家）
        BleGatt.close();
        this.dragIdx = -1;
        this.pendingOptimistic.clear();
        this.clearBurst();
      }
    })
  }
}
