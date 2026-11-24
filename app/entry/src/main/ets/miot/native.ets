/**
 * @miot/sdk —— 原生桥 + Promise 领域 API
 * native 库 libmijia_napi（ohpm 本地包，ohos-rs 生成 index.d.ts 提供类型）。
 */

import {
  init as nativeInit,
  isLoggedIn as nativeIsLoggedIn,
  reloadSession as nativeReloadSession,
  logout as nativeLogout,
  loginPassword as nativeLoginPassword,
  finishNotification as nativeFinishNotification,
  getDevices as nativeGetDevices,
  getProperty as nativeGetProperty,
  getProperties as nativeGetProperties,
  setProperty as nativeSetProperty,
  callMethod as nativeCallMethod,
  cardItems as nativeCardItems,
  cardDetail as nativeCardDetail,
  quickSwitch as nativeQuickSwitch,
  cardLayoutOptions as nativeCardLayoutOptions,
  exportCardProfile as nativeExportCardProfile,
  cardProfileWarnings as nativeCardProfileWarnings,
  cardLayoutPreview as nativeCardLayoutPreview,
  callAction as nativeCallAction,
  textIsSpecToken as nativeTextIsSpecToken,
  bleDecode as nativeBleDecode,
  bleDecodeMulti as nativeBleDecodeMulti,
  bleIngest as nativeBleIngest,
  blePeekMac as nativeBlePeekMac,
  bleBeaconkey as nativeBleBeaconkey,
  bleParseHt as nativeBleParseHt,
  JsDevice, JsLoginResult, JsPropValue, JsPropRef, JsCardItem, JsSwitchDecision,
  JsLayoutOption, JsLayoutPreview, JsHtReading
} from 'libmijia_napi.so';
import { common } from '@kit.AbilityKit';
import { fileIo as fs } from '@kit.CoreFileKit';
import { webview } from '@kit.ArkWeb';
import { hilog } from '@kit.PerformanceAnalysisKit';
import { Device, DeviceChangedEvent, LoginResult, MiotPropValue, NeedConfirmResult,
  Unsubscribe, MiotCardItem, MiotCardChoice, MiotSwitchDecision,
  MiotLayoutOption, MiotCardLayout, MiotLayoutPreview } from './types';

let pendingConfirmUrl: string = '';
let pollTimer: number = -1;

/**
 * 小米账号 H5 登录入口（sid=xiaomiio）：服务端 302 到
 * account.xiaomi.com/fe/service/login（官方登录页，支持密码/短信/扫码），
 * 登录成功后按 callback=sts.api.io.mi.com/sts 下发 xiaomiio 作用域的
 * passToken/userId cookie —— 本 App 用 passToken 重放 serviceLogin 换会话，
 * 因此 WebView 里输入的任何凭据都不经过我们的代码。
 */
const WEB_LOGIN_ENTRY: string = 'https://account.xiaomi.com/pass/serviceLogin?sid=xiaomiio';

/**
 * 小米官方扫码登录页（通用 H5 路由，不需要我们预取 qs/_sign）。
 * 仍显式带上 sid + callback：sid 保证作用域是米家，callback 给我们一个
 * 可拦截的"登录已完成"信号（拦不到也有 cookie 轮询兜底）。
 */
const WEB_LOGIN_QR: string =
  'https://account.xiaomi.com/fe/service/login/qrcode' +
  '?sid=xiaomiio&callback=https%3A%2F%2Fsts.api.io.mi.com%2Fsts';

/** 小米官方网页登出路由（实测 200；在 Web 内导航才能顺带清掉该 origin 的 storage） */
const WEB_LOGOUT: string = 'https://account.xiaomi.com/pass/logout';

/** passToken 探测域（.xiaomi.com 是落点，io/www 兜底，与确认页同口径） */
const WEB_LOGIN_PROBES: Array<string> =
  ['https://account.xiaomi.com/', 'https://io.mi.com/', 'https://www.mi.com/'];

