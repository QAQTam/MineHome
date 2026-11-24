/**
 * SpecKit —— miot-spec.org 公开注册表的 spec 获取 + 磁盘缓存 + 领域解析。
 * device_list_page 只下发 spec_type URN；能力明细（可读/可写属性）从
 * https://miot-spec.org/miot-spec-v2/instance?type=<urn> 拉取并缓存到
 * <filesDir>/spec/。纯只读模块，不含任何控制下发。
 */

import { http } from '@kit.NetworkKit';
import { fileIo as fs } from '@kit.CoreFileKit';
import { hilog } from '@kit.PerformanceAnalysisKit';
import { util } from '@kit.ArkTS';

/** 枚举值项（value-list） */
export interface MiotEnumItem {
  value: number;
  desc: string;
}

/** spec 属性（领域形态：siid/piid/名称/访问标志/取值约束） */
export interface MiotSpecProp {
  siid: number;
  piid: number;
  service: string;
  name: string;
  desc: string;
  access: string[];
  format: string;
  hasRange: boolean;
  rangeMin: number;
  rangeMax: number;
  rangeStep: number;
  enums: MiotEnumItem[];
}

/** spec 服务 */
export interface MiotSpecService {
  iid: number;
  name: string;
  desc: string;
  props: MiotSpecProp[];
}

/** 设备 spec */
export interface MiotDeviceSpec {
  type: string;
  services: MiotSpecService[];
}

const TAG_DOMAIN = 0x0000;
const TAG = 'miot';

function urnName(urn: string): string {
  const parts: Array<string> = urn.split(':');
  return parts.length >= 4 ? parts[3] : urn;
}

/** 设备分类图标（spec_type 类别优先，model 兜底） */
export function emojiFor(specType: string, model: string): string {
  const cat = specType === '' ? '' : urnName(specType);
  if (cat === 'light' || cat === 'night-light') {
    return '💡';
  }
  if (cat === 'air-conditioner' || cat === 'air-condition-outlet-matching') {
    return '❄️';
  }
  if (cat === 'fan') {
    return '🌬️';
  }
  if (cat === 'outlet' || cat === 'socket' || cat === 'switch') {
    return '🔌';
  }
  if (cat === 'camera') {
    return '🎥';
  }
  if (cat === 'router') {
    return '📶';
  }
  if (cat === 'gateway') {
    return '🏠';
  }
  if (cat === 'massager') {
    return '💆';
  }
  if (cat === 'sensor') {
    return '📡';
  }
  if (cat === 'temperature-humidity-sensor') {
    return '🌡️';
  }
  if (cat === 'door-lock' || cat === 'contact-sensor' || cat === 'magnet') {
    return '🚪';
  }
  if (cat === 'human-presence-sensor') {
    return '🚶';
  }
  if (cat === 'battery' || cat === 'charger') {
    return '🔋';
  }
  if (model.indexOf('sensor_ht') >= 0) {
    return '🌡️';
  }
  if (model.indexOf('magnet') >= 0) {
    return '🚪';
  }
  if (model.indexOf('switch') >= 0) {
    return '🔘';
  }
  if (model.indexOf('router') >= 0) {
    return '📶';
  }
  return '📦';
}

/** 设备分类符号图标 + 着色（图标容器底色/前景色，米家蓝体系内微调） */
export class DeviceIconTint {
  /** sys.symbol 图标资源（名称已对照 SDK sysResource.js 校验，无效名会编译失败） */
  res: Resource = $r('sys.symbol.house_equipment');
  /** 图标前景色 */
  fg: string = '#9899a0';
  /** 图标容器底色 */
  bg: string = '#f0f0f2';
}

