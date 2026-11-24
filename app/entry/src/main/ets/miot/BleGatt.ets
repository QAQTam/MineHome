/**
 * BleGatt —— 档位 B：GATT 直连「弹层保活会话」（免登录路径）。
 *
 * 交互形态 A（2026-11-24 用户拍板）：弹层打开即连接并**保持**，订阅 ebe0ccc1
 * notify 做"变化秒推"；离开弹层（或超过 SESSION_HOLD_MS 兜底）才断开。
 * 为什么保活而不是读完即断：
 * 1. 设备常态广播是 ADV_NONCONN_IND 不可连接（固件分析 §4.5），一次断开后
 *    很可能再也进不去（要用户再长按）——连接本身是稀缺资源；
 * 2. 真机实测：连着的时候 ebe0ccc1 会**免登录推 notify**（2026-11-24 CCCD 写
 *    成功后 2.3s 收到推送 7409→6a09），变化即上屏，比反复建链更省电。
 * 代价（已确认可接受）：会话期间设备只吃一条 GATT，米家 App 连不上。
 *
 * GATT 操作必须**串行**：并发 read + 订阅会被系统拒 `2900011 operation busy`
 * （真机实测），所以 open 的建链后动作排成 read ccc1 → read ccc4 → 订阅 notify。
 *
 * 型号门控：只有 06 这类带秒秒测 ebe0ccc1 的设备才值得连（其它型号连上枚举后
 * 报 "ebe0ccc1 not in GATT table"，白耗 3s 还刷屏）→ supports() 白名单 +
 * 负样本缓存（同 mac 10min 内不再尝试）。
 *
 * 编码定案（§4.4）：ebe0ccc1 = [temp int16 LE ÷100][humi u8][电压 int16 LE ÷1000]，
 * ebe0ccc4 = 电量 u8%。解码在 Rust（ble_auth::parse_ebe0ccc1 / napi bleParseHt）。
 * 登录路径（mible + spec properties_changed 0x001A）待 V3.3/V3.4 钉死后再接。
 */
import { ble, access } from '@kit.ConnectivityKit';
import { BusinessError } from '@kit.BasicServicesKit';
import { hilog } from '@kit.PerformanceAnalysisKit';
import { JsHtReading } from 'libmijia_napi.so';
import { miot } from './native';

const DOMAIN = 0x0000;
const TAG = 'miot-ble-gatt';

const CCC1 = 'ebe0ccc1-7a0a-4b0c-8a1a-6ff2997da3a6';
const CCC4 = 'ebe0ccc4-7a0a-4b0c-8a1a-6ff2997da3a6';
/** constant.ProfileConnectionState.STATE_CONNECTED / STATE_DISCONNECTED */
const CONN_CONNECTED = 2;
const CONN_DISCONNECTED = 0;
/** BusinessError 2900003 = 蓝牙未开启 */
const ERR_BT_OFF = 2900003;
/** 连接等待上限（ms）：设备在可连接窗口时秒级连上；超时按不可连处理 */
const CONNECT_TIMEOUT_MS = 10000;
/** 弹层保活上限（ms）：忘记关弹层也到此必断（耗电 + 独占会挡米家） */
const SESSION_HOLD_MS = 5 * 60 * 1000;
/** notify 不可用时的轮询间隔（ms） */
const POLL_MS = 10000;
/** 负样本冷却（ms）：这台 mac 枚举后没有 ebe0ccc1，一段时间内不再尝试 */
const INCAP_TTL_MS = 10 * 60 * 1000;
/** 广播 devid（0x55b5）：合成落库 JSON 用，仅做归档标识 */
const MJ6_DEVID = 21941;

/** 支持免登录 ebe0ccc1 直读/推送的型号（06 实机 dump §4.1；新型号实测后加） */
const HT_MODELS: string[] = ['xiaomi.sensor_ht.mini'];

export interface GattValues {
  temperature: number;
  humidity: number;
  /** 电池电压 V */
  voltage_v: number;
  battery?: number;
}

export type GattFail = 'bt_off' | 'not_connectable' | 'gatt_error' | 'unsupported';

export interface GattResult {
  ok: boolean;
  fail?: GattFail;
  detail?: string;
  values?: GattValues;
  /** true = ccc1 notify 已订阅（变化即推）；false = 退化为 POLL_MS 轮询 */
  live: boolean;
}