function parseSpecText(text?: string): object | undefined {
  if (text === undefined || text === null || text === '') {
    return undefined;
  }
  return JSON.parse(text) as object;
}

function toDevice(d: JsDevice): Device {
  const dev: Device = {
    did: d.did,
    model: d.model,
    name: d.name,
    online: d.online
  };
  if (d.specType !== undefined && d.specType !== null) {
    dev.specType = d.specType;
  }
  const spec = parseSpecText(d.spec);
  if (spec !== undefined) {
    dev.spec = spec;
  }
  if (d.token !== undefined && d.token !== null) {
    dev.token = d.token;
  }
  if (d.mac !== undefined && d.mac !== null) {
    dev.mac = d.mac;
  }
  if (d.room !== undefined && d.room !== null) {
    dev.room = d.room;
  }
  // napi-derive 把 Rust 的 room_rank 转成 camelCase roomRank（与 spec_type→specType 同规则）
  if (d.roomRank !== undefined && d.roomRank !== null) {
    dev.roomRank = d.roomRank;
  }
  return dev;
}

/** JsCardItem（JSON 文本字段）→ MiotCardItem 领域对象（零模板解析） */
function mapJsCardItem(j: JsCardItem): MiotCardItem {
  let value: object | boolean | number | string | null = null;
  if (j.value !== undefined && j.value !== null && j.value !== '') {
    value = JSON.parse(j.value) as object | boolean | number | string | null;
  }
  let choices: MiotCardChoice[] = [];
  if (j.choices !== undefined && j.choices !== null && j.choices !== '') {
    choices = JSON.parse(j.choices) as MiotCardChoice[];
  }
  const item: MiotCardItem = {
    kind: j.kind,
    siid: j.siid,
    piid: j.piid,
    label: j.label,
    unit: j.unit,
    format: j.format,
    ratio: j.ratio,
    value: value,
    choices: choices,
    enabled: j.enabled,
    code: j.code,
    nameIgnore: j.nameIgnore,
    isAction: j.isAction
  };
  if (j.src !== undefined && j.src !== null) {
    item.src = j.src;
  }
  if (j.min !== undefined && j.min !== null) {
    item.min = j.min;
  }
  if (j.max !== undefined && j.max !== null) {
    item.max = j.max;
  }
  if (j.step !== undefined && j.step !== null) {
    item.step = j.step;
  }
  if (j.paramDelta !== undefined && j.paramDelta !== null) {
    item.paramDelta = j.paramDelta;
  }
  return item;
}

