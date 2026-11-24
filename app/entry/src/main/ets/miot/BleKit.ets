/**
 * XiaomiBle —— 档位 A：本地 BLE 广播收数（只读，MiBeacon fe95）。
 *
 * 职责：BLE 扫描 → 原始 advertisingData 交 Rust `ble_decode`（解密+解析）→
 * 命中已配 token 的小米传感器时回调读数。所有解析/解密逻辑在 mijia-core::ble，
 * 本文件只是鸿蒙 ConnectivityKit 胶水。
 */
import { ble, access } from '@kit.ConnectivityKit';
import { util } from '@kit.ArkTS';
import { hilog } from '@kit.PerformanceAnalysisKit';
import { miot } from './native';

const DOMAIN = 0x0000;
const TAG = 'miot-ble';

export interface BleValues {
  temperature?: number;
  humidity?: number;
  battery?: number;
  door_closed?: boolean;
}

export interface BleAdv {
  mac: string;
  device_id: number;
  version: number;
  packet_id: number;
  encrypted: boolean;
  values: BleValues;
  unknown?: string[];
}

export interface BleReading {
  adv: BleAdv;
  rssi: number;
  ts: number;
  rawHex: string;
}

/** mac 归一：去分隔符、大写 */
export function normMac(mac: string): string {
  return mac.replace(/:/g, '').toUpperCase();
}

type ReadingCb = (r: BleReading) => void;

class XiaomiBleImpl {
  private targets: Map<string, string> = new Map(); // normMac -> token(b64 或 hex)
  private soloMac: string = ''; // 单目标（弹层）便捷路径：identity mac 与 key
  private soloKey: string = '';
  private lastPkt: Map<string, number> = new Map(); // mac -> packet_id 去重
  private logged: Map<string, number> = new Map(); // mac -> 诊断日志上次时间（节流）
  private cb: ReadingCb | null = null;
  private scanning: boolean = false;
  private findBatches: number = 0; // 校准期心跳计数

  /** 目标设备（mac+token）；空 token 的明文设备也可收（token 传空串） */
  setTargets(devs: Array<string[]>): void {
    this.targets.clear();
    this.soloMac = '';
    this.soloKey = '';
    for (const d of devs) {
      if (d.length >= 1 && d[0] !== '') {
        this.targets.set(normMac(d[0]), d.length >= 2 ? d[1] : '');
      }
    }
    if (this.targets.size === 1) {
      this.targets.forEach((v: string, k: string): void => {
        this.soloMac = k;
        this.soloKey = v;
      });
    }
  }

  isOn(): boolean {
    try {
      return access.getState() === access.BluetoothState.STATE_ON;
    } catch (e) {
      return false;
    }
  }

  start(cb: ReadingCb): boolean {
    this.stop();
    if (!this.isOn()) {
      hilog.warn(DOMAIN, TAG, 'bluetooth not on, local ble ingest disabled');
      return false;
    }
    this.cb = cb;
    this.findBatches = 0;
    try {
      ble.on('BLEDeviceFind', this.onFind);
      // 不设 ScanFilter：米家 fe95 设备地址可能与列表 mac 不一致，按包内 mac 匹配
      // 文档口径：不用 ScanFilter 时该参数传 null（空数组会触发 401 Invalid parameter）；
      // options 省略即用默认（SCAN_MODE_LOW_POWER / MATCH_MODE_AGGRESSIVE）
      ble.startBLEScan(null);
      this.scanning = true;
      hilog.info(DOMAIN, TAG, 'scan started, targets=%{public}d', this.targets.size);
      return true;
    } catch (e) {
      hilog.error(DOMAIN, TAG, 'startBLEScan failed: %{public}s', String(e));
      return false;
    }
  }

  stop(): void {
    if (!this.scanning) {
      return;
    }
    try {
      ble.off('BLEDeviceFind', this.onFind);
      ble.stopBLEScan();
    } catch (e) {
      hilog.warn(DOMAIN, TAG, 'stopBLEScan: %{public}s', String(e));
    }
    this.scanning = false;
    this.cb = null;
  }

