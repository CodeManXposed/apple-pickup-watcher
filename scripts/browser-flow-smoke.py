"""Exercise the extension in real Chrome against local checkout fixtures.

Requires Chrome for Testing and ``pip install websockets==14.2``. Set
APW_CHROME_PATH if Chrome is not in PATH or the app's local browser directory.
The script copies and adapts the extension into a temporary directory; it never
changes the user's Chrome profile or contacts Apple, Alipay, or Discord.
"""

import asyncio
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlencode
from urllib.request import urlopen

import websockets

ROOT = Path(__file__).resolve().parents[1]
TOKEN = "a" * 32
TASK = "b" * 32
PART = "MJYE4CH/A"
STORE = "R683"
QR = "https://qr.alipay.com/apw-local-test-only"
PAGE_HITS = []
BRIDGE_REQUESTS = []
BRIDGE_RESPONSES = []
LATEST_PAYMENT = None
TEST_PAYMENT_MODE = ""

PAGES = {
    "/shop/pdpAddToBag/MJYE4CH/A": '''<!doctype html><meta charset="utf-8"><title>测试商品</title>
<form data-part-number="MJYE4CH/A"><button data-autom="add-to-cart" type="button"
 onclick="location.href='/shop/buy-iphone/test?step=attach&product=MJYE4CH%2FA'">添加到购物袋</button></form>''',
    "/shop/buy-iphone/test": '''<!doctype html><meta charset="utf-8"><title>配件推荐</title>''',
    "/shop/bag": '''<!doctype html><meta charset="utf-8"><title>购物袋</title>
<div class="rs-iteminfo-details"><a data-autom="bag-item-name"
 href="/shop/product/MJYE4CH/A">测试商品</a><select data-autom="item-quantity-dropdown">
<option value="1" selected>1</option></select></div>
<div class="rs-bag-checkout-mainbutton"><button data-autom="checkout"
 onclick="location.href='/shop/signIn'">结账</button></div>''',
    "/shop/signIn": '''<!doctype html><meta charset="utf-8"><title>访客结账</title>
<button onclick="location.href='/shop/checkout'">以游客身份继续</button>''',
    "/shop/checkout": '''<!doctype html><meta charset="utf-8"><title>结账</title>
<section id="fulfillment"><h2>到店取货</h2><label><input type="radio"
 class="form-selector-input" name="pickupStore" value="R683" checked>上海环球港</label>
<label>取货签到时段<select aria-label="取货签到时段"><option value="12:30" selected>
12:30 – 12:45</option></select></label><button data-autom="fulfillment-continue-button"
 onclick="document.getElementById('fulfillment').style.display='none';
 document.getElementById('contact').style.display='block'">继续</button></section>
<section id="contact" class="pickup-contact" style="display:none"><h2>取货联系人</h2>
<label>姓名 <input name="pickupFullName" required></label>
<label>手机 <input name="pickupPhone" type="tel" required></label>
<label>邮箱 <input name="pickupEmail" type="email" required></label>
<div class="rs-pickup-button"><button onclick="document.getElementById('contact').style.display='none';
 document.getElementById('payment').style.display='block'">继续</button></div></section>
<section id="payment" style="display:none"><h2>付款方式</h2>
<label><input type="radio" id="alipay" name="payment" value="alipay">支付宝</label>
<button data-autom="continue-button-review" onclick="document.getElementById('payment').style.display='none';
 document.getElementById('review').style.display='block'">继续</button></section>
<section id="review" style="display:none"><h2>查看订单</h2>
<ul><li><h3>Test Product</h3><span>数量：1</span></li></ul>
<div><span>订单总额</span><strong>RMB 9,999.00</strong></div>
<button data-autom="continue-button-placeOrder" onclick="sessionStorage.setItem('apwSmokeResult',
 JSON.stringify({name:document.querySelector('[name=pickupFullName]').value,
 phone:document.querySelector('[name=pickupPhone]').value,
 email:document.querySelector('[name=pickupEmail]').value,
 alipay:document.getElementById('alipay').checked})); location.href='/alipay'">
现在下订单</button></section>''',
    "/alipay": f'''<!doctype html><meta charset="utf-8"><title>测试支付宝</title>
<div data-url="{QR}" style="width:120px;height:120px">测试付款码</div>''',
    "/alipay/frame": f'''<!doctype html><meta charset="utf-8"><title>测试支付宝内嵌帧</title>
<div data-url="{QR}" style="width:120px;height:120px">测试付款码</div>''',
}