export interface MiotSdk {
  init(context: common.Context): void;
  isLoggedIn(): boolean;
  importSession(sessionJsonText: string, filesDir: string): boolean;
  logout(): void;
  login(user: string, pass: string): Promise<LoginResult>;
  pendingConfirmUrl(): string;
  tryFinishConfirmFromWeb(url: string): Promise<boolean>;
  /** 小米登录页 URL（供 Web 组件加载，sid=xiaomiio 作用域） */
  webLoginUrl(): string;
  /** 小米扫码登录页 URL（通用 H5 路由，手机米家 App 扫码确认） */
  webLoginQrUrl(): string;
  /** 小米网页登出 URL（换账号前必走：光清 cookie 会被前端持久层静默续登） */
  webLogoutUrl(): string;
  /** 网页登录收尾：从 WebView cookie 罐提 passToken+userId 换会话；成功 true */
  loginWithWebCookies(): Promise<boolean>;
  /** 清 WebView cookie（只动网页登录态，不碰 session.json 与排序锚） */
  clearWebCookies(): Promise<void>;
  /** 彻底清网页登录态：cookie + 站点 storage（localStorage/IndexedDB） */
  clearWebLoginState(): Promise<void>;
  getDevices(): Promise<Device[]>;
  getProperty(did: string, siid: number, piid: number): Promise<MiotPropValue>;
  /** 批量读（一次 rpcv2；单项失败不影响其它项） */
  getProperties(did: string, refs: JsPropRef[]): Promise<MiotPropValue[]>;
  /** 卡片模板归一化 DTO（方案 A：Rust 全消费，ArkTS 零解析只渲染） */
  /** `layout` 省略 = 该设备没自定义过，返回模板原样（默认行为零变化） */
  /** `mac` 传入 BLE 设备 MAC 时，Rust 侧做本地优先合并（蓝牙读数比云端新鲜则覆盖） */
  cardItems(did: string, specType: string,
    layout?: MiotCardLayout, mac?: string): Promise<MiotCardItem[]>;
  /** 首页编辑器数据源：全量候选项 + 当前是否显示（key 由 Rust 生成，本层不拼） */
  cardLayoutOptions(did: string, specType: string,
    layout?: MiotCardLayout): Promise<MiotLayoutOption[]>;
  /** 导入档案后的编辑器预览：勾选状态与拒绝原因均由 Rust 按当前固件判定 */
  cardLayoutPreview(did: string, specType: string,
    profile: string): Promise<MiotLayoutPreview>;
  /** 生成可共享档案（含 spec 语义指纹）。返回不透明文本，本层不解析。 */
  exportCardProfile(did: string, specType: string, model: string,
    visible: string[], known: string[]): Promise<string>;
  /** 导入前预检：返回每项被拒绝的原因；空数组 = 这份档案在当前固件上完全可用 */
  cardProfileWarnings(specType: string, profile: string): Promise<string[]>;
  /** 二级卡片全量 DTO（模板全量 + spec 可写 + 动作；含 range/paramDelta/isAction）。
   * realtime=true 走 rpcv2 穿透实时读（下发后确认），false 走云端缓存读。
   * `mac` 传入 BLE 设备 MAC 时做本地优先合并（见 cardItems）。 */
  cardDetail(did: string, specType: string, realtime: boolean,
    mac?: string): Promise<MiotCardItem[]>;
  /** 一键圆钮放行判定：判定本体在 Rust（safety.rs），本层只是通道。 */
  quickSwitch(model: string, specType: string, toggleSiid: number, togglePiid: number,
    label: string): Promise<MiotSwitchDecision>;
  /** 执行 spec 动作（isAction=true 的二级项；无参动作） */
  callAction(did: string, siid: number, iid: number): Promise<void>;
  /** 文案是否为未翻译的协议 token；规则唯一实现在 Rust `safety.rs` */
  textIsSpecToken(text: string): boolean;
  /** 档位 A：解码一条 MiBeacon 广播（adv 原始字节 base64）；失败返回 null */
  /** knownMac：设备真实 mac（12hex 或带冒号）；RPA+包内无 mac 的省电帧组 nonce 用，可传空串 */
  decodeBleAdv(token: string, advBase64: string, knownMac: string): string | null;
  /** 档位 A：不解密，只提取 MiBeacon 包内真实 MAC（RPA 广播地址场景二次查 token） */
  peekBleMac(advBase64: string): string | null;
  /** 档位 A：blt.* 设备 MiBeacon 广播 bindkey（32 hex）；非 BLE 设备 null */
  getBeaconkey(did: string): Promise<string | null>;
  /** 本地优先：解码成功的 MiBeacon JSON 落库（按包内 MAC 归档快照，读值时合并） */
  bleIngest(beaconJson: string): boolean;
  /** 档位 A 常驻扫描：多目标试解 RPA 无包内 mac 的 V4/V5 加密帧；返回命中 MiBeacon JSON */
  bleDecodeMulti(targetsJson: string, advBase64: string): string | null;
  /** 档位 B：ebe0ccc1(+可选 ccc4) 免登录读数解码；非法输入返回 null */
  bleParseHt(ccc1Hex: string, ccc4Hex?: string): JsHtReading | null;
  setProperty(did: string, siid: number, piid: number,
    value: boolean | number | string | object): Promise<void>;
  callMethod(did: string, method: string, params: object): Promise<object>;
  onDeviceChanged(cb: (e: DeviceChangedEvent) => void, intervalMs?: number): Unsubscribe;
}

