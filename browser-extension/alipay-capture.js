// 仅捕获这次 Apple 访客结账进入的支付宝页面。网页无法读取扩展配对码。
(() => {
  const topFrame = window.top === window;
  let active = false;
  let sentQr = '';
  let sentCashier = '';
  let scheduled = false;
  let scanning = false;
  let lastPixelScan = 0;
  const startedAt = Date.now();

  function alipayUrl(value) {
    try {
      const url = new URL(value, location.href);
      return url.protocol === 'https:' &&
        (url.hostname === 'alipay.com' || url.hostname.endsWith('.alipay.com'))
        ? url.href : null;
    } catch {
      return null;
    }
  }

  function qrTarget(raw) {
    if (!raw || raw.length > 8192) return null;
    const direct = alipayUrl(raw);
    if (direct && new URL(direct).hostname === 'qr.alipay.com') return direct;
    // 部分收银台用二维码图片服务，目标地址在名为 url / content / qr 的参数里。
    try {
      const image = new URL(raw, location.href);
      for (const [key, value] of image.searchParams) {
        if (!/^(?:url|content|qr|qrcode|qrurl)$/i.test(key)) continue;
        const target = alipayUrl(value);
        if (target && new URL(target).hostname === 'qr.alipay.com') return target;
      }
    } catch { /* 没有可解析的链接。 */ }
    return null;
  }

  function visible(node) {
    return node.getClientRects().length > 0 && !node.closest('[aria-hidden="true"]');
  }

  function findQr() {
    for (const node of document.querySelectorAll('a[href], img[src], [data-url], [data-qr], [data-qrcode]')) {
      if (!visible(node)) continue;
      for (const name of ['href', 'src', 'data-url', 'data-qr', 'data-qrcode']) {
        const target = qrTarget(node.getAttribute(name));
        if (target) return target;
      }
    }
    return null;
  }

  async function findQrInPixels() {
    if (typeof BarcodeDetector !== 'function' || Date.now() - lastPixelScan < 1000) return null;
    lastPixelScan = Date.now();
    try {
      const detector = new BarcodeDetector({ formats: ['qr_code'] });
      for (const node of Array.from(document.querySelectorAll('canvas, img')).filter(visible).slice(0, 8)) {
        for (const code of await detector.detect(node)) {
          const target = qrTarget(code.rawValue);
          if (target) return target;
        }
      }
    } catch { /* 不支持像素解码时仍可转发收银台地址。 */ }
    return null;
  }

  function notice(message, failed = false) {
    let box = document.getElementById('apw-alipay-notice');
    if (!box) {
      box = document.createElement('div');
      box.id = 'apw-alipay-notice';
      box.setAttribute('role', 'status');
      Object.assign(box.style, {
        position: 'fixed', right: '20px', bottom: '20px', zIndex: '2147483647',
        maxWidth: '340px', padding: '14px 18px', borderRadius: '12px',
        background: '#1d1d1f', color: '#fff', font: '14px/1.5 system-ui, sans-serif',
        boxShadow: '0 4px 24px #0004',
      });
      document.body.appendChild(box);
    }
    box.textContent = message;
    box.style.background = failed ? '#8b1e1e' : '#1d1d1f';
  }

  async function send(kind, url) {
    const result = await chrome.runtime.sendMessage({ type: 'apw-payment-link', kind, url });
    if (result?.ok) {
      notice(kind === 'qr' ? '已将支付宝二维码链接发送到桌面应用。' : '已将支付宝收银台链接发送到桌面应用，等待二维码。');
    } else if (result?.reason === 'missing-pairing-code') {
      notice('请在扩展设置中填写桌面应用的扩展连接码。', true);
    } else if (result?.reason !== 'no-active-checkout') {
      notice('支付宝链接未送达桌面应用，请确认应用正在运行及扩展连接码正确。', true);
    }
    return Boolean(result?.ok);
  }

  async function scan() {
    if (!active || scanning) return;
    scanning = true;
    try {
      const qr = findQr() || await findQrInPixels();
      if (qr && qr !== sentQr) {
        if (await send('qr', qr)) sentQr = qr;
        return;
      }
      if (topFrame && !sentQr && Date.now() - startedAt >= 5000) {
        const cashier = alipayUrl(location.href);
        if (cashier && cashier !== sentCashier && await send('cashier', cashier)) {
          sentCashier = cashier;
        }
      }
    } finally {
      scanning = false;
    }
  }

  function schedule() {
    if (scheduled) return;
    scheduled = true;
    setTimeout(() => { scheduled = false; void scan(); }, 150);
  }

  async function start() {
    // 新标签页创建时，后台复制结账会话可能稍晚于内容脚本启动。
    for (let attempt = 0; attempt < 10; attempt++) {
      const flow = await chrome.runtime.sendMessage({ type: 'apw-get-checkout' });
      if (flow?.phase === 'guest-clicked') { active = true; break; }
      await new Promise((resolve) => setTimeout(resolve, 300));
    }
    if (!active) return;
    await scan();
    new MutationObserver(schedule).observe(document.documentElement, { childList: true, subtree: true, attributes: true });
    setTimeout(schedule, 5000);
    if (topFrame) setInterval(schedule, 2000);
  }
  void start();
})();