/** 会话推送回调（每条新读数） */
export type HtPush = (v: GattValues) => void;
/** 会话终止回调（掉线 / 保活到期），reason 供横幅文案 */
export type HtClosed = (reason: string) => void;

/** 合成 MiBeacon JSON（ble_cache::ingest 的输入形态；字段与 Rust MiBeacon 对齐） */
interface BeaconValues {
  temperature: number;
  humidity: number;
  battery?: number;
}

interface BeaconJson {
  mac: string;
  device_id: number;
  version: number;
  packet_id: number;
  encrypted: boolean;
  values: BeaconValues;
  unknown: string[];
}

/** mac（12hex 或带冒号）→ "AA:BB:…" 连接地址格式 */
function colonMac(mac: string): string {
  const m = mac.replace(/:/g, '').toUpperCase();
  if (m.length !== 12) {
    return mac;
  }
  let out = '';
  for (let i = 0; i < 12; i += 2) {
    out += m.substring(i, i + 2);
    if (i < 10) {
      out += ':';
    }
  }
  return out;
}

function bufHex(buf: ArrayBuffer): string {
  const u = new Uint8Array(buf);
  let s = '';
  for (let i = 0; i < u.length; i++) {
    s += u[i].toString(16).padStart(2, '0');
  }
  return s;
}

function withTimeout(p: Promise<void>, ms: number, what: string): Promise<void> {
  return new Promise<void>((resolve: (v: void) => void, reject: (e: Error) => void) => {
    const t = setTimeout((): void => {
      reject(new Error(what));
    }, ms);
    p.then((v: void): void => {
      clearTimeout(t);
      resolve(v);
    }, (e: Error): void => {
      clearTimeout(t);
      reject(e);
    });
  });
}

class BleGattImpl {
  private gce: ble.GattClientDevice | null = null;
  private ccc1: ble.BLECharacteristic | null = null;
  private ccc4: ble.BLECharacteristic | null = null;
  private mac: string = '';
  private onPush: HtPush | null = null;
  private onClosed: HtClosed | null = null;
  private live: boolean = false;
  private seq: number = 0;
  private holdTimer: number = -1;
  private pollTimer: number = -1;
  /** 主动关闭中（抑制 DISCONNECTED 事件误报"设备断开"） */
  private closing: boolean = false;
  private lastVals: GattValues | undefined = undefined;
  /** ccc4 电量缓存（不常变，连上读一次就够） */
  private batHex: string | undefined = undefined;
  /** mac → 判定"没有 ebe0ccc1"的时间戳（负样本冷却） */
  private incapable: Map<string, number> = new Map();
  /** 同 mac 的建链请求去重：连点两次会并发开两条链（gce 要等 connect 回来才置位） */
  private pending: Promise<GattResult> | null = null;
  private pendingMac: string = '';
  /** 上一条原始 ccc1 帧（notify 与首读会重叠，同值不再上报） */
  private lastRaw: string = '';
  /** 会话代号：close/切设备后作废旧会话的在途回调（防迟到的 connect 覆盖新会话） */
  private gen: number = 0;

  /** 该型号是否值得走 GATT 直连（白名单 + 负样本冷却） */
  supports(model: string, mac: string): boolean {
    let hit = false;
    for (const m of HT_MODELS) {
      if (m === model) {
        hit = true;
      }
    }
    if (!hit) {
      return false;
    }
    const bad = this.incapable.get(mac.replace(/:/g, '').toLowerCase()) ?? 0;
    return Date.now() - bad >= INCAP_TTL_MS;
  }

  isOpen(): boolean {
    return this.gce !== null;
  }

