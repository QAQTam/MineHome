/**
 * 三方平台登录页（替代「默认密码登录」的主路径）：
 * Web 组件直跳设备平台的官方 H5 登录页（sid 作用域由 native 层决定），用户在平台
 * 自己的页面里用密码 / 短信验证码 / 扫码任一方式完成登录；登录成功后平台在账号域
 * 写 passToken/userId，本应用只做「探测 cookie → 重放 serviceLogin 换会话」这一步
 * （与通知确认同一条通道，Rust 零改动）。账号与密码全程只进平台服务器，
 * 不经过我们的代码、日志与存储。
 *
 * UI 文案约定（法务规避，勿在该页面的可见字串里写平台品牌名）：
 * 一律用「三方平台 / 平台方 / 所属平台」指代；内嵌 Web 内容是平台自己的页面，
 * 那里的品牌标识不由我们渲染，因此不 echo 它的 document.title 到本页顶栏。
 */

import { webview } from '@kit.ArkWeb';
import { hilog } from '@kit.PerformanceAnalysisKit';
import { miot } from '../miot/native';
import { LoginRouteParam } from '../miot/types';

const DOMAIN = 0x0000;
/** cookie 罐轮询间隔（ms）：登录链路写完 cookie 相对页面跳转有滞后 */
const PROBE_INTERVAL_MS: number = 1500;
/** 自动探测总时长（ms）：超时后停轮询，保留手动收尾按钮 */
const PROBE_TOTAL_MS: number = 300000;
/** 本页顶栏固定标题（不跟随内嵌页面标题，避免把平台品牌带进我们的界面） */
const PAGE_TITLE: string = '连接三方平台';
/**
 * 移动端 UA：平台 H5 按 UA 分支出「手机号/短信验证码」版登录页，
 * 默认 ArkWeb UA 会被识别成未适配的桌面端。必须在 controller 挂载后、
 * 首次加载前设置（Web 组件的 userAgent 属性自 API 10 起已废弃）。
 */
const LOGIN_UA: string = 'Mozilla/5.0 (Linux; Android 13; Pixel 7) AppleWebKit/537.36 ' +
  '(KHTML, like Gecko) Chrome/120.0.0.0 Mobile Safari/537.36';

@Entry
@Component
struct PlatformLogin {
  @State statusText: string = '正在打开平台登录页…';
  @State progress: number = 0;
  @State done: boolean = false;
  @State busy: boolean = false;
  @State loadFailed: boolean = false;
  /** 登录方式：false=账号/短信（平台页内自选），true=扫码（手机侧平台 App 确认） */
  @State qrMode: boolean = false;
  /** 先走平台 /pass/logout（换账号入口带进来），登出页落地后再清站点数据 */
  @State loggingOut: boolean = false;
  /** 首屏 URL（只在 build 时取一次，后续导航统一走 controller.loadUrl） */
  private initUrl: string = miot.webLoginUrl();
  private ctrl: webview.WebviewController = new webview.WebviewController();
  private probeTimer: number = -1;
  private deadline: number = 0;

  aboutToAppear(): void {
    // 平台登录链路依赖 cookie（deviceId/passToken/userId），必须允许写 cookie
    try {
      webview.WebCookieManager.putAcceptCookieEnabled(true);
      hilog.info(DOMAIN, 'miot', 'web login cookie allowed=%{public}s',
        String(webview.WebCookieManager.isCookieAllowed()));
    } catch (e) {
      hilog.warn(DOMAIN, 'miot', 'putAcceptCookieEnabled: %{public}s', String(e));
    }
    this.deadline = Date.now() + PROBE_TOTAL_MS;
    // 换账号入口：先加载平台官方登出页，断掉服务端网页会话（光清本地存储会被静默续登）
    const p: LoginRouteParam | undefined = this.getUIContext().getRouter().getParams() as LoginRouteParam;
    if (p !== undefined && p !== null && p.logoutFirst) {
      this.loggingOut = true;
      this.initUrl = miot.webLogoutUrl();
      this.statusText = '正在退出平台网页登录态…';
      hilog.info(DOMAIN, 'miot', 'login page opened for account switch');
    }
  }

  aboutToDisappear(): void {
    this.stopProbe();
  }

  private stopProbe(): void {
    if (this.probeTimer >= 0) {
      clearInterval(this.probeTimer);
      this.probeTimer = -1;
    }
  }

  /**
   * 切换登录方式。用 loadUrl 而不是改 src：Web 的 src 只在首次生效，
   * 改状态不会重新导航；controller 未挂载时 loadUrl 会抛错，兜底回退到状态值。
   */
  private switchMode(qr: boolean): void {
    if (this.qrMode === qr || this.done) {
      return;
    }
    this.qrMode = qr;
    this.loadFailed = false;
    this.progress = 0;
    const url: string = qr ? miot.webLoginQrUrl() : miot.webLoginUrl();
    this.statusText = qr ? '请用设备平台的 App 扫描页面二维码并确认' : '正在打开平台登录页…';
    try {
      this.ctrl.loadUrl(url);
    } catch (e) {
      hilog.warn(DOMAIN, 'miot', 'switchMode loadUrl failed, fallback src: %{public}s', String(e));
    }
    this.startProbe();
  }