class MiotSdkImpl implements MiotSdk {
  /** 在 EntryAbility onCreate 调用；session.json 与 Python 原型格式互通 */
  init(context: common.Context): void {
    nativeInit(`${context.filesDir}/session.json`);
  }

  isLoggedIn(): boolean {
    return nativeIsLoggedIn();
  }

  /**
   * 导入 PC 端 session.json 文本：写入 filesDir 后重载会话。
   * 免 hdc 推文件的真机导入通道（应用沙盒外部不可直写）。
   */
  importSession(sessionJsonText: string, filesDir: string): boolean {
    const path = `${filesDir}/session.json`;
    const file = fs.openSync(
      path,
      fs.OpenMode.READ_WRITE | fs.OpenMode.CREATE | fs.OpenMode.TRUNC
    );
    try {
      fs.writeSync(file.fd, sessionJsonText);
    } finally {
      fs.closeSync(file);
    }
    try {
      nativeReloadSession();
      return true;
    } catch (e) {
      return false;
    }
  }

  /** 退出会话（调试用）：清空内存会话并删除 session.json */
  logout(): void {
    nativeLogout();
    pendingConfirmUrl = '';
  }

  /** 密码登录。'ok' 直连成功；NeedConfirmResult 时调 confirmOnPhone() */
  async login(user: string, pass: string): Promise<LoginResult> {
    const r: JsLoginResult = await nativeLoginPassword(user, pass);
    if (r.ok) {
      return 'ok';
    }
    pendingConfirmUrl = r.needConfirm ?? '';
    const ret: NeedConfirmResult = { needConfirm: pendingConfirmUrl };
    return ret;
  }

  /** 当前待确认 URL（ConfirmPage 用） */
  pendingConfirmUrl(): string {
    return pendingConfirmUrl;
  }

  /**
   * securityStatus=16 收尾：Web 组件确认后从 cookie 提取 passToken 换会话。
   * fetchCookieSync 只返回匹配该 URL 的 cookie，且无 cookie 时抛 17100002；
   * 而原型实测 passToken 可能落在通知页以外的 .xiaomi.com 域（serviceToken
   * 甚至落 io.mi.com / mi.com），故按多域名探测 + 无 cookie 静默。
   */
  async tryFinishConfirmFromWeb(url: string): Promise<boolean> {
    const probes: Array<string> =
      [url, 'https://account.xiaomi.com/', 'https://io.mi.com/', 'https://www.mi.com/'];
    return this.harvestAndFinish(probes);
  }

  /** 小米登录页入口 URL */
  webLoginUrl(): string {
    return WEB_LOGIN_ENTRY;
  }

  /** 小米扫码登录页入口 URL */
  webLoginQrUrl(): string {
    return WEB_LOGIN_QR;
  }

  /** 小米网页登出 URL */
  webLogoutUrl(): string {
    return WEB_LOGOUT;
  }

  /**
   * 清 ArkWeb cookie 罐（非废弃的 Promise 版；callback 版自 API 11 废弃）。
   * 为什么必须由 UI 触发：小米 H5 认 cookie，passToken 还在时登录页会跳过表单
   * 直接 302 回调换会话 —— 不清就换不了账号（也验证不到真实登录流程）。
   */
  async clearWebCookies(): Promise<void> {
    await webview.WebCookieManager.clearAllCookies();
    hilog.info(0x0000, 'miot', 'web cookies cleared by user action');
  }