  private onFind = (results: Array<ble.ScanResult>): void => {
    this.findBatches++;
    if (this.findBatches <= 3 || this.findBatches % 40 === 0) {
      // 心跳诊断：确认回调链路活着（校准期用）
      const first = results.length > 0 ? results[0] : null;
      let keys = '';
      if (first !== null && first.advertisingDataMap !== undefined && first.advertisingDataMap !== null) {
        first.advertisingDataMap.forEach((v: Uint8Array, type: number): void => {
          keys += type.toString(16) + ':' + v.length + ' ';
        });
      }
      hilog.info(DOMAIN, TAG, 'find batch#%{public}d n=%{public}d dev0=%{public}s map=[%{public}s]',
        this.findBatches, results.length, first !== null ? first.deviceId : '-', keys);
    }
    for (const r of results) {
      this.onOne(r);
    }
  };

  private onOne(r: ble.ScanResult): void {
    // API 26：ScanResult 不再提供 advertisingData，用 advertisingDataMap 重建 AD 序列
    const map: Map<number, Uint8Array> | undefined = r.advertisingDataMap;
    const devMac0 = normMac(r.deviceId);
    const ad: Uint8Array = this.rebuildAd(map);
    if (ad.length === 0) {
      return;
    }
    // 校准探针（临时）：任何设备广播含 95 fe 都打一条（10s 节流），
    // 证明鸿蒙侧 fe95 的 map 键形态与扫描可达性，与 token/目标无关
    if (this.contains2(ad, 0x95, 0xFE) && this.allowLog('any95-' + normMac(r.deviceId))) {
      hilog.info(DOMAIN, TAG, 'fe95-sight dev=%{public}s raw=%{public}s',
        normMac(r.deviceId), this.hex(ad));
    }
    const devMac = devMac0;
    // token：先按广播地址查，未命中留待包内 mac 二查
    const token: string = this.targets.get(devMac) ?? '';
    // 严格预检 [0x16][0x95][0xFE]；同时做宽松预检（任意位置 95 FE）供真机校准
    const hasFe95 = this.contains(ad, 0x16, 0x95, 0xFE);
    if (!hasFe95 && this.contains2(ad, 0x95, 0xFE)) {
      // 小米 service data 在，但 AD type 键不是标准 0x16：打全量 map 键表+raw 校准
      let keys = '';
      if (map !== undefined && map !== null) {
        map.forEach((v: Uint8Array, type: number): void => {
          keys += type.toString(16) + ':' + v.length + ' ';
        });
      }
      if (this.allowLog(devMac)) {
        hilog.info(DOMAIN, TAG, 'fe95-loose dev=%{public}s keys=[%{public}s] raw=%{public}s',
          devMac, keys, this.hex(ad));
      }
      return;
    }
    if (!hasFe95) {
      return;
    }
    let b64: string;
    try {
      b64 = new util.Base64Helper().encodeToStringSync(ad);
    } catch (e) {
      return;
    }
    // knownMac：deviceId 命中时即 identity mac；否则先空串（包内带 mac 的帧自带）
    let out: string | null = miot.decodeBleAdv(token, b64, devMac);
    let usedKey: string = token;
    if (out === null && token === '') {
      // RPA 广播地址：deviceId 查不到 token，peek 包内真实 mac 再试
      const pk: string | null = miot.peekBleMac(b64);
      const t2: string = pk !== null ? (this.targets.get(normMac(pk)) ?? '') : '';
      if (t2 !== '') {
        out = miot.decodeBleAdv(t2, b64, pk !== null ? normMac(pk) : '');
        usedKey = t2;
      } else if (this.soloKey !== '') {
        // RPA 源地址 + 包内无 mac（温湿度计省电帧）：弹层单目标场景试唯一 key，
        // 加密帧 V5 需 MIC 通过才出得来 values → 通过即证明是本机
        out = miot.decodeBleAdv(this.soloKey, b64, this.soloMac);
        usedKey = this.soloKey;
        if (out !== null && this.allowLog('solo-' + this.soloMac)) {
          hilog.info(DOMAIN, TAG, 'solo hit dev=%{public}s raw=%{public}s',
            this.soloMac, this.hex(ad));
        }
      }
    }
    if (out === null && this.targets.size > 1 && this.targets.size <= 16) {
      // 常驻扫描多目标：RPA 且包内无 mac 的 V5 加密帧无法按地址查 key
      //（solo 路径只覆盖单目标），逐目标试解——V5 解密带 MIC 校验，
      // key 错必败，不会把邻居的帧误归档；legacy 无 MIC 不参与（Rust 侧过滤）。
      // try 包住：native 导出缺失/异常时不让单帧拖死整个扫描批次。
      try {
        const list: Array<string[]> = [];
        this.targets.forEach((v: string, k: string): void => {
          list.push([k, v]);
        });
        const mo: string | null = miot.bleDecodeMulti(JSON.stringify(list), b64);
        if (mo !== null) {
          out = mo;
          usedKey = '';
          if (this.allowLog('multi-' + mo.substring(0, 16))) {
            hilog.info(DOMAIN, TAG, 'multi hit len=%{public}d raw=%{public}s',
              this.targets.size, this.hex(ad));
          }
        }
      } catch (e) {
        if (this.allowLog('multi-err')) {
          hilog.warn(DOMAIN, TAG, 'bleDecodeMulti failed: %{public}s', String(e));
        }
      }
    }
    if (out === null) {
      if (token !== '' && this.allowLog(devMac)) {
        hilog.info(DOMAIN, TAG, 'decode fail dev=%{public}s raw=%{public}s', devMac, this.hex(ad));
      }
      return;
    }
    const adv: BleAdv = JSON.parse(out) as BleAdv;
    const key = adv.mac === '' ? devMac : normMac(adv.mac);
    if (!this.targets.has(key) && !this.targets.has(devMac) && usedKey === '') {
      return; // 不在目标清单，也没命中过任何 key
    }
    // 只回调传感器读数帧（有 values）；重复 packet_id 去重
    const v = adv.values;
    if (v === undefined || (v.temperature === undefined && v.humidity === undefined &&
      v.battery === undefined && v.door_closed === undefined)) {
      return;
    }
    if (this.lastPkt.get(key) === adv.packet_id && adv.encrypted) {
      return;
    }
    this.lastPkt.set(key, adv.packet_id);
    const rd: BleReading = { adv: adv, rssi: r.rssi, ts: Date.now(), rawHex: this.hex(ad) };
    hilog.info(DOMAIN, TAG, 'ble reading mac=%{public}s v=%{public}d raw=%{public}s',
      key, adv.version, this.hex(ad));
    if (this.cb !== null) {
      this.cb(rd);
    }
  }