  /**
   * 登出页已落地：清掉 origin storage + cookie（静默续登的真凶），再载正常登录页。
   * 顺序很关键 —— 先让平台自己把会话断掉，否则前端拿着 storage 里的登录态
   * 会在几秒内重新换出凭据，用户以为换了账号其实还是旧号。
   */
  private async finishLogout(): Promise<void> {
    this.loggingOut = false;
    try {
      await miot.clearWebLoginState();
    } catch (e) {
      hilog.warn(DOMAIN, 'miot', 'clearWebLoginState failed: %{public}s', String(e));
    }
    this.qrMode = false;
    this.statusText = '已退出网页登录态，正在打开登录页…';
    try {
      this.ctrl.loadUrl(miot.webLoginUrl());
    } catch (e) {
      hilog.warn(DOMAIN, 'miot', 'reload login url failed: %{public}s', String(e));
    }
    this.startProbe();
  }

  /** 低频轮询兜底：onLoadIntercept/onPageEnd 命中外，cookie 也可能晚于事件落地 */
  private startProbe(): void {
    if (this.probeTimer >= 0 || this.done) {
      return;
    }
    this.probeTimer = setInterval((): void => {
      if (this.done || Date.now() > this.deadline) {
        this.stopProbe();
        return;
      }
      this.probe('poll');
    }, PROBE_INTERVAL_MS);
  }

  /** 提 passToken+userId → 换会话；成功后退回首页（Index.onPageShow 自动拉设备） */
  private async probe(from: string): Promise<void> {
    if (this.done || this.busy) {
      return;
    }
    this.busy = true;
    try {
      const ok: boolean = await miot.loginWithWebCookies();
      if (ok) {
        this.done = true;
        this.stopProbe();
        this.statusText = '连接成功，正在返回设备列表…';
        hilog.info(DOMAIN, 'miot', 'web login finished from=%{public}s', from);
        setTimeout((): void => {
          this.getUIContext().getRouter().back();
        }, 800);
      } else if (from === 'manual') {
        this.statusText = '还没拿到登录态——请在页面上完成登录（密码/短信/扫码均可）';
      }
    } catch (e) {
      const msg = String(e);
      hilog.error(DOMAIN, 'miot', 'web login finish failed: %{public}s', msg);
      this.statusText = `建立连接失败：${msg}`;
    } finally {
      this.busy = false;
    }
  }

  /** 登录方式切换（账号/短信 ↔ 扫码） */
  @Builder
  modeSwitch() {
    Row({ space: 6 }) {
      Text('账号 / 短信')
        .fontSize(12)
        .fontWeight(this.qrMode ? FontWeight.Regular : FontWeight.Medium)
        .fontColor(this.qrMode ? $r('app.color.text_secondary') : $r('app.color.brand'))
        .backgroundColor(this.qrMode ? Color.Transparent : $r('app.color.brand_tint'))
        .padding({ left: 12, right: 12, top: 5, bottom: 5 })
        .borderRadius(999)
        .onClick((): void => {
          this.switchMode(false);
        })
      Text('扫码登录')
        .fontSize(12)
        .fontWeight(this.qrMode ? FontWeight.Medium : FontWeight.Regular)
        .fontColor(this.qrMode ? $r('app.color.brand') : $r('app.color.text_secondary'))
        .backgroundColor(this.qrMode ? $r('app.color.brand_tint') : Color.Transparent)
        .padding({ left: 12, right: 12, top: 5, bottom: 5 })
        .borderRadius(999)
        .onClick((): void => {
          this.switchMode(true);
        })
    }
    .width('100%')
    .justifyContent(FlexAlign.Center)
    .padding({ top: 2, bottom: 6 })
  }

  /** 顶栏：返回 + 固定标题 + 完成态（标题不跟随内嵌页面，避免带出平台品牌） */
  @Builder
  header() {
    Row({ space: 8 }) {
      Row() {
        SymbolGlyph($r('sys.symbol.chevron_left'))
          .fontSize(20)
          .fontColor([$r('app.color.text_primary')])
      }
      .width(32)
      .height(32)
      .borderRadius(16)
      .backgroundColor($r('app.color.neutral_tint'))
      .justifyContent(FlexAlign.Center)
      .onClick((): void => {
        this.getUIContext().getRouter().back();
      })

      Column({ space: 1 }) {
        Text(PAGE_TITLE)
          .fontSize(16)
          .fontWeight(FontWeight.Medium)
          .fontColor($r('app.color.text_primary'))
          .maxLines(1)
          .textOverflow({ overflow: TextOverflow.Ellipsis })
        Text('登录页面由平台方提供，凭据不经过本应用')
          .fontSize(11)
          .fontColor($r('app.color.text_secondary'))
          .maxLines(1)
      }
      .alignItems(HorizontalAlign.Start)
      .layoutWeight(1)

      if (this.done) {
        Row({ space: 4 }) {
          SymbolGlyph($r('sys.symbol.checkmark_circle_fill'))
            .fontSize(15)
            .fontColor([$r('app.color.online_green')])
          Text('已完成')
            .fontSize(12)
            .fontColor($r('app.color.green_text'))
        }
      } else if (this.progress < 100) {
        LoadingProgress()
          .width(18)
          .height(18)
          .color($r('app.color.brand'))
      }
    }
    .width('100%')
    .padding({ left: 12, right: 12, top: 10, bottom: 8 })
  }

