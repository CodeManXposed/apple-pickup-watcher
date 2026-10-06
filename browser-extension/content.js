// 在 Apple 官方商品页完成必需的选配，再通过官方按钮加入当前浏览器的购物袋。
(() => {
  if (window.top !== window || !location.pathname.includes('/shop/')) return;

  const KEY = 'apwAutoAddIntent';
  const PART = /^[A-Z0-9]{4,20}\/[A-Z]$/;
  const STORE = /^[A-Z0-9]{2,10}$/;
  const params = new URLSearchParams(location.hash.slice(1));
  let intent;

  if (params.has('apw-auto-add')) {
    const part = (params.get('apw-auto-add') || '').toUpperCase();
    const storeNumber = (params.get('apw-store') || '').toUpperCase();
    if (!PART.test(part) || !STORE.test(storeNumber)) return;
    intent = { part, storeNumber, phase: 'choosing', startedAt: Date.now() };
    sessionStorage.setItem(KEY, JSON.stringify(intent));
    // 返回商品页或刷新时不能再次启动一次加车。
    history.replaceState(history.state, '', location.pathname + location.search);
  } else {
    try { intent = JSON.parse(sessionStorage.getItem(KEY) || 'null'); } catch { /* 忽略无效数据 */ }
  }
  if (!intent || !PART.test(intent.part) || !STORE.test(intent.storeNumber) ||
      Date.now() - intent.startedAt > 60000) {
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
  }

  function finish() {
    clearInterval(timer);
    sessionStorage.removeItem(KEY);
    notice('已加入购物袋，正在核对商品并进入结账…');
    const prefix = location.pathname.split('/shop/')[0];
    const bag = `${location.origin}${prefix}/shop/bag`;
    location.assign(`${bag}#apw-checkout=1&apw-store=${intent.storeNumber}&apw-part=${encodeURIComponent(intent.part)}`);
  }

  function visible(element) {
    return element && !element.disabled && element.getAttribute('aria-disabled') !== 'true' &&
      element.getClientRects().length > 0;
  }

  function tick() {
    if (Date.now() - intent.startedAt > 60000) {
      stop('自动加车超时。请在 Apple 页面核对后手动操作。');
      return;
    }

    // Apple 接受官方加车动作后会进入配件推荐页；product 参数可确认本次 SKU。
    const query = new URLSearchParams(location.search);
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

    intent.phase = 'submitting';
    intent.submittedAt = Date.now();
    sessionStorage.setItem(KEY, JSON.stringify(intent));
    notice(`正在将 ${intent.part} 加入购物袋…`);
    add.click();
  }

  notice(`正在配置 ${intent.part}…`);
  const timer = setInterval(tick, 250);
  tick();
})();