  /**
   * 换账号必须走到这一层：真机实测只 clearAllCookies 不够 ——
   * 小米 H5 会把登录态放进 origin storage，裸扫码页在清 cookie 后 5s 内
   * 仍静默拿到新的 passToken（等于没退出）。WebStorage.deleteAllData()
   * 把本应用 WebView 的站点数据整体清掉，配合官方 /pass/logout 才算真退出。
   */
  async clearWebLoginState(): Promise<void> {
    try {
      webview.WebStorage.deleteAllData();
      hilog.info(0x0000, 'miot', 'web site data deleted');
    } catch (e) {
      hilog.warn(0x0000, 'miot', 'WebStorage.deleteAllData failed: %{public}s', String(e));
    }
    await webview.WebCookieManager.clearAllCookies();
    hilog.info(0x0000, 'miot', 'web login state fully cleared');
  }
  /**
   * 网页登录收尾：用户在 Web 组件里完成小米登录（密码/短信/扫码任选，
   * 凭据只进小米自己的 H5），我们从 WebView cookie 罐取 passToken+userId，
   * 走与通知确认同一条 serviceLogin 重放通道换会话。
   */
  async loginWithWebCookies(): Promise<boolean> {
    return this.harvestAndFinish(WEB_LOGIN_PROBES);
  }

  /** 多域探测 passToken → finishNotification 换会话；无 passToken 返回 false */
  private async harvestAndFinish(probes: Array<string>): Promise<boolean> {
    let passToken: string | null = null;
    let userId: string = '';
    for (const probe of probes) {
      let cookie: string;
      try {
        cookie = webview.WebCookieManager.fetchCookieSync(probe);
      } catch (e) {
        continue; // 17100002：该 URL 下暂无 cookie
      }
      const pass = /(?:^|;\s*)passToken=([^;]+)/.exec(cookie);
      if (pass !== null) {
        passToken = pass[1];
        const uid = /(?:^|;\s*)userId=([^;]+)/.exec(cookie);
        if (uid !== null) {
          userId = uid[1];
        }
        // 只记长度，不打 token 明文；userId 为数字 id 便于对账
        hilog.info(0x0000, 'miot', 'passToken captured from %{public}s (len=%d, userId=%{public}s)',
          probe, passToken.length, userId === '' ? '<none>' : userId);
        break;
      }
    }
    if (passToken === null) {
      hilog.info(0x0000, 'miot', 'finish probe: no passToken in cookie store yet');
      return false;
    }
    // serviceLogin 重放必须同时带 passToken + userId（原型 _pass_token_login 实测口径），
    // 缺 userId 会 70016 登录验证失败
    await nativeFinishNotification(passToken, userId);
    pendingConfirmUrl = '';
    return true;
  }

  async getDevices(): Promise<Device[]> {
    const list: Array<JsDevice> = await nativeGetDevices();
    return list.map((d: JsDevice): Device => toDevice(d));
  }

  async getProperty(did: string, siid: number, piid: number): Promise<MiotPropValue> {
    const v: JsPropValue = await nativeGetProperty(did, siid, piid);
    const prop: MiotPropValue = {
      did: v.did,
      siid: v.siid,
      piid: v.piid,
      code: v.code,
      value: JSON.parse(v.value) as object | boolean | number | string | null
    };
    return prop;
  }

  async getProperties(did: string, refs: JsPropRef[]): Promise<MiotPropValue[]> {
    const vals: Array<JsPropValue> = await nativeGetProperties(did, refs);
    return vals.map((v: JsPropValue): MiotPropValue => {
      const prop: MiotPropValue = {
        did: v.did,
        siid: v.siid,
        piid: v.piid,
        code: v.code,
        value: JSON.parse(v.value) as object | boolean | number | string | null
      };
      if (v.updateTime !== undefined && v.updateTime !== null) {
        prop.updateTime = v.updateTime;
      }
      return prop;
    });
  }