  /**
   * 打开（或复用）保活会话：connect → 串行 read ccc1 → read ccc4 → 订阅 notify → 保持。
   * 同一 mac 已开会话：换回调 + 立即回一次最近读数，不重复建链。
   * 不同 mac：先关旧会话再开新的（设备只吃一条连接）。
   */
  async open(mac: string, push: HtPush, closed: HtClosed): Promise<GattResult> {
    const devMac = mac.replace(/:/g, '').toLowerCase();
    if (this.gce !== null && this.mac === devMac) {
      this.onPush = push;
      this.onClosed = closed;
      return { ok: true, values: this.lastVals, live: this.live };
    }
    // 同 mac 建链在途：复用这次连接，不再开第二条
    if (this.pending !== null && this.pendingMac === devMac) {
      this.onPush = push;
      this.onClosed = closed;
      return await this.pending;
    }
    if (this.gce !== null || this.pending !== null) {
      this.close();
    }
    const job = this.doOpen(devMac, mac, push, closed, ++this.gen);
    this.pending = job;
    this.pendingMac = devMac;
    try {
      return await job;
    } finally {
      if (this.pending === job) {
        this.pending = null;
        this.pendingMac = '';
      }
    }
  }

  private async doOpen(devMac: string, mac: string, push: HtPush, closed: HtClosed,
    myGen: number): Promise<GattResult> {
    try {
      if (access.getState() !== access.BluetoothState.STATE_ON) {
        return { ok: false, fail: 'bt_off', detail: 'bluetooth off', live: false };
      }
    } catch (e) {
      return { ok: false, fail: 'bt_off', detail: String(e), live: false };
    }
    this.mac = devMac;
    this.onPush = push;
    this.onClosed = closed;
    this.live = false;
    this.closing = false;
    this.lastVals = undefined;
    this.batHex = undefined;
    const addr = colonMac(mac);
    const t0 = Date.now();
    let resolveConn: (() => void) | null = null;
    let gce: ble.GattClientDevice | null = null;
    try {
      const connected = new Promise<void>((resolve: (v: void) => void) => {
        resolveConn = resolve;
      });
      gce = ble.createGattClientDevice(addr);
      // 连接事件常驻：连上 resolve 首帧；会话期间 DISCONNECTED = 失效通知
      const onState = (s: ble.BLEConnectionChangeState): void => {
        if (myGen !== this.gen) {
          return; // 旧会话的残留事件
        }
        if (s.state === CONN_CONNECTED && resolveConn !== null) {
          resolveConn();
          resolveConn = null;
        } else if (s.state === CONN_DISCONNECTED && !this.closing && this.gce !== null) {
          hilog.warn(DOMAIN, TAG, 'link dropped mac=%{public}s', this.mac);
          this.teardown('设备断开连接');
        }
      };
      gce.on('BLEConnectionStateChange', onState);
      gce.connect();
      await withTimeout(connected, CONNECT_TIMEOUT_MS, 'connect timeout');
      if (myGen !== this.gen) {
        // 等待期间用户已关弹层/切设备：这条链直接释放，不要接管会话
        hilog.info(DOMAIN, TAG, 'stale connect discarded mac=%{public}s', devMac);
        try {
          gce.disconnect();
        } catch (e) {
        }
        try {
          gce.close();
        } catch (e) {
        }
        return { ok: false, fail: 'gatt_error', detail: 'stale session', live: false };
      }
      this.gce = gce;
      const services: ble.GattService[] = await gce.getServices();
      for (const s of services) {
        for (const ch of s.characteristics) {
          const u = ch.characteristicUuid.toLowerCase();
          if (u === CCC1) {
            this.ccc1 = ch;
          } else if (u === CCC4) {
            this.ccc4 = ch;
          }
        }
      }
      if (this.ccc1 === null) {
        // 这台设备没有秒秒测温湿度特征（型号不符/固件不同）：记负样本，别再连
        this.incapable.set(devMac, Date.now());
        throw new Error('ebe0ccc1 not in GATT table');
      }
      // 特征变化 → 落库上报（notify 与轮询共用）
      const onChar = (c: ble.BLECharacteristic): void => {
        if (c.characteristicUuid.toLowerCase() === CCC1) {
          this.consume(bufHex(c.characteristicValue), true);
        }
      };
      gce.on('BLECharacteristicChange', onChar);
      // 串行：首读 → 电量 → 订阅（并发会被系统拒 2900011 busy）
      await this.setupChain(gce);
      this.holdTimer = setTimeout((): void => {
        hilog.info(DOMAIN, TAG, 'session hold expired mac=%{public}s', this.mac);
        this.teardown('保活到时');
      }, SESSION_HOLD_MS);
      hilog.info(DOMAIN, TAG, 'session open mac=%{public}s t=%{public}dms live=%{public}s',
        devMac, Date.now() - t0, String(this.live));
      return { ok: true, values: this.lastVals, live: this.live };
    } catch (e) {
      const txt = this.errText(e);
      if (gce !== null) {
        try {
          gce.disconnect();
        } catch (e2) {
        }
        try {
          gce.close();
        } catch (e2) {
        }
      }
      const unsupported = txt.indexOf('ebe0ccc1 not in GATT table') >= 0;
      this.reset();
      if (unsupported) {
        hilog.info(DOMAIN, TAG, 'session skip mac=%{public}s: 无 ebe0ccc1（负样本 10min）',
          devMac);
        return { ok: false, fail: 'unsupported', detail: txt, live: false };
      }
      hilog.warn(DOMAIN, TAG, 'session open fail mac=%{public}s detail=%{public}s', devMac, txt);
      const fail: GattFail = txt.indexOf('connect timeout') >= 0 ? 'not_connectable' : 'gatt_error';
      return { ok: false, fail: fail, detail: txt, live: false };
    }
  }

