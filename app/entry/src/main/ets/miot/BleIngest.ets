/**
 * BleIngest —— 全局常驻 BLE 收数服务（档位 A 升级：从"弹层随开随扫"到常驻）。
 *
 * 职责：
 * 1. 设备列表到位后登记 BLE 目标（有 mac 的设备）；blt.* 异步预取 beaconkey；
 * 2. beaconkey 持久化 `<filesDir>/ble_keys.json`——断网冷启动也能解加密广播；
 * 3. XiaomiBle 起一次全目标扫描，每条解码成功的读数 `miot.bleIngest` 落 Rust 快照
 *    （card_items/card_detail 读值时做本地优先合并）；
 * 4. 向 UI（弹层横幅/卡片实时刷新）分发读数事件。
 *
 * 与旧弹层方案的区别：扫描常驻 + 全目标，不再随弹层开闭启停；
 * RPA 且包内无 mac 的帧由 BleKit 的多目标试解兜住（V5 MIC 保证不误归档）。
 */
import { util } from '@kit.ArkTS';
import { fileIo as fs } from '@kit.CoreFileKit';
import { hilog } from '@kit.PerformanceAnalysisKit';
import { common } from '@kit.AbilityKit';
import { Device } from './types';
import { miot } from './native';
import { XiaomiBle, BleReading, normMac } from './BleKit';

const DOMAIN = 0x0000;
const TAG = 'miot-ble-ingest';

type ReadingCb = (r: BleReading) => void;

class BleIngestImpl {
  /** normMac → did（读数命中后反查设备，UI 用） */
  private macToDid: Map<string, string> = new Map();
  /** did → 24-hex 云 token（beaconkey 拿不到时明文帧/legacy 帧兜底） */
  private tokenByDid: Map<string, string> = new Map();
  /** did → 32-hex beaconkey（持久化 ble_keys.json） */
  private keys: Map<string, string> = new Map();
  /** 正在云端取 beaconkey 的 did（防并发重复取） */
  private fetching: Set<string> = new Set();
  /** 上次挂到 XiaomiBle 的目标签名（没变化不重启扫描） */
  private targetsSig: string = '';
  /** 重启扫描的合并定时器：多台 beaconkey 陆续到位时只重启一次 */
  private restartTimer: number = -1;
  private cbs: Set<ReadingCb> = new Set();
  private keysFile: string = '';
  private started: boolean = false;

  /** Index.aboutToAppear 调用：加载持久化 beaconkey。幂等。 */
  init(context: common.Context): void {
    if (this.keysFile !== '') {
      return;
    }
    this.keysFile = `${context.filesDir}/ble_keys.json`;
    this.loadKeys();
  }

  private loadKeys(): void {
    try {
      const stat = fs.statSync(this.keysFile);
      const buf = new ArrayBuffer(stat.size);
      const file = fs.openSync(this.keysFile, fs.OpenMode.READ_ONLY);
      try {
        fs.readSync(file.fd, buf);
      } finally {
        fs.closeSync(file);
      }
      const txt = new util.TextDecoder('utf-8').decodeToString(new Uint8Array(buf));
      const obj: Record<string, Object> = JSON.parse(txt) as Record<string, Object>;
      for (const k of Object.keys(obj)) {
        this.keys.set(k, obj[k] as string);
      }
      hilog.info(DOMAIN, TAG, 'ble keys loaded: %{public}d', this.keys.size);
    } catch (e) {
      // 首次运行无文件：静默
    }
  }

  private saveKeys(): void {
    if (this.keysFile === '') {
      return;
    }
    try {
      const obj: Record<string, string> = {};
      this.keys.forEach((v: string, k: string): void => {
        obj[k] = v;
      });
      const file = fs.openSync(this.keysFile,
        fs.OpenMode.READ_WRITE | fs.OpenMode.CREATE | fs.OpenMode.TRUNC);
      try {
        fs.writeSync(file.fd, JSON.stringify(obj));
      } finally {
        fs.closeSync(file);
      }
    } catch (e) {
      hilog.warn(DOMAIN, TAG, 'save keys failed: %{public}s', String(e));
    }
  }