/** 设备分类符号图标（spec_type 类别优先，model 兜底；与 emojiFor 同一映射逻辑） */
export function iconFor(specType: string, model: string): DeviceIconTint {
  const t = new DeviceIconTint();
  const cat = specType === '' ? '' : urnName(specType);
  if (cat === 'light' || cat === 'night-light') {
    t.res = $r('sys.symbol.lightbulb_max');
    t.fg = '#f59e0b';
    return t;
  }
  if (cat === 'air-conditioner' || cat === 'air-condition-outlet-matching') {
    t.res = $r('sys.symbol.snowflake');
    t.fg = '#4a90e2';
    return t;
  }
  if (cat === 'fan') {
    t.res = $r('sys.symbol.fan_fill_1');
    t.fg = '#2fb380';
    return t;
  }
  if (cat === 'outlet' || cat === 'socket' || cat === 'switch') {
    t.res = $r('sys.symbol.powercord_fill');
    t.fg = '#3478f6';
    return t;
  }
  if (cat === 'camera') {
    t.res = $r('sys.symbol.camera_fill');
    t.fg = '#7b61ff';
    return t;
  }
  if (cat === 'router') {
    t.res = $r('sys.symbol.wifi_router_fill');
    t.fg = '#3478f6';
    return t;
  }
  if (cat === 'gateway') {
    t.res = $r('sys.symbol.house_equipment');
    t.fg = '#6b7a90';
    return t;
  }
  if (cat === 'massager') {
    t.res = $r('sys.symbol.person_massage');
    t.fg = '#ec7fb0';
    return t;
  }
  if (cat === 'sensor') {
    t.res = $r('sys.symbol.signal');
    t.fg = '#14b8a6';
    return t;
  }
  if (cat === 'temperature-humidity-sensor') {
    t.res = $r('sys.symbol.thermometer_medium_fill');
    t.fg = '#ff7a45';
    return t;
  }
  if (cat === 'door-lock' || cat === 'contact-sensor' || cat === 'magnet') {
    t.res = $r('sys.symbol.doors_and_windows_closed');
    t.fg = '#a0714f';
    return t;
  }
  if (cat === 'human-presence-sensor') {
    t.res = $r('sys.symbol.figure_walk');
    t.fg = '#3478f6';
    return t;
  }
  if (cat === 'battery' || cat === 'charger') {
    t.res = $r('sys.symbol.battery_bolt_fill');
    t.fg = '#34c759';
    return t;
  }
  if (model.indexOf('sensor_ht') >= 0) {
    t.res = $r('sys.symbol.thermometer_medium_fill');
    t.fg = '#ff7a45';
    return t;
  }
  if (model.indexOf('magnet') >= 0) {
    t.res = $r('sys.symbol.doors_and_windows_closed');
    t.fg = '#a0714f';
    return t;
  }
  if (model.indexOf('switch') >= 0) {
    t.res = $r('sys.symbol.powercord_fill');
    t.fg = '#3478f6';
    return t;
  }
  if (model.indexOf('router') >= 0) {
    t.res = $r('sys.symbol.wifi_router_fill');
    t.fg = '#3478f6';
    return t;
  }
  // 底色 = 前景色 15% alpha（#AARRGGBB）：亮色卡上呈浅色调、暗色卡上呈深色微光，自动适配深浅色
  t.bg = '#26' + t.fg.substring(1);
  return t;
}

/** 观察值可视化指标图标（温度计/水滴/电池等，米家传感器卡样式） */
export class MetricIconTint {
  res: Resource = $r('sys.symbol.drop_fill');
  color: string = '#3aa2e8';
}