class PageHandler(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_GET(self):
        path = self.path.split("?", 1)[0]
        PAGE_HITS.append(path)
        body = PAGES.get(path, "<h1>missing</h1>").encode()
        self.send_response(200 if path in PAGES else 404)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


class BridgeHandler(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def send_cors(self):
        self.send_header("Access-Control-Allow-Origin", self.headers.get("Origin", "*"))
        self.send_header("Access-Control-Allow-Methods", "POST, OPTIONS")
        self.send_header("Access-Control-Allow-Headers", "authorization, content-type")
        self.send_header("Access-Control-Allow-Private-Network", "true")
        self.send_header("Content-Type", "application/json")

    def do_OPTIONS(self):
        self.send_response(204)
        self.send_cors()
        self.end_headers()

    def do_POST(self):
        global LATEST_PAYMENT
        raw = self.rfile.read(int(self.headers.get("Content-Length", "0")))
        payload = json.loads(raw)
        duplicate_order_stage = self.path == "/task-status" and payload.get("stage") == "orderSubmitted" and any(
            path == self.path and previous == payload for path, previous in BRIDGE_REQUESTS
        )
        first_frame_qr = TEST_PAYMENT_MODE == "iframe-retry" and self.path == "/payment-link" and (
            payload.get("kind") == "qr" and not any(
                path == "/payment-link" and previous.get("kind") == "qr"
                for path, previous in BRIDGE_REQUESTS
            )
        )
        BRIDGE_REQUESTS.append((self.path, payload))
        body = b'{"ok":true}'
        status = 401 if self.headers.get("Authorization") != "Bearer " + TOKEN else (
            503 if first_frame_qr else 409 if duplicate_order_stage else 200
        )
        BRIDGE_RESPONSES.append((self.path, payload, status))
        if self.path == "/payment-link" and status == 200 and not (
            LATEST_PAYMENT and LATEST_PAYMENT["taskId"] == payload["taskId"] and
            LATEST_PAYMENT["kind"] == "qr" and payload["kind"] == "cashier"
        ):
            LATEST_PAYMENT = payload
        self.send_response(status)
        self.send_cors()
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


class CDP:
    def __init__(self, websocket):
        self.websocket = websocket
        self.next_id = 0
        self.pending = {}

    async def start(self):
        asyncio.create_task(self.read())

    async def read(self):
        async for raw in self.websocket:
            message = json.loads(raw)
            if "id" in message:
                future = self.pending.pop(message["id"], None)
                if future and not future.done():
                    future.set_result(message)

    async def call(self, method, params=None):
        self.next_id += 1
        future = asyncio.get_running_loop().create_future()
        self.pending[self.next_id] = future
        await self.websocket.send(json.dumps({"id": self.next_id, "method": method, "params": params or {}}))
        message = await asyncio.wait_for(future, 10)
        if "error" in message:
            raise RuntimeError((method, message["error"]))
        return message.get("result", {})

    async def evaluate(self, expression):
        result = await self.call("Runtime.evaluate", {
            "expression": expression, "awaitPromise": True, "returnByValue": True,
        })
        if "exceptionDetails" in result:
            raise RuntimeError(result["exceptionDetails"])
        return result.get("result", {}).get("value")


def chrome_binary():
    configured = os.environ.get("APW_CHROME_PATH")
    if configured:
        binary = Path(configured)
        if binary.is_file():
            return binary
        raise FileNotFoundError(f"APW_CHROME_PATH does not exist: {binary}")
    for command in ("google-chrome", "chromium", "chromium-browser"):
        found = shutil.which(command)
        if found:
            return Path(found)
    local = Path.home() / "Library/Application Support/io.github.enchigo.apple-pickup-watcher/browser-bot/versions"
    candidates = sorted(local.glob("*/chrome-mac-arm64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing"))
    if candidates:
        return candidates[-1]
    raise FileNotFoundError("Chrome for Testing not found; set APW_CHROME_PATH")


def replace_once(source, old, new):
    if source.count(old) != 1:
        raise RuntimeError(f"extension test adaptation no longer matches: {old[:40]}")
    return source.replace(old, new)


def test_extension_copy(destination, bridge_port, disable_tabs_handoff=False):
    shutil.copytree(ROOT / "browser-extension", destination)
    manifest_path = destination / "manifest.json"
    manifest = json.loads(manifest_path.read_text())
    for item in manifest["content_scripts"]:
        path = "/alipay*" if item["js"] == ["alipay-capture.js"] else "/shop/*"
        item["matches"].append("http://127.0.0.1" + path)
    manifest["host_permissions"].append(f"http://127.0.0.1:{bridge_port}/*")
    manifest_path.write_text(json.dumps(manifest))

    path = destination / "background.js"
    background = path.read_text()
    background = background.replace("43849", str(bridge_port))
    marker = "function applePage(url) {\n  try {\n    const parsed = new URL(url);"
    background = replace_once(background, marker, marker + "\n    if (parsed.protocol === 'http:' && parsed.hostname === '127.0.0.1' && parsed.pathname.startsWith('/shop/')) return true;")
    marker = "function alipayPage(url) {\n  try {\n    const parsed = new URL(url);"
    background = replace_once(background, marker, marker + "\n    if (parsed.protocol === 'http:' && parsed.hostname === '127.0.0.1' && parsed.pathname.startsWith('/alipay')) return true;")
    if disable_tabs_handoff:
        # Make the real Chrome navigation event the only route for this case.
        background = replace_once(
            background,
            "chrome.tabs.onCreated.addListener((tab) => {\n",
            "chrome.tabs.onCreated.addListener((tab) => {\n  return;\n",
        )
    path.write_text(background)
    path = destination / "alipay-capture.js"
    capture = path.read_text()
    marker = "const url = new URL(value, location.href);\n      return url.protocol === 'https:'"
    capture = replace_once(
        capture, marker,
        "const url = new URL(value, location.href);\n"
        "      if (url.protocol === 'http:' && url.hostname === '127.0.0.1' && "
        "url.pathname.startsWith('/alipay')) return url.href;\n"
        "      return url.protocol === 'https:'",
    )
    path.write_text(capture)
    path = destination / "checkout-flow.js"
    checkout = path.read_text()
    checkout = replace_once(
        checkout,
        "if (!(host === 'www.apple.com.cn'",
        "if (!(host === '127.0.0.1' || host === 'www.apple.com.cn'",
    )
    path.write_text(checkout)


async def wait_until(predicate, label, timeout=20):
    started = time.monotonic()
    while time.monotonic() - started < timeout:
        if predicate():
            return
        await asyncio.sleep(0.1)
    raise TimeoutError(f"timed out waiting for {label}; last bridge calls: {BRIDGE_REQUESTS[-10:]}")


async def run_browser(page_port, extension, profile, navigation_mode, payment_mode):
    command = [
        str(chrome_binary()), "--headless=new", "--remote-debugging-port=0",
        "--remote-allow-origins=*", "--disable-background-networking",
        "--disable-popup-blocking",
        "--no-proxy-server", f"--user-data-dir={profile}",
        f"--disable-extensions-except={extension}", f"--load-extension={extension}",
        "--no-first-run", "--no-default-browser-check", "about:blank",
    ]
    process = subprocess.Popen(command, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        marker = profile / "DevToolsActivePort"
        await wait_until(marker.exists, "Chrome DevTools port", timeout=10)
        port = marker.read_text().splitlines()[0]

        def targets():
            with urlopen(f"http://127.0.0.1:{port}/json", timeout=3) as response:
                return json.load(response)

        worker_ready = False
        for _ in range(100):
            for target in targets():
                if target["type"] != "service_worker" or not target["url"].endswith("/background.js"):
                    continue
                try:
                    async with websockets.connect(target["webSocketDebuggerUrl"]) as socket:
                        worker = CDP(socket)
                        await worker.start()
                        await worker.call("Runtime.enable")
                        if await worker.evaluate("chrome.runtime.getManifest().name") != "Apple Pickup Watcher":
                            continue
                        if await worker.evaluate("typeof FLOW_PREFIX !== 'undefined' ? FLOW_PREFIX : ''") != "apwCheckoutFlow:":
                            raise RuntimeError("unexpected extension service worker")
                        profile_data = {
                            "enabled": True,
                            "pickup": {"fullName": "Test Person", "phone": "13800138000", "email": "test@example.invalid"},
                            "delivery": {},
                        }
                        await worker.evaluate("chrome.storage.local.set(" + json.dumps({
                            "apwBridgeToken": TOKEN,
                            "apwCheckoutProfile": profile_data,
                            "apwOrderConfig": {"enabled": True, "maxCny": 9999},
                        }) + ")")
                        worker_ready = True
                        break
                except (ConnectionError, OSError):
                    continue
            if worker_ready:
                break
            await asyncio.sleep(0.1)
        if not worker_ready:
            raise RuntimeError("Apple Pickup Watcher extension worker did not load")

        page = next(target for target in targets() if target["type"] == "page" and target["url"] == "about:blank")
        async with websockets.connect(page["webSocketDebuggerUrl"]) as socket:
            browser = CDP(socket)
            await browser.start()
            await browser.call("Page.enable")
            await browser.call("Runtime.enable")
            fragment = urlencode({
                "apw-auto-add": PART, "apw-store": STORE, "apw-task": TASK,
                "apw-product": "Test Product",
            })
            await browser.call("Page.navigate", {
                "url": f"http://127.0.0.1:{page_port}/shop/pdpAddToBag/{PART}#{fragment}",
            })
            expected_links = 2 if payment_mode in ("late-qr", "iframe-retry") else 1
            await wait_until(
                lambda: sum(path == "/payment-link" for path, _ in BRIDGE_REQUESTS) >= expected_links,
                "local payment link after automatic fixture order",
            )
            if payment_mode == "iframe-retry":
                await wait_until(
                    lambda: any(path == "/payment-link" and payload["kind"] == "cashier"
                                for path, payload in BRIDGE_REQUESTS),
                    "top-frame cashier fallback after iframe QR",
                    timeout=10,
                )
            contact = await browser.evaluate("JSON.parse(sessionStorage.getItem('apwSmokeResult'))")
            assert contact == {
                "name": "Test Person", "phone": "13800138000",
                "email": "test@example.invalid", "alipay": True,
            }, contact
            expected_pages = [
                "/shop/pdpAddToBag/MJYE4CH/A", "/shop/buy-iphone/test", "/shop/bag",
                "/shop/signIn", "/shop/checkout",
            ]
            assert [path for path in PAGE_HITS if path in PAGES][:5] == expected_pages, PAGE_HITS
            assert "/alipay" in PAGE_HITS, PAGE_HITS
            if payment_mode == "iframe-retry":
                assert "/alipay/frame" in PAGE_HITS, PAGE_HITS
            payments = [payload for path, payload in BRIDGE_REQUESTS if path == "/payment-link"]
            expected_kinds = ["qr", "qr", "cashier"] if payment_mode == "iframe-retry" else (
                ["cashier", "qr"] if payment_mode == "late-qr" else [
                "cashier" if payment_mode == "cashier" else "qr"
                ]
            )
            assert [payment["kind"] for payment in payments] == expected_kinds, payments
            if payment_mode == "iframe-retry":
                responses = [status for path, _, status in BRIDGE_RESPONSES if path == "/payment-link"]
                assert responses == [503, 200, 200], responses
            for payment in payments:
                expected_url = QR if payment["kind"] == "qr" else f"http://127.0.0.1:{page_port}/alipay"
                assert payment == {
                    "url": expected_url, "kind": payment["kind"], "partNumber": PART,
                    "storeNumber": STORE, "taskId": TASK,
                }, payment
            assert LATEST_PAYMENT["kind"] == ("cashier" if payment_mode == "cashier" else "qr"), LATEST_PAYMENT
            stages = [payload["stage"] for path, payload in BRIDGE_REQUESTS if path == "/task-status"]
            assert stages == ["bag", "checkout", "guest", "profile", "payment", "review"] + [
                "orderSubmitted"
            ] * len(payments), stages
            print(f"Chrome extension smoke passed ({navigation_mode}, {payment_mode}): product → bag → guest checkout → Profile → capped fixture order → Alipay link bridge")
    finally:
        process.terminate()
        try:
            process.wait(timeout=3)
        except subprocess.TimeoutExpired:
            process.kill()


async def main():
    global LATEST_PAYMENT, TEST_PAYMENT_MODE
    pages = ThreadingHTTPServer(("127.0.0.1", 0), PageHandler)
    bridge = ThreadingHTTPServer(("127.0.0.1", 0), BridgeHandler)
    threading.Thread(target=pages.serve_forever, daemon=True).start()
    threading.Thread(target=bridge.serve_forever, daemon=True).start()
    try:
        checkout = PAGES["/shop/checkout"]
        for navigation_mode, payment_mode in (
            ("same tab", "qr"), ("new tab", "qr"), ("noopener tab", "qr"),
            ("same tab", "cashier"), ("new tab", "cashier"), ("noopener tab", "cashier"),
            ("same tab", "late-qr"),
            ("same tab", "iframe-retry"),
        ):
            PAGE_HITS.clear()
            BRIDGE_REQUESTS.clear()
            BRIDGE_RESPONSES.clear()
            LATEST_PAYMENT = None
            TEST_PAYMENT_MODE = payment_mode
            PAGES["/alipay"] = {
                "qr": f'''<!doctype html><meta charset="utf-8"><title>测试支付宝</title>
<div data-url="{QR}" style="width:120px;height:120px">测试付款码</div>''',
                "cashier": '<!doctype html><meta charset="utf-8"><title>测试支付宝收银台</title>',
                "late-qr": f'''<!doctype html><meta charset="utf-8"><title>测试支付宝收银台</title>
<script>setTimeout(() => {{ document.body.innerHTML =
'<div data-url="{QR}" style="width:120px;height:120px">测试付款码</div>' }}, 6500)</script>''',
                "iframe-retry": '''<!doctype html><meta charset="utf-8"><title>测试支付宝收银台</title>
<iframe src="/alipay/frame" style="width:150px;height:150px"></iframe>''',
            }[payment_mode]
            PAGES["/shop/checkout"] = checkout.replace(
                "location.href='/alipay'",
                {
                    "same tab": "location.href='/alipay'",
                    "new tab": "window.open('/alipay', '_blank')",
                    "noopener tab": "window.open('/alipay', '_blank', 'noopener')",
                }[navigation_mode],
            )
            with tempfile.TemporaryDirectory(prefix="apw-browser-smoke-") as directory:
                base = Path(directory)
                extension = base / "extension"
                profile = base / "profile"
                test_extension_copy(
                    extension, bridge.server_address[1],
                    disable_tabs_handoff=navigation_mode == "noopener tab",
                )
                await run_browser(pages.server_address[1], extension, profile, navigation_mode, payment_mode)
    finally:
        pages.shutdown()
        bridge.shutdown()
        pages.server_close()
        bridge.server_close()


if __name__ == "__main__":
    asyncio.run(main())
