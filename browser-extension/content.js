// 在 Apple 官方商品页完成必需的选配，再通过官方按钮加入当前浏览器的购物袋。
(() => {
  if (window.top !== window || !location.pathname.includes('/shop/')) return;

  const KEY = 'apwAutoAddIntent';
  const PART = /^[A-Z0-9]{4,20}\/[A-Z]$/;
  const STORE = /^[A-Z0-9]{2,10}$/;
  const TASK = /^[a-f0-9]{32}$/;
  const WAIT_LIMIT_MS = 4 * 60 * 60 * 1000;
  const SUBMIT_LIMIT_MS = 90 * 1000;
  const params = new URLSearchParams(location.hash.slice(1));
  let intent;

  if (params.has('apw-auto-add')) {
    const part = (params.get('apw-auto-add') || '').toUpperCase();
    const storeNumber = (params.get('apw-store') || '').toUpperCase();
    const taskId = params.get('apw-task');
    const productName = params.get('apw-product');
    const province = params.get('apw-province');
    const city = params.get('apw-city');
    const district = params.get('apw-district');
    if (!PART.test(part) || !STORE.test(storeNumber) || !TASK.test(taskId || '')) return;
    intent = { part, storeNumber, taskId, productName, province, city, district, phase: 'choosing', startedAt: Date.now() };
    sessionStorage.setItem(KEY, JSON.stringify(intent));
    // 返回商品页或刷新时不能再次启动一次加车。
    history.replaceState(history.state, '', location.pathname + location.search);
  } else {
    try { intent = JSON.parse(sessionStorage.getItem(KEY) || 'null'); } catch { /* 忽略无效数据 */ }
  }
  const expired = () => {
    if (!Number.isFinite(intent?.startedAt) || intent.startedAt > Date.now()) return true;
    if (intent.phase === 'submitting' || intent.phase === 'failed') {
      return !Number.isFinite(intent.submittedAt) || intent.submittedAt > Date.now() ||
        Date.now() - intent.submittedAt > SUBMIT_LIMIT_MS;
    }
    return Date.now() - intent.startedAt > WAIT_LIMIT_MS;
  };
  if (!intent || !PART.test(intent.part) || !STORE.test(intent.storeNumber) || !TASK.test(intent.taskId || '') || expired()) {
    sessionStorage.removeItem(KEY);
    return;
  }

  function notice(message, failed = false) {
    let box = document.getElementById('apw-cart-notice');
    if (!box) {
      box = document.createElement('div');
      box.id = 'apw-cart-notice';
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

  function stop(message) {
    clearInterval(timer);
    sessionStorage.removeItem(KEY);
    notice(message, true);
    void chrome.runtime.sendMessage({
      type: 'apw-cart-timeout', partNumber: intent.part,
      storeNumber: intent.storeNumber, taskId: intent.taskId,
    }).catch(() => {});
  }

  function finish() {
    clearInterval(timer);
    sessionStorage.removeItem(KEY);
    notice('已加入购物袋，正在核对商品并进入结账…');
    const prefix = location.pathname.split('/shop/')[0];
    const bag = `${location.origin}${prefix}/shop/bag`;
    const checkout = new URLSearchParams({
      'apw-checkout': '1', 'apw-store': intent.storeNumber, 'apw-part': intent.part,
    });
    if (intent.taskId) checkout.set('apw-task', intent.taskId);
    if (intent.productName) checkout.set('apw-product', intent.productName);
    for (const key of ['province', 'city', 'district']) {
      if (intent[key]) checkout.set(`apw-${key}`, intent[key]);
    }
    location.assign(`${bag}#${checkout}`);
  }

  function visible(element) {
    return element && !element.disabled && element.getAttribute('aria-disabled') !== 'true' &&
      element.getClientRects().length > 0;
  }

  let checkingTask = false;

  function tick() {
    if (expired()) {
      stop('自动加车超时。请在 Apple 页面核对后手动操作。');
      return;
    }

    // Apple 接受官方加车动作后会进入配件推荐页；product 参数可确认本次 SKU。
    const query = new URLSearchParams(location.search);
    if (intent.phase === 'submitting' && query.get('add-to-cart') === 'add-to-cart' &&
        /^Page Not Found(?:\s*-|$)/i.test(document.title)) {
      // Apple 的加车请求可能直接返回错误页（实测 HTTP 541），不能继续等待配件页。
      intent.phase = 'failed';
      sessionStorage.setItem(KEY, JSON.stringify(intent));
      clearInterval(timer);
      notice('Apple 未确认加车请求。请检查购物袋；本轮已停止，避免重复加车。', true);
      return;
    }
    if (intent.phase === 'submitting' && query.get('step') === 'attach' &&
        query.get('product')?.toUpperCase() === intent.part) {
      finish();
      return;
    }
    // 部分商品可能直接跳到购物袋。只在目标商品确实出现时继续。
    if (intent.phase === 'submitting' && /\/shop\/bag\/?$/.test(location.pathname)) {
      const found = Array.from(document.querySelectorAll('[data-autom="bag-item-name"]'))
        .some((link) => new URL(link.href, location.origin).pathname.toUpperCase()
          .endsWith(`/SHOP/PRODUCT/${intent.part}`));
      if (found) finish();
      return;
    }
    if (intent.phase === 'submitting') {
      if (Date.now() - intent.submittedAt > 30000) {
        stop('Apple 未确认商品已加入购物袋。请检查库存并手动操作。');
      }
      return;
    }

    const form = Array.from(document.querySelectorAll('form[data-part-number]'))
      .find((node) => node.getAttribute('data-part-number')?.toUpperCase() === intent.part);
    if (!form) return;
    const add = form.querySelector('[data-autom="add-to-cart"]');
    if (!add) return;
    if (!visible(add)) {
      const noTradeIn = document.querySelector('input[data-autom="choose-noTradeIn"]');
      if (visible(noTradeIn) && !noTradeIn.checked) {
        notice('正在选择不折抵换购…');
        noTradeIn.click();
        return;
      }
      const noCare = document.querySelector('input[data-autom="noapplecare"]');
      if (visible(noCare) && !noCare.checked) {
        notice('正在选择不加 AppleCare+…');
        noCare.click();
        return;
      }
      if (!visible(noTradeIn) && !visible(noCare) && !intent.expanded) {
        // Apple 的预选 SKU 页需点击已选容量，才显示折抵与服务选项。
        const capacity = document.querySelector('input[name="dimensionCapacity"]:checked');
        if (visible(capacity)) {
          intent.expanded = true;
          sessionStorage.setItem(KEY, JSON.stringify(intent));
          capacity.click();
        }
      }
      return;
    }

    if (checkingTask) return;
    checkingTask = true;
    chrome.runtime.sendMessage({
      type: 'apw-cart-active', partNumber: intent.part,
      storeNumber: intent.storeNumber, taskId: intent.taskId,
    }).then((active) => {
      if (!active) {
        notice('正在等待桌面应用确认加车任务；若已停止监控，本页不会加车。');
        return;
      }
      if (intent.phase !== 'choosing' || !visible(add) || add.isConnected === false) return;
      intent.phase = 'submitting';
      intent.submittedAt = Date.now();
      sessionStorage.setItem(KEY, JSON.stringify(intent));
      notice(`正在将 ${intent.part} 加入购物袋…`);
      add.click();
    }).catch(() => {
      notice('暂时无法确认桌面加车任务，正在等待连接恢复。');
    }).finally(() => { checkingTask = false; });
  }

  notice(`正在配置 ${intent.part}…`);
  const timer = setInterval(tick, 250);
  tick();
})();