  build() {
    Column() {
      this.header()

      this.modeSwitch()

      // 加载进度条（页面级，不遮挡内嵌页面自身内容）
      Progress({ value: this.progress, total: 100, type: ProgressType.Linear })
        .width('100%')
        .height(3)
        .color($r('app.color.brand'))
        .backgroundColor($r('app.color.separator'))
        .visibility(this.done || this.progress >= 100 ? Visibility.Hidden : Visibility.Visible)

      if (this.loadFailed) {
        Column({ space: 12 }) {
          SymbolGlyph($r('sys.symbol.wifi'))
            .fontSize(34)
            .fontColor([$r('app.color.text_secondary')])
          Text('平台登录页加载失败')
            .fontSize(15)
            .fontWeight(FontWeight.Medium)
            .fontColor($r('app.color.text_primary'))
          Text('检查网络后重试；也可退回「我的」用已导出的会话文件连接')
            .fontSize(12)
            .fontColor($r('app.color.text_secondary'))
          Button('重新加载')
            .height(34)
            .fontSize(14)
            .backgroundColor($r('app.color.brand'))
            .onClick((): void => {
              this.loadFailed = false;
              this.statusText = '正在重新打开平台登录页…';
              try {
                this.ctrl.loadUrl(miot.webLoginUrl());
              } catch (e) {
                this.statusText = `加载失败：${String(e)}`;
              }
            })
        }
        .width('100%')
        .layoutWeight(1)
        .justifyContent(FlexAlign.Center)
      } else {
        Web({ src: this.initUrl, controller: this.ctrl })
          .domStorageAccess(true)
          .javaScriptAccess(true)
          .fileAccess(false)
          .onControllerAttached((): void => {
            this.ctrl.setCustomUserAgent(LOGIN_UA);
          })
          .layoutWeight(1)
          .onProgressChange((event: OnProgressChangeEvent) => {
            this.progress = event.newProgress;
          })
          .onPageBegin(() => {
            this.loadFailed = false;
            if (!this.done) {
              this.startProbe();
            }
          })
          .onPageEnd(() => {
            if (this.loggingOut) {
              this.finishLogout();
              return;
            }
            // 回调链落地后再探一次（cookie 写入通常已完成）
            this.probe('pageEnd');
            if (!this.done && !this.loadFailed) {
              // 页面已就绪：把「正在打开…」换成明确的下一步指引
              this.statusText = '请在页面完成登录（密码 / 短信验证码 / 扫码均可）';
            }
          })
          .onLoadIntercept((event: OnLoadInterceptEvent): boolean => {
            const url: string = event.data.getRequestUrl();
            // 登录成功 → 302 回调 sts 域：cookie 已就位，立刻收尾。
            // 不阻断导航（阻断可能让凭据的 Set-Cookie 链断在半路）。
            if (url.indexOf('sts.api.io.mi.com/sts') >= 0) {
              this.statusText = '登录完成，正在建立连接…';
              this.probe('callback');
            }
            return false;
          })
          .onErrorReceive((event: OnErrorReceiveEvent) => {
            if (event.request.isMainFrame()) {
              this.loadFailed = true;
              this.statusText = '平台登录页加载失败（网络不可达）';
              hilog.error(DOMAIN, 'miot', 'web login error code=%{public}d url=%{public}s',
                event.error.getErrorCode(), event.request.getRequestUrl());
            }
          })
      }

      // ---------- 底部：状态 + 手动收尾（自动探测失败时的兜底） ----------
      Column({ space: 8 }) {
        Text(this.done ? '连接已建立' : (this.busy ? '正在建立连接…' : this.statusText))
          .fontSize(12)
          .fontColor(this.done ? $r('app.color.green_text') : $r('app.color.text_secondary'))
          .maxLines(2)
          .textOverflow({ overflow: TextOverflow.Ellipsis })
          .width('100%')
          .textAlign(TextAlign.Center)
        if (!this.done) {
          Button('我已完成登录')
            .width('70%')
            .height(36)
            .fontSize(14)
            .backgroundColor($r('app.color.brand'))
            .enabled(!this.busy)
            .onClick((): void => {
              this.probe('manual');
            })
        }
      }
      .width('100%')
      .padding({ left: 16, right: 16, top: 8, bottom: 12 })
    }
    .width('100%')
    .height('100%')
    .backgroundColor($r('app.color.page_bg'))
  }
}