/** 按属性标签/单位推断指标图标；未知类型返回 null（卡片回落纯文本状态行） */
export function metricIcon(label: string, unit: string): MetricIconTint | null {
  const t = new MetricIconTint();
  const l = label;
  const u = unit;
  // 电量先于 '%' 湿度判断（电池单位也是 %）
  if (l.indexOf('电') >= 0 || l.indexOf('battery') >= 0) {
    t.res = $r('sys.symbol.battery_fill');
    t.color = '#34c759';
    return t;
  }
  if (l.indexOf('温') >= 0 || u.indexOf('℃') >= 0 || u.indexOf('°C') >= 0) {
    t.res = $r('sys.symbol.thermometer_medium_fill');
    t.color = '#ff7a45';
    return t;
  }
  if (l.indexOf('湿') >= 0 || u.indexOf('%') >= 0) {
    t.res = $r('sys.symbol.drop_fill');
    t.color = '#3aa2e8';
    return t;
  }
  if (l.indexOf('光') >= 0 || l.indexOf('照') >= 0 || l.indexOf('illumination') >= 0) {
    t.res = $r('sys.symbol.sun_max_fill');
    t.color = '#f5a623';
    return t;
  }
  if (l.indexOf('门') >= 0 || l.indexOf('磁') >= 0) {
    t.res = $r('sys.symbol.doors_and_windows_closed');
    t.color = '#a0714f';
    return t;
  }
  if (l.indexOf('人') >= 0 || l.indexOf('存在') >= 0) {
    t.res = $r('sys.symbol.figure_walk');
    t.color = '#3478f6';
    return t;
  }
  return null;
}

function num(v: Object, key: string): number {
  const rec = v as Record<string, Object>;
  const raw = rec[key];
  return typeof raw === 'number' ? raw as number : 0;
}

function str(v: Object, key: string): string {
  const rec = v as Record<string, Object>;
  const raw = rec[key];
  return typeof raw === 'string' ? raw as string : '';
}

function cachePath(filesDir: string, specType: string): string {
  const safe = specType.replace(/:/g, '_');
  return `${filesDir}/spec/${safe}.json`;
}

function parseSpecRoot(root: Record<string, Object>): MiotDeviceSpec {
  const spec: MiotDeviceSpec = { type: str(root, 'type'), services: [] };
  const servicesRaw = root['services'] as Array<Object>;
  if (servicesRaw === undefined || servicesRaw === null) {
    return spec;
  }
  for (const s of servicesRaw) {
    const so = s as Record<string, Object>;
    const svc: MiotSpecService = {
      iid: num(s, 'iid'),
      name: urnName(str(s, 'type')),
      desc: str(s, 'description'),
      props: []
    };
    const propsRaw = so['properties'] as Array<Object>;
    if (propsRaw !== undefined && propsRaw !== null) {
      for (const p of propsRaw) {
        const po = p as Record<string, Object>;
        const prop: MiotSpecProp = {
          siid: svc.iid,
          piid: num(p, 'iid'),
          service: svc.name,
          name: urnName(str(p, 'type')),
          desc: str(p, 'description'),
          access: [],
          format: str(p, 'format'),
          hasRange: false,
          rangeMin: 0,
          rangeMax: 0,
          rangeStep: 1,
          enums: []
        };
        const accRaw = po['access'] as Array<Object>;
        if (accRaw !== undefined && accRaw !== null) {
          for (const a of accRaw) {
            prop.access.push(String(a));
          }
        }
        const vr = po['value-range'] as Array<Object>;
        if (vr !== undefined && vr !== null && vr.length >= 3) {
          prop.hasRange = true;
          prop.rangeMin = vr[0] as number;
          prop.rangeMax = vr[1] as number;
          prop.rangeStep = vr[2] as number;
        }
        const vl = po['value-list'] as Array<Object>;
        if (vl !== undefined && vl !== null) {
          for (const item of vl) {
            prop.enums.push({ value: num(item, 'value'), desc: str(item, 'description') });
          }
        }
        svc.props.push(prop);
      }
    }
    spec.services.push(svc);
  }
  return spec;
}

function parseSpec(text: string): MiotDeviceSpec {
  return parseSpecRoot(JSON.parse(text) as Record<string, Object>);
}

export class SpecKit {
  private static cache: Map<string, MiotDeviceSpec> = new Map();