  /**
   * 卡片 DTO：native 返回归一化 JsCardItem（value/choices 为 JSON 文本），
   * 这里仅做文本→领域对象转换，不做任何模板匹配/条件求值（都在 Rust 完成）。
   */
  async cardItems(did: string, specType: string,
    layout?: MiotCardLayout, mac?: string): Promise<MiotCardItem[]> {
    // 有档案优先走档案通道（Rust 侧按 spec 指纹逐项校验，不匹配项一律剔除）；
    // 没档案才退回裸名单。实现签名少写参数时 ArkTS 不报错但会静默丢参数，
    // 所以这里把入参都显式传满（mac 走本地优先合并，非 BLE 设备传 undefined）。
    const list: Array<JsCardItem> = await nativeCardItems(did, specType,
      layout === undefined ? undefined : layout.visible,
      layout === undefined ? undefined : layout.known,
      layout === undefined || layout.profile === undefined ? undefined : layout.profile,
      mac === undefined ? undefined : mac);
    return list.map(mapJsCardItem);
  }

  async exportCardProfile(did: string, specType: string, model: string,
    visible: string[], known: string[]): Promise<string> {
    return nativeExportCardProfile(did, specType, model, visible, known);
  }

  async cardProfileWarnings(specType: string, profile: string): Promise<string[]> {
    return nativeCardProfileWarnings(specType, profile);
  }

  /** JsLayoutOption → 领域对象。编辑器两条数据源（名单版 / 档案版）共用一份映射 */
  private mapOption(o: JsLayoutOption): MiotLayoutOption {
    const item: MiotLayoutOption = {
      key: o.key,
      label: o.label,
      kind: o.kind,
      checked: o.checked
    };
    return item;
  }

  /** 首页编辑器候选项（全量、不过滤） */
  async cardLayoutOptions(did: string, specType: string,
    layout?: MiotCardLayout): Promise<MiotLayoutOption[]> {
    const list: Array<JsLayoutOption> = await nativeCardLayoutOptions(did, specType,
      layout === undefined ? undefined : layout.visible,
      layout === undefined ? undefined : layout.known);
    return list.map((o: JsLayoutOption): MiotLayoutOption => this.mapOption(o));
  }

  /**
   * 导入档案后的编辑器预览：勾选状态与「为什么这项被拒」都由 Rust 给出。
   * 本层刻意不解析档案 JSON——否则"这项还算不算同一属性"的判定会分裂成两处实现。
   */
  async cardLayoutPreview(did: string, specType: string,
    profile: string): Promise<MiotLayoutPreview> {
    const j: JsLayoutPreview = await nativeCardLayoutPreview(did, specType, profile);
    const p: MiotLayoutPreview = {
      options: j.options.map((o: JsLayoutOption): MiotLayoutOption => this.mapOption(o)),
      warnings: j.warnings
    };
    return p;
  }

  /** 二级弹层全量 DTO（Rust card_detail：模板全量 + spec 可写 + 动作） */
  async cardDetail(did: string, specType: string, realtime: boolean,
    mac?: string): Promise<MiotCardItem[]> {
    const list: Array<JsCardItem> = await nativeCardDetail(did, specType, realtime,
      mac === undefined ? undefined : mac);
    return list.map(mapJsCardItem);
  }

  /**
   * 一键圆钮放行判定。spec 由 Rust 侧从磁盘缓存取（与 cardItems 同一份），
   * 缓存缺失且拉不到时 Rust 用空 spec 继续判定 → 结果退回 label 兜底。
   */
  async quickSwitch(model: string, specType: string, toggleSiid: number, togglePiid: number,
    label: string): Promise<MiotSwitchDecision> {
    const j: JsSwitchDecision = await nativeQuickSwitch(model, specType, toggleSiid, togglePiid,
      label);
    const d: MiotSwitchDecision = {
      allowed: j.allowed,
      code: j.code,
      hint: j.hint
    };
    if (j.onSiid !== undefined && j.onSiid !== null) {
      d.onSiid = j.onSiid;
      d.onPiid = j.onPiid;
    }
    return d;
  }

