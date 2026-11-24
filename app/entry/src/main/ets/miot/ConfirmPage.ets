/**
 * 通知确认页（securityStatus=16）：Web 组件加载 notificationUrl，
 * 等用户在已登录小米账号的设备上点"允许"后，从 cookie 提取 passToken 收尾。
 */

import { webview } from '@kit.ArkWeb';
import { router } from '@kit.ArkUI';
import { hilog } from '@kit.PerformanceAnalysisKit';
import { miot } from './native';

@Entry
@Component
struct MiotConfirmPage {
  @State confirmStatus: string = '正在打开通知确认页…';
  @State confirmFinished: boolean = false;
  private webController: webview.WebviewController = new webview.WebviewController();

  private async tryFinish(): Promise<void> {
    if (this.confirmFinished) {
      return;
    }
    const url = miot.pendingConfirmUrl();
    if (url === '') {
      this.confirmStatus = '无待确认登录';
      return;
    }
    try {
      const ok: boolean = await miot.tryFinishConfirmFromWeb(url);
      hilog.info(0x0000, 'miot', 'confirm finish probe result=%{public}s', String(ok));
      if (ok) {
        this.confirmFinished = true;
        this.confirmStatus = '登录完成';
        router.back();
      } else {
        this.confirmStatus = '尚未确认——请在手机上点"允许"后重试';
      }
    } catch (e) {
      hilog.error(0x0000, 'miot', 'confirm finish error: %{public}s', String(e));
      this.confirmStatus = `确认失败：${String(e)}`;
    }
  }

  build() {
    Column() {
      Text(this.confirmStatus)
        .fontSize(14)
        .fontColor('#666666')
        .padding(12)

      Web({ src: miot.pendingConfirmUrl(), controller: this.webController })
        .domStorageAccess(true)
        .fileAccess(false)
        .layoutWeight(1)
        .onPageEnd(() => {
          this.tryFinish();
        })

      Button('我已在手机上确认')
        .width('80%')
        .margin(12)
        .onClick(() => {
          this.tryFinish();
        })
    }
    .width('100%')
    .height('100%')
  }
}