  /** 加载 spec：内存缓存 → 磁盘缓存 → 网络。失败抛错由调用方兜底。 */
  static async load(specType: string, filesDir: string): Promise<MiotDeviceSpec> {
    const hit = SpecKit.cache.get(specType);
    if (hit !== undefined) {
      return hit;
    }
    const path = cachePath(filesDir, specType);
    if (fs.accessSync(path)) {
      const file = fs.openSync(path, fs.OpenMode.READ_ONLY);
      try {
        const stat = fs.statSync(file.fd);
        const buf: ArrayBuffer = new ArrayBuffer(stat.size);
        fs.readSync(file.fd, buf);
        const spec = parseSpec(util.TextDecoder.create('utf-8').decodeToString(new Uint8Array(buf)));
        SpecKit.cache.set(specType, spec);
        return spec;
      } finally {
        fs.closeSync(file);
      }
    }
    const text = await SpecKit.fetch(specType);
    const spec = parseSpec(text);
    SpecKit.save(path, text);
    SpecKit.cache.set(specType, spec);
    return spec;
  }

  private static async fetch(specType: string): Promise<string> {
    const url = `https://miot-spec.org/miot-spec-v2/instance?type=${specType}`;
    const req = http.createHttp();
    try {
      const opts: http.HttpRequestOptions = {
        method: http.RequestMethod.GET,
        expectDataType: http.HttpDataType.STRING,
        connectTimeout: 10000,
        readTimeout: 10000
      };
      const resp: http.HttpResponse = await req.request(url, opts);
      if (resp.responseCode !== 200) {
        hilog.error(TAG_DOMAIN, TAG, 'spec fetch %{public}s http=%{public}d', specType, resp.responseCode);
        throw new Error(`spec 拉取失败 http=${resp.responseCode}`);
      }
      return resp.result as string;
    } finally {
      req.destroy();
    }
  }

  private static save(path: string, text: string): void {
    try {
      const dir = path.substring(0, path.lastIndexOf('/'));
      if (!fs.accessSync(dir)) {
        fs.mkdirSync(dir, true);
      }
      const file = fs.openSync(path, fs.OpenMode.READ_WRITE | fs.OpenMode.CREATE | fs.OpenMode.TRUNC);
      try {
        fs.writeSync(file.fd, text);
      } finally {
        fs.closeSync(file);
      }
    } catch (e) {
      hilog.error(TAG_DOMAIN, TAG, 'spec save failed: %{public}s', String(e));
    }
  }

  // findOnProp / fromObject 已于 2026-11-17 删除：「规范 on 属性」的查找
  // 随一键圆钮判定一起下沉到 Rust（`safety.rs::spec_on_prop`，口径一致：
  // 先要 bool+可写，退一步只要求可写）。两份实现会漂移，所以这里不再留副本。

  /** 卡片状态用的只读属性（最多 2 个）：温湿度/门磁等传感器优先 */
  static findStatusProps(spec: MiotDeviceSpec): MiotSpecProp[] {
    const preferred: Array<string> =
      ['temperature', 'relative-humidity', 'contact-state', 'illumination', 'battery-level'];
    const out: MiotSpecProp[] = [];
    for (const want of preferred) {
      for (const svc of spec.services) {
        for (const p of svc.props) {
          if (p.name === want && p.access.includes('read')) {
            out.push(p);
            if (out.length >= 2) {
              return out;
            }
          }
        }
      }
    }
    return out;
  }

  /** 详情页用的全部可读属性（跳过 device-information 服务） */
  static readableProps(spec: MiotDeviceSpec): MiotSpecProp[] {
    const out: MiotSpecProp[] = [];
    for (const svc of spec.services) {
      if (svc.name === 'device-information') {
        continue;
      }
      for (const p of svc.props) {
        if (p.access.includes('read')) {
          out.push(p);
        }
      }
    }
    return out;
  }
}