  /** 主动关会话（弹层关闭 / 切设备）；同时作废在途建链 */
  close(): void {
    this.gen++;
    if (this.gce === null) {
      if (this.pending !== null) {
        this.pending = null;
        this.pendingMac = '';
      }
      return;
    }
    hilog.info(DOMAIN, TAG, 'session close mac=%{public}s', this.mac);
    this.teardown('');
  }

  /** 建链后的串行动作：首读温湿度 → 读电量 → 订阅 notify */
  private async setupChain(gce: ble.GattClientDevice): Promise<void> {
    const c1 = this.ccc1;
    const c4 = this.ccc4;
    if (c1 === null) {
      return;
    }
    try {
      const r: ble.BLECharacteristic = await gce.readCharacteristicValue(c1);
      this.consume(bufHex(r.characteristicValue), false);
    } catch (e) {
      hilog.warn(DOMAIN, TAG, 'read ccc1 failed %{public}s', this.errText(e as BusinessError));
    }
    if (c4 !== null) {
      try {
        const r4: ble.BLECharacteristic = await gce.readCharacteristicValue(c4);
        this.batHex = bufHex(r4.characteristicValue);
      } catch (e) {
        hilog.warn(DOMAIN, TAG, 'read ccc4 failed %{public}s', this.errText(e as BusinessError));
      }
    }
    await this.armNotify(gce, c1);
  }

  /** 解码 → 落本地快照 → 上报 UI（电量用缓存，不额外占 GATT 队列） */
  private consume(hex1: string, pushed: boolean): void {
    if (hex1 === this.lastRaw) {
      // 首读与紧随其后的 notify 会重叠；同值重复不再落库/刷横幅
      return;
    }
    this.lastRaw = hex1;
    // 显式标注：未验证 napi 模块的返回类型在推断位置会被判 any（arkts-no-any-unknown）
    const ht: JsHtReading | null = miot.bleParseHt(hex1, this.batHex);
    if (ht === null) {
      hilog.warn(DOMAIN, TAG, 'ccc1 decode failed raw=%{public}s', hex1);
      return;
    }
    this.seq = (this.seq + 1) & 0xFF;
    const values: BeaconValues = { temperature: ht.temperature, humidity: ht.humidity };
    if (ht.battery !== undefined && ht.battery !== null) {
      values.battery = ht.battery;
    }
    const beacon: BeaconJson = {
      mac: this.mac,
      device_id: MJ6_DEVID,
      version: 5,
      packet_id: this.seq,
      encrypted: true,
      values: values,
      unknown: []
    };
    miot.bleIngest(JSON.stringify(beacon));
    const out: GattValues = {
      temperature: ht.temperature,
      humidity: ht.humidity,
      // napi-derive 把 Rust snake_case 字段转 camelCase
      voltage_v: ht.voltageV
    };
    if (ht.battery !== undefined && ht.battery !== null) {
      out.battery = ht.battery;
    }
    this.lastVals = out;
    hilog.info(DOMAIN, TAG, '%{public}s mac=%{public}s raw=%{public}s',
      pushed ? 'gatt push' : 'gatt read ok', this.mac, hex1);
    if (this.onPush !== null) {
      this.onPush(out);
    }
  }