  private hex(ad: Uint8Array): string {
    let s = '';
    for (let i = 0; i < ad.length; i++) {
      s += ad[i].toString(16).padStart(2, '0');
    }
    return s;
  }

  /** ad 中是否含三连字节 a,b,c（任意位置，含跨 type/data 边界——重建错位时也能命中） */
  private contains(ad: Uint8Array, a: number, b: number, c: number): boolean {
    for (let i = 0; i + 3 <= ad.length; i++) {
      if (ad[i] === a && ad[i + 1] === b && ad[i + 2] === c) {
        return true;
      }
    }
    return false;
  }

  private contains2(ad: Uint8Array, a: number, b: number): boolean {
    for (let i = 0; i + 2 <= ad.length; i++) {
      if (ad[i] === a && ad[i + 1] === b) {
        return true;
      }
    }
    return false;
  }

  /** 诊断日志节流：每设备 10s 最多一条（校准期用，防邻居刷屏） */
  private allowLog(devMac: string): boolean {
    const now = Date.now();
    const last = this.logged.get(devMac) ?? 0;
    if (now - last < 10000) {
      return false;
    }
    this.logged.set(devMac, now);
    return true;
  }

  /** advertisingDataMap → 原始 AD 字节序列（[len][type][data...]，len 含 type） */
  private rebuildAd(map: Map<number, Uint8Array> | undefined): Uint8Array {
    const out: number[] = [];
    if (map === undefined || map === null) {
      return new Uint8Array(0);
    }
    map.forEach((v: Uint8Array, type: number): void => {
      out.push(v.length + 1, type);
      for (let i = 0; i < v.length; i++) {
        out.push(v[i]);
      }
    });
    return new Uint8Array(out);
  }
}

export const XiaomiBle: XiaomiBleImpl = new XiaomiBleImpl();