  /** 执行 spec 动作（无参动作；out 值本期不消费） */
  async callAction(did: string, siid: number, iid: number): Promise<void> {
    await nativeCallAction(did, siid, iid);
  }

  /**
   * 同步判定。ArkTS 侧原先自带一份正则，与 Rust 版本并存迟早漂移，
   * 本轮起只保留 Rust 那一份（`safety::looks_like_spec_token`）。
   */
  textIsSpecToken(text: string): boolean {
    return nativeTextIsSpecToken(text);
  }

  /** MiBeacon 广播解码（解密+解析全在 Rust，这里纯转发） */
  decodeBleAdv(token: string, advBase64: string, knownMac: string): string | null {
    return nativeBleDecode(token, advBase64, knownMac);
  }

  peekBleMac(advBase64: string): string | null {
    return nativeBlePeekMac(advBase64);
  }

  async getBeaconkey(did: string): Promise<string | null> {
    return await nativeBleBeaconkey(did);
  }

  /** MiBeacon JSON 落库（同步；返回 false = 解析失败，非 MiBeacon JSON） */
  bleIngest(beaconJson: string): boolean {
    return nativeBleIngest(beaconJson);
  }

  /** 多目标试解（targetsJson = [["12hex mac","key"],...]） */
  bleDecodeMulti(targetsJson: string, advBase64: string): string | null {
    return nativeBleDecodeMulti(targetsJson, advBase64);
  }

  /** ebe0ccc1 = [temp÷100][humi][电压÷1000] + ccc4 电量（解码全在 Rust） */
  bleParseHt(ccc1Hex: string, ccc4Hex?: string): JsHtReading | null {
    return nativeBleParseHt(ccc1Hex, ccc4Hex);
  }

  async setProperty(
    did: string, siid: number, piid: number,
    value: boolean | number | string | object
  ): Promise<void> {
    await nativeSetProperty(did, siid, piid, JSON.stringify(value));
  }

  /** 老 miIO 设备（非 spec） */
  async callMethod(did: string, method: string, params: object): Promise<object> {
    const raw: string = await nativeCallMethod(did, method, JSON.stringify(params));
    return JSON.parse(raw) as object;
  }

  /**
   * v0.2 接口先立：轮询设备列表 diff，online/name 变化触发 cb。
   * 属性级事件待云订阅通道（二期）。
   */
  onDeviceChanged(cb: (e: DeviceChangedEvent) => void, intervalMs: number = 5000): Unsubscribe {
    if (pollTimer >= 0) {
      return (): void => undefined; // 单订阅：已有轮询在跑
    }
    const snapshot = new Map<string, Device>();
    pollTimer = setInterval(() => {
      miot.getDevices()
        .then((devices: Device[]): void => {
          const seen = new Set<string>();
          for (const d of devices) {
            seen.add(d.did);
            const prev: Device | undefined = snapshot.get(d.did);
            if (prev !== undefined && (prev.online !== d.online || prev.name !== d.name)) {
              const e: DeviceChangedEvent = { did: d.did };
              if (prev.online !== d.online) {
                e.online = d.online;
              }
              if (prev.name !== d.name) {
                e.name = d.name;
              }
              cb(e);
            }
            snapshot.set(d.did, d);
          }
          const removed: Array<string> = [];
          snapshot.forEach((v: Device, did: string): void => {
            if (!seen.has(did)) {
              removed.push(did);
            }
          });
          for (const did of removed) {
            snapshot.delete(did);
          }
        })
        .catch((): void => undefined); // 网络抖动静默，下轮重试
    }, intervalMs);
    return (): void => {
      clearInterval(pollTimer);
      pollTimer = -1;
    };
  }
}

export const miot: MiotSdk = new MiotSdkImpl();