  /** 错误对象取 code/message（直接 String(e) 只会得到 [object Object]） */
  private errText(e: object | undefined): string {
    if (e === undefined || e === null) {
      return 'unknown';
    }
    const be = e as BusinessError;
    if (be.code === undefined || be.code === null) {
      return String(e);
    }
    const msg = be.message === undefined || be.message === null || be.message === ''
      ? String(e) : be.message;
    return `${msg} (code=${String(be.code)})`;
  }

  /** 订阅 ccc1 notify；被拒则手写 CCCD(0x2902)=0x0001，仍不行才轮询 */
  private async armNotify(gce: ble.GattClientDevice, c1: ble.BLECharacteristic): Promise<void> {
    try {
      await gce.setCharacteristicChangeNotification(c1, true);
      this.live = true;
      hilog.info(DOMAIN, TAG, 'ccc1 notify armed mac=%{public}s', this.mac);
      return;
    } catch (e) {
      hilog.warn(DOMAIN, TAG, 'arm failed %{public}s → 手写 CCCD', this.errText(e as BusinessError));
    }
    await this.writeCccd(gce, c1);
  }

  /** setCharacteristicChangeNotification 被拒时的等价路径：直接写 CCCD 描述符 */
  private async writeCccd(gce: ble.GattClientDevice, c1: ble.BLECharacteristic): Promise<void> {
    let cccd: ble.BLEDescriptor | null = null;
    for (const d of c1.descriptors) {
      if (d.descriptorUuid.toLowerCase().indexOf('2902') >= 0) {
        cccd = d;
        break;
      }
    }
    if (cccd === null) {
      hilog.warn(DOMAIN, TAG, 'no CCCD (desc=%{public}d) → poll', c1.descriptors.length);
      this.startPoll();
      return;
    }
    const buf = new ArrayBuffer(2);
    const u = new Uint8Array(buf);
    u[0] = 0x01;
    u[1] = 0x00;
    cccd.descriptorValue = buf;
    try {
      await gce.writeDescriptorValue(cccd);
      this.live = true;
      hilog.info(DOMAIN, TAG, 'CCCD written mac=%{public}s', this.mac);
    } catch (e) {
      hilog.warn(DOMAIN, TAG, 'CCCD write failed %{public}s → poll', this.errText(e as BusinessError));
      this.startPoll();
    }
  }

  private startPoll(): void {
    if (this.pollTimer >= 0) {
      return;
    }
    this.pollTimer = setInterval((): void => {
      const gce = this.gce;
      const c1 = this.ccc1;
      if (gce === null || c1 === null) {
        return;
      }
      gce.readCharacteristicValue(c1).then((r: ble.BLECharacteristic): void => {
        this.consume(bufHex(r.characteristicValue), true);
      }, (e: Error): void => {
        hilog.warn(DOMAIN, TAG, 'poll read failed %{public}s', this.errText(e));
      });
    }, POLL_MS);
  }

  private teardown(reason: string): void {
    const cb = this.onClosed;
    this.reset();
    if (reason !== '' && cb !== null) {
      cb(reason);
    }
  }

  private reset(): void {
    this.closing = true;
    if (this.holdTimer >= 0) {
      clearTimeout(this.holdTimer);
      this.holdTimer = -1;
    }
    if (this.pollTimer >= 0) {
      clearInterval(this.pollTimer);
      this.pollTimer = -1;
    }
    const gce = this.gce;
    const c1 = this.ccc1;
    if (gce !== null) {
      if (c1 !== null && this.live) {
        try {
          gce.setCharacteristicChangeNotification(c1, false);
        } catch (e) {
        }
      }
      try {
        gce.off('BLECharacteristicChange');
      } catch (e) {
      }
      try {
        gce.off('BLEConnectionStateChange');
      } catch (e) {
      }
      try {
        gce.disconnect();
      } catch (e) {
      }
      try {
        gce.close();
      } catch (e) {
      }
    }
    this.gce = null;
    this.ccc1 = null;
    this.ccc4 = null;
    this.live = false;
    this.lastVals = undefined;
    this.lastRaw = '';
    this.batHex = undefined;
    this.onPush = null;
    this.onClosed = null;
    this.mac = '';
    this.closing = false;
  }
}

export const BleGatt: BleGattImpl = new BleGattImpl();