  /** 单台设备取 beaconkey（异步；取到后重挂目标再起扫） */
  private fetchKey(did: string): void {
    this.fetching.add(did);
    miot.getBeaconkey(did)
      .then((k: string | null): void => {
        if (k !== null && k !== '') {
          this.keys.set(did, k);
          this.saveKeys();
          this.applyAndStart();
        }
      })
      .catch((): void => {
        // 取不到（断网/非 blt.*）：不阻断——token 兜底或收明文帧
      })
      .finally((): void => {
        this.fetching.delete(did);
      });
  }

  /** 组目标串并挂到 XiaomiBle；未启动或目标变化才重启扫描（500ms 合并抖动） */
  private applyAndStart(): void {
    const targets: Array<string[]> = [];
    this.macToDid.forEach((did: string, macN: string): void => {
      const k = this.keys.get(did);
      const key = k !== undefined ? k : (this.tokenByDid.get(did) ?? '');
      targets.push([macN, key]);
    });
    // 签名比较：目标集合（mac+key）没变就不重启扫描
    const sigParts: string[] = [];
    for (const t of targets) {
      sigParts.push(t[0] + '=' + t[1].length);
    }
    sigParts.sort();
    const sig = sigParts.join(',');
    if (this.started && sig === this.targetsSig) {
      return;
    }
    // 多台 beaconkey 陆续返回会连发多次 apply：合并成一次重启，
    // 避免 stop/start 互相打断广播窗口
    if (this.restartTimer >= 0) {
      clearTimeout(this.restartTimer);
    }
    this.restartTimer = setTimeout((): void => {
      this.restartTimer = -1;
      const btOn: boolean = XiaomiBle.isOn();
      XiaomiBle.setTargets(targets);
      const ok: boolean = XiaomiBle.start(this.onReading);
      this.started = ok;
      this.targetsSig = sig;
      hilog.info(DOMAIN, TAG, 'scan targets=%{public}d btOn=%{public}s started=%{public}s',
        targets.length, String(btOn), String(ok));
    }, 500);
  }

  /**
   * 设备列表变化时同步（loadDevices 后调用；幂等）。
   * 目标 = 有 mac 的设备；key 优先持久化的 beaconkey，异步预取缺的。
   */
  syncDevices(devices: Device[]): void {
    for (const d of devices) {
      if (d.mac === undefined || d.mac === null || d.mac === '') {
        continue;
      }
      const m = normMac(d.mac);
      this.macToDid.set(m, d.did);
      if (d.token !== undefined && d.token !== null && d.token !== '') {
        this.tokenByDid.set(d.did, d.token);
      }
      if (!this.keys.has(d.did) && !this.fetching.has(d.did)) {
        this.fetchKey(d.did);
      }
    }
    this.applyAndStart();
  }

  /** 弹层打开某台时确保已登记（并触发 key 预取）；全局扫描不受弹层开闭影响 */
  ensureDevice(did: string, mac: string, token: string): void {
    if (mac === '') {
      return;
    }
    const m = normMac(mac);
    if (this.macToDid.get(m) !== did) {
      this.macToDid.set(m, did);
    }
    if (token !== '' && !this.tokenByDid.has(did)) {
      this.tokenByDid.set(did, token);
    }
    if (!this.keys.has(did) && !this.fetching.has(did)) {
      this.fetchKey(did);
    }
    this.applyAndStart();
  }

  /** mac（任意写法）→ did；未登记返回 '' */
  didForMac(mac: string): string {
    const did: string | undefined = this.macToDid.get(normMac(mac));
    return did === undefined ? '' : did;
  }

  subscribe(cb: ReadingCb): void {
    this.cbs.add(cb);
  }

  unsubscribe(cb: ReadingCb): void {
    this.cbs.delete(cb);
  }

  /** 登出等场景整体停扫（下次 syncDevices 会再起） */
  stop(): void {
    XiaomiBle.stop();
    this.started = false;
    this.targetsSig = '';
  }

  /** 每条解码成功的读数：落 Rust 快照（本地优先数据源）+ 分发 UI */
  private onReading = (r: BleReading): void => {
    try {
      miot.bleIngest(JSON.stringify(r.adv));
      hilog.info(DOMAIN, TAG, 'ingested mac=%{public}s v=%{public}d rssi=%{public}d',
        r.adv.mac, r.adv.version, r.rssi);
    } catch (e) {
      hilog.warn(DOMAIN, TAG, 'bleIngest failed: %{public}s', String(e));
    }
    this.cbs.forEach((cb: ReadingCb): void => {
      cb(r);
    });
  };
}

export const BleIngest: BleIngestImpl = new BleIngestImpl();
