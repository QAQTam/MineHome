# -*- coding: utf-8 -*-
"""
登录原型第二步：securityStatus=16（需通知确认）的 headless 解法
================================================================

原理：米家 App 对该分支的处理就是开 WebView 加载 notificationUrl（
com.xiaomi.accountsdk: NeedNotificationException -> 通知登录页），
用户在已登录小米账号的设备上点"允许"后，页面写入 passToken cookie，
SDK 再用 passToken 调 /pass/serviceLogin 换取 ssecurity/serviceToken。

本脚本 = requests 拿 notificationUrl（复用 mijia_proto 的登录前两步）
       + playwright 打开通知页（默认有头，便于观察；--headless 可后台）
       + 等待手机确认 → 抓 passToken/userId cookie
       + 复用 mijia_proto._pass_token_login 完成 session.json

用法：
  pip install playwright && playwright install chromium
  python login_browser.py <user> <password>
  python login_browser.py <user> <password> --headless   # 无界面模式
  python login_browser.py --resume                       # 已有 notify_url 时复用
"""
import json
import os
import sys
import time

import requests

import mijia_proto as mp


def fetch_notification_url(user: str, password: str):
    """前两步登录，返回 (notificationUrl|None, userId)。"""
    sess = requests.Session()
    sess.headers["User-Agent"] = mp.UA

    r1 = sess.get("https://account.xiaomi.com/pass/serviceLogin",
                  params={"sid": "xiaomiio", "_json": "true"}, timeout=15)
    step1 = json.loads(mp._strip_jsonp(r1.text))
    print("[1] serviceLogin code=%s" % step1.get("code"))

    r2 = sess.post("https://account.xiaomi.com/pass/serviceLoginAuth2", data={
        "user": user,
        "hash": __import__("hashlib").md5(password.encode()).hexdigest().upper(),
        "qs": step1.get("qs"), "_sign": step1.get("_sign"),
        "sid": "xiaomiio", "_json": "true",
    }, timeout=15)
    step2 = json.loads(mp._strip_jsonp(r2.text))
    print("[2] auth2 code=%s desc=%s" % (step2.get("code"), step2.get("desc")))
    if step2.get("ssecurity"):            # 直连成功，直接收尾
        out = mp._finish_client_sign(sess, step2)
        out.pop("final", None)
        mp.save_session(out)
        return None, step2.get("userId")
    url = step2.get("notificationUrl")
    if not url:
        print("无 notificationUrl，响应键:", list(step2.keys()))
        sys.exit(1)
    return url, step2.get("userId")


def browser_confirm(url: str, headless: bool = False, timeout_s: int = 180):
    """打开通知页，等手机确认，返回 (passToken, userId) cookie。"""
    from playwright.sync_api import sync_playwright

    print("[3] 打开通知页（请在已登录小米账号的手机上点“允许”）")
    with sync_playwright() as p:
        # 优先用系统自带浏览器，避免下载 chromium（国内网络易失败）
        browser = None
        for channel in ("msedge", "chrome"):
            try:
                browser = p.chromium.launch(channel=channel, headless=headless)
                print("    使用系统浏览器:", channel)
                break
            except Exception as e:
                print("    channel=%s 不可用: %s" % (channel, str(e).splitlines()[0]))
        if browser is None:
            browser = p.chromium.launch(headless=headless)
        ctx = browser.new_context(
            user_agent="Mozilla/5.0 (Windows NT 10.0; Win64; x64) Chrome/120 Safari/537.36",
            locale="zh-CN",
        )
        page = ctx.new_page()
        page.goto(url, wait_until="domcontentloaded", timeout=30000)
        deadline = time.time() + timeout_s
        while time.time() < deadline:
            # 全量 cookie（确认完成后 serviceToken 可能落在 io.mi.com / mi.com 域）
            all_cookies = {c["name"]: c["value"] for c in ctx.cookies()}
            pass_token = next((v for k, v in all_cookies.items()
                               if k == "passToken"), None)
            service_token = next((v for k, v in all_cookies.items()
                                  if k.startswith("serviceToken")), None)
            if service_token:
                print("[4] 浏览器 cookie 里已有 serviceToken（完整登录已完成）")
                browser.close()
                return pass_token, all_cookies.get("userId"), service_token
            if pass_token:
                print("[4] 页面已写入 passToken，等待 serviceToken（点确认后页面会跳转）...")
            time.sleep(2)
        browser.close()
        raise TimeoutError("等待手机确认超时（%ds），重跑一次即可" % timeout_s)


CACHE = os.path.join(os.path.dirname(os.path.abspath(__file__)), "pass_token.json")


def cached_pass_token():
    try:
        with open(CACHE, encoding="utf-8") as f:
            return json.load(f)
    except Exception:
        return None


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    headless = "--headless" in sys.argv
    user, password = args[0], args[1]

    url, user_id = fetch_notification_url(user, password)
    if url is None:                       # 直连路径已在函数内存好 session
        print("完成！现在可以运行 python mijia_proto.py devices")
        return

    cache = cached_pass_token()           # 复用上次确认过的 passToken
    pass_token = cache and cache.get("passToken")
    user_id = (cache and cache.get("userId")) or user_id
    browser_service_token = None
    if pass_token:
        print("[3] 使用缓存的 passToken（pass_token.json）")
    else:
        pass_token, user_id, browser_service_token = browser_confirm(url, headless=headless)
        with open(CACHE, "w", encoding="utf-8") as f:
            json.dump({"passToken": pass_token, "userId": user_id}, f)

    print("[5] 用 passToken 换 ssecurity ...")
    out, step4 = mp._pass_token_login(requests.Session(), pass_token, user_id)
    if not out or not out.get("ssecurity"):
        print("passToken 换取失败：")
        if step4:
            mp.dump_masked(step4)
            if step4.get("code") == 70016:
                print("    passToken 已失效，下次运行将重新走浏览器确认")
                try:
                    os.remove(CACHE)
                except OSError:
                    pass
        sys.exit(1)
    if browser_service_token:
        out["serviceToken"] = browser_service_token
    if not out.get("serviceToken"):
        if browser_service_token is None and os.path.exists(CACHE):
            # 缓存的 passToken 只能拿 ssecurity；serviceToken 需要完整浏览器确认
            print("    缓存会话缺 serviceToken，已清缓存——重跑一次并完成手机确认")
            os.remove(CACHE)
        else:
            print("    确认手机点完“允许”后，让页面完全加载完再等几秒")
        sys.exit(1)
    out.pop("final", None)
    mp.save_session(out)
    print("完成！现在可以运行：")
    print("   python mijia_proto.py devices")
    print("   python mijia_proto.py switch <did> on")


if __name__ == "__main__":
    main()
