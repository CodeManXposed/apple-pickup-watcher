// 仅在本扩展成功加车后的同一 Chrome 标签页推进结账。
(() => {
  if (window.top !== window) return;
  const BAG_PATH = /\/shop\/bag\/?$/;
  const CHECKOUT_PATH = /\/shop\/checkout(?:\/|$)/;
  const isBag = BAG_PATH.test(location.pathname);
  const params = new URLSearchParams(location.hash.slice(1));
  const markedBag = isBag && params.get('apw-checkout') === '1';
  const storeNumber = params.get('apw-store');
  const partNumber = (params.get('apw-part') || '').toUpperCase();
  const taskId = params.get('apw-task');
  const ORDER_CONFIG_KEY = 'apwOrderConfig';
  const GUEST_TEXT = /^(?:(?:以访客身份|以游客身份)(?:继续|结[账帐]|购买)|(?:继续)?以访客身份结[账帐]|访客结[账帐]|访客继续|continue as (?:a )?guest|guest checkout)$/i;
  let done = false;
  let scheduled = false;
  const clickedSteps = new Set();
  let orderNoted = false;
  let begun = false;
  const requestedContacts = new WeakSet();
  const reportedStages = new Set();
  const startedAt = Date.now();

  function shown(element) {
    return !element.closest('[aria-hidden="true"]') && element.getClientRects().length > 0;
  }

  function visible(element) {
    return !element.disabled && element.getAttribute('aria-disabled') !== 'true' && shown(element);
  }

  function firstShown(selector) {
    return Array.from(document.querySelectorAll(selector)).find(shown);
  }

  function notice(message, failed = false) {
    let box = document.getElementById('apw-checkout-notice');
    if (!box) {
      box = document.createElement('div');
      box.id = 'apw-checkout-notice';
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

  function reportStage(stage) {
    if (reportedStages.has(stage)) return;
    reportedStages.add(stage);
    void chrome.runtime.sendMessage({ type: 'apw-task-stage', stage }).catch(() => {});
  }

  function checkoutButton() {
    // Apple 当前购物袋脚本给普通结账按钮标了 data-autom="checkout"；
    // Apple Pay 与分期弹窗的按钮具有不同标记，不能误点。
    const official = Array.from(document.querySelectorAll('[data-autom="checkout"]'))
      .filter((element) => element.closest('.rs-bag-checkout-mainbutton') && visible(element));
    if (official.length) return official[0];
    return Array.from(document.querySelectorAll('button, a, [role="button"]')).find((element) =>
      visible(element) && /^(?:结[账帐]|立即结[账帐]|去结[账帐]|check out(?: now)?|proceed to checkout)$/i
        .test((element.getAttribute('aria-label') || element.textContent || '').trim()));
  }

  function bagStatus() {
    if (!/^[A-Z0-9]{4,20}\/[A-Z]$/.test(partNumber)) return 'invalid';
    const items = Array.from(document.querySelectorAll('[data-autom="bag-item-name"]'));
    if (!items.length) return 'loading';
    if (items.length !== 1) return 'other-items';
    const item = items[0];
    try {
      if (!new URL(item.href, location.origin).pathname.toUpperCase()
        .endsWith(`/SHOP/PRODUCT/${partNumber}`)) return 'other-items';
    } catch { return 'invalid'; }
    const quantity = item.closest('.rs-iteminfo-details')?.querySelector('[data-autom="item-quantity-dropdown"]');
    return quantity?.value === '1' ? 'ready' : 'quantity';
  }

  function guestButton() {
    return Array.from(document.querySelectorAll('button, a, [role="button"]')).find((element) =>
      visible(element) && GUEST_TEXT.test(
        (element.getAttribute('aria-label') || element.textContent || '').trim()));
  }

  function clickOnce(key, element, message) {
    if (!element || !visible(element) || clickedSteps.has(key)) return false;
    clickedSteps.add(key);
    notice(message);
    element.click();
    return true;
  }

  function contactReady(button) {
    if (!requestedContacts.has(button)) {
      requestedContacts.add(button);
      document.documentElement.setAttribute('data-apw-profile-fill-ready', 'pending');
      window.dispatchEvent(new Event('apw-request-profile-fill'));
    }
    if (document.documentElement.getAttribute('data-apw-profile-fill-ready') !== '1') {
      notice('请先在扩展中保存并启用结账资料，或在 Apple 页面手动继续。');
      return false;
    }
    const required = Array.from(document.querySelectorAll('input[required], textarea[required], select[required], input[aria-required="true"], textarea[aria-required="true"], select[aria-required="true"]'))
      .filter(visible);
    const missing = required.some((field) => {
      if (field.type === 'checkbox') return !field.checked;
      if (field.type === 'radio') {
        return !Array.from(document.querySelectorAll('input[type="radio"]'))
          .some((radio) => radio.name === field.name && radio.checked);
      }
      return !String(field.value || '').trim();
    });
    if (missing) notice('结账资料仍有必填项未完成，请在 Apple 页面补全。');
    return !missing;
  }

  function contactFingerprint() {
    return Array.from(document.querySelectorAll('input, textarea, select'))
      .filter(visible).map((field) => `${field.name}:${field.value}:${field.checked}`).join('|');
  }

  function selectedPickupStore(number) {
    const candidates = Array.from(document.querySelectorAll('input[type="radio"].form-selector-input'));
    return candidates.find((input) => input.value === number && visible(input));
  }

  function alipayRadio() {
    const candidates = Array.from(document.querySelectorAll('input[type="radio"]')).filter(visible);
    const byIdentity = candidates.filter((input) =>
      /ali.?pay/i.test(`${input.id} ${input.name} ${input.value}`));
    if (byIdentity.length === 1) return byIdentity[0];
    const byLabel = candidates.filter((input) => {
      const label = input.labels?.[0]?.textContent || input.closest('label')?.textContent || '';
      return /支付宝|ali\s?pay/i.test(label);
    });
    return byLabel.length === 1 ? byLabel[0] : null;
  }

  function amountCents(text) {
    const amounts = Array.from(text.matchAll(/(?:[¥￥]|RMB\s*|CNY\s*|人民币\s*)([0-9][0-9,]*(?:\.[0-9]{1,2})?)/gi));
    if (amounts.length !== 1) return null;
    const raw = amounts[0][1].replaceAll(',', '');
    if (!/^\d{1,8}(?:\.\d{1,2})?$/.test(raw)) return null;
    const [whole, fraction = ''] = raw.split('.');
    const cents = Number(whole) * 100 + Number(fraction.padEnd(2, '0'));
    return Number.isSafeInteger(cents) && cents > 0 ? cents : null;
  }

  function reviewTotalCents() {
    const label = /^(?:订单总计|订单总额|应付总额|应付金额|总计|Order Total|Total)$/i;
    const prices = new Set();
    for (const node of document.querySelectorAll('span, strong, b, dt, th, p, div')) {
      if (node.children.length || !shown(node) || !label.test((node.textContent || '').trim())) continue;
      const row = node.parentElement;
      const text = row?.textContent?.trim() || '';
      if (text.length > 120) continue;
      const cents = amountCents(text);
      if (cents !== null) prices.add(cents);
    }
    return prices.size === 1 ? prices.values().next().value : null;
  }

  async function placeOrderIfAuthorized(button, flow) {
    const { [ORDER_CONFIG_KEY]: config } = await chrome.storage.local.get(ORDER_CONFIG_KEY);
    if (!config?.enabled) {
      notice('请核对商品、门店、金额及资料；点击“现在下订单”后 Apple 才会跳转支付宝。');
      return;
    }
    const maxCny = Number(config.maxCny);
    if (!Number.isSafeInteger(maxCny) || maxCny < 1 || maxCny > 1_000_000 ||
        !flow.pickupConfirmed || !flow.alipayConfirmed || !/^[a-f0-9]{32}$/.test(flow.taskId || '')) {
      notice('自动下单条件未通过：请确认监控门店、支付宝、连接码和金额上限，然后手动核对。', true);
      return;
    }
    const cents = reviewTotalCents();
    if (cents === null) {
      notice('无法唯一识别 Apple 订单总额，已停止自动下单。请手动核对。', true);
      return;
    }
    if (cents > maxCny * 100) {
      notice(`订单总额 ¥${(cents / 100).toFixed(2)} 超过 ¥${maxCny} 上限，已停止自动下单。`, true);
      return;
    }
    if (clickedSteps.has('place-order') || !visible(button)) return;
    const updated = await chrome.runtime.sendMessage({ type: 'apw-checkout-step', phase: 'order-submitted' });
    if (!updated || !visible(button)) {
      notice('无法锁定本次自动下单任务，请手动核对。', true);
      return;
    }
    clickedSteps.add('place-order');
    orderNoted = true;
    notice(`订单总额 ¥${(cents / 100).toFixed(2)}，正在提交 Apple 订单并等待支付宝链接…`);
    button.click();
  }

  async function checkoutStep(flow) {
    if (!CHECKOUT_PATH.test(location.pathname)) return;
    const privacy = Array.from(document.querySelectorAll('[role="dialog"]')).find((dialog) =>
      visible(dialog) && /隐私政策|数据隐私|privacy policy/i.test(dialog.textContent || ''));
    if (privacy) {
      notice('请先在 Apple 页面自行阅读并处理隐私同意书。');
      return;
    }
    const fulfillment = firstShown('[data-autom="fulfillment-continue-button"]');
    if (fulfillment) {
      const store = selectedPickupStore(flow.storeNumber);
      if (!store) {
        notice(`请在 Apple 页面选择监控门店 ${flow.storeNumber}，再继续结账。`);
        return;
      }
      if (!store.checked) {
        clickOnce('select-store', store, `正在选择监控门店 ${flow.storeNumber}…`);
        return;
      }
      const confirmed = await chrome.runtime.sendMessage({ type: 'apw-confirm-store', storeNumber: flow.storeNumber });
      if (!confirmed) {
        notice('无法确认本次取货门店，请手动核对。', true);
        return;
      }
      clickOnce('fulfillment', fulfillment, '正在继续到取货资料…');
      return;
    }
    const pickupContinue = firstShown('.rs-pickup-button button');
    if (pickupContinue) {
      reportStage('profile');
      if (contactReady(pickupContinue)) clickOnce(`pickup-contact:${contactFingerprint()}`, pickupContinue, '正在继续到付款方式…');
      return;
    }
    const shippingContinue = firstShown('[data-autom="shipping-continue-button"]');
    if (shippingContinue) {
      reportStage('profile');
      if (contactReady(shippingContinue)) clickOnce(`shipping:${contactFingerprint()}`, shippingContinue, '正在继续到付款方式…');
      return;
    }
    const paymentContinue = firstShown('[data-autom="continue-button-review"]');
    if (paymentContinue) {
      reportStage('payment');
      const alipay = alipayRadio();
      if (!alipay) {
        notice('未能确认支付宝选项，请在 Apple 页面自行选择付款方式。');
        return;
      }
      if (!alipay.checked) {
        clickOnce('select-alipay', alipay, '正在选择支付宝…');
        return;
      }
      const confirmed = await chrome.runtime.sendMessage({ type: 'apw-confirm-alipay' });
      if (!confirmed) {
        notice('无法确认本次支付宝付款方式，请手动核对。', true);
        return;
      }
      clickOnce('billing', paymentContinue, '正在进入确认订单页…');
      return;
    }
    const placeOrder = firstShown('[data-autom="continue-button-placeOrder"]');
    if (placeOrder) {
      reportStage('review');
      await placeOrderIfAuthorized(placeOrder, flow);
    }
  }

  async function act() {
    if (done) return;
    if (markedBag) {
      const status = bagStatus();
      if (status !== 'ready') {
        if (status !== 'loading' || Date.now() - startedAt > 30000) {
          done = true;
          notice(`购物袋须仅包含一件 ${partNumber}；请检查商品和数量后手动结账。`, true);
        }
        return;
      }
    }
    if (markedBag && !begun) {
      const started = await chrome.runtime.sendMessage({ type: 'apw-begin-checkout', storeNumber, partNumber, taskId });
      if (!started) { done = true; return; }
      begun = true;
      reportStage('bag');
      history.replaceState(history.state, '', location.pathname + location.search);
      notice('已确认目标商品，正在查找结账按钮…');
    }
    const flow = await chrome.runtime.sendMessage({ type: 'apw-get-checkout' });
    if (!flow) { done = true; return; }
    if (isBag && markedBag && flow.phase === 'bag') {
      const button = checkoutButton();
      if (button) {
        const updated = await chrome.runtime.sendMessage({ type: 'apw-checkout-step', phase: 'bag-clicked' });
        if (!updated) return;
        reportStage('checkout');
        done = true;
        notice('正在进入 Apple 结账…');
        button.click();
        return;
      }
      if (Date.now() - startedAt > 30000) {
        done = true;
        notice('未找到可用的结账按钮。请检查购物袋和取货选项，然后手动结账。', true);
      }
      return;
    }
    if (!isBag && flow.phase === 'bag-clicked') {
      const button = guestButton();
      if (button) {
        const updated = await chrome.runtime.sendMessage({ type: 'apw-checkout-step', phase: 'guest-clicked' });
        if (!updated) return;
        reportStage('guest');
        done = true;
        notice('正在以访客身份结账；若 Apple 显示隐私同意书，请自行阅读并决定。');
        button.click();
      }
      return;
    }
    if (flow.phase === 'guest-clicked') await checkoutStep(flow);
  }

  function schedule() {
    if (done || scheduled) return;
    scheduled = true;
    setTimeout(() => { scheduled = false; void act(); }, 120);
  }

  async function start() {
    if (CHECKOUT_PATH.test(location.pathname)) {
      document.addEventListener('click', (event) => {
        if (orderNoted || !event.target?.closest?.('[data-autom="continue-button-placeOrder"]')) return;
        orderNoted = true;
        void chrome.runtime.sendMessage({ type: 'apw-checkout-step', phase: 'order-submitted' });
      }, true);
    }
    await act();
    if (done) return;
    new MutationObserver((records) => {
      if (records.some((record) => !record.target.closest?.('#apw-checkout-notice'))) schedule();
    }).observe(document.documentElement, {
      childList: true, subtree: true, attributes: true,
      attributeFilter: ['class', 'style', 'hidden', 'aria-hidden', 'disabled', 'aria-disabled', 'data-apw-profile-fill-ready'],
    });
    document.addEventListener('input', schedule);
    document.addEventListener('change', schedule);
    if (markedBag) setTimeout(schedule, 30000);
  }
  void start();
})();
