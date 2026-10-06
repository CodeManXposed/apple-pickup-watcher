import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import vm from 'node:vm';
import test from 'node:test';

const script = readFileSync(resolve('browser-extension/checkout-flow.js'), 'utf8');

function button(label, group = null) {
  return {
    textContent: label, disabled: false, clicks: 0, style: {},
    getAttribute(key) { return key === 'aria-label' ? null : null; },
    getClientRects() { return [1]; },
    closest(selector) { return selector === '.rs-bag-checkout-mainbutton' ? group : null; },
    click() { this.clicks++; },
  };
}

async function page({ pathname, hash = '', official = [], candidates = [], phase = null, selectors = {} }) {
  const location = { pathname, search: '', hash };
  const history = { state: null, replaceState(_state, _title, url) { this.url = url; } };
  const notices = new Map();
  let currentPhase = phase;
  const chrome = {
    runtime: {
      async sendMessage(message) {
        if (message.type === 'apw-begin-checkout') currentPhase = 'bag';
        if (message.type === 'apw-checkout-step') currentPhase = message.phase;
        return currentPhase ? { phase: currentPhase, storeNumber: 'R683' } : null;
      },
    },
  };
  const document = {
    documentElement: {},
    body: { appendChild(node) { notices.set(node.id, node); } },
    getElementById(id) { return notices.get(id) || null; },
    createElement() { return { style: {}, setAttribute() {}, textContent: '' }; },
    querySelectorAll(selector) {
      if (selector in selectors) return selectors[selector];
      return selector === '[data-autom="checkout"]' ? official : candidates;
    },
    querySelector(selector) { return selectors[selector]?.[0] || null; },
  };
  const window = {};
  window.top = window;
  vm.runInNewContext(script, {
    window, location, history, document, chrome, URLSearchParams, Date,
    MutationObserver: class { observe() {} },
    setTimeout() { return 1; },
  });
  await new Promise(setImmediate);
  return { history, notices, get phase() { return currentPhase; } };
}

test('marked bag clicks the official checkout button once', async () => {
  const checkout = button('结账', {});
  const applePay = button('Check out with Apple Pay');
  const result = await page({
    pathname: '/shop/bag', hash: '#apw-checkout=1&apw-store=R683', official: [checkout], candidates: [applePay],
  });
  assert.equal(checkout.clicks, 1);
  assert.equal(applePay.clicks, 0);
  assert.equal(result.phase, 'bag-clicked');
  assert.equal(result.history.url, '/shop/bag');
});

test('unmarked bag cannot start checkout and guest step does not place an order', async () => {
  const checkout = button('结账', {});
  await page({ pathname: '/shop/bag', official: [checkout] });
  assert.equal(checkout.clicks, 0);

  const guest = button('以游客身份继续');
  const placeOrder = button('现在下订单');
  const result = await page({
    pathname: '/shop/checkout', phase: 'bag-clicked', candidates: [placeOrder, guest],
  });
  assert.equal(guest.clicks, 1);
  assert.equal(placeOrder.clicks, 0);
  assert.equal(result.phase, 'guest-clicked');
});

test('pickup fulfillment selects only the monitored store before continuing', async () => {
  const target = button('上海环球港');
  target.value = 'R683';
  target.checked = false;
  const fulfillment = button('继续');
  const other = button('其他门店');
  other.value = 'R999';
  const selectors = {
    'input[type="radio"].form-selector-input': [other, target],
    '[data-autom="fulfillment-continue-button"]': [fulfillment],
    '[role="dialog"]': [],
  };
  await page({ pathname: '/shop/checkout', phase: 'guest-clicked', selectors });
  assert.equal(target.clicks, 1);
  assert.equal(other.clicks, 0);
  assert.equal(fulfillment.clicks, 0);

  target.checked = true;
  await page({ pathname: '/shop/checkout', phase: 'guest-clicked', selectors });
  assert.equal(fulfillment.clicks, 1);
});

test('privacy consent and a missing monitored store stop automatic checkout', async () => {
  const fulfillment = button('继续');
  const privacy = button('Apple 和你的数据隐私');
  const selectors = {
    '[data-autom="fulfillment-continue-button"]': [fulfillment],
    'input[type="radio"].form-selector-input': [],
    '[role="dialog"]': [privacy],
  };
  await page({ pathname: '/shop/checkout', phase: 'guest-clicked', selectors });
  assert.equal(fulfillment.clicks, 0);

  selectors['[role="dialog"]'] = [];
  await page({ pathname: '/shop/checkout', phase: 'guest-clicked', selectors });
  assert.equal(fulfillment.clicks, 0);
});

test('billing selects Alipay and leaves the final Apple order button to the user', async () => {
  const alipay = button('支付宝');
  alipay.id = 'checkout-billing-alipay';
  alipay.name = 'billing';
  alipay.value = 'ALIPAY';
  alipay.checked = false;
  const other = button('信用卡');
  other.id = 'checkout-billing-card';
  other.name = 'billing';
  other.value = 'CARD';
  const review = button('继续');
  const order = button('现在下订单');
  const selectors = {
    'input[type="radio"]': [other, alipay],
    '[data-autom="continue-button-review"]': [review],
    '[data-autom="continue-button-placeOrder"]': [order],
    '[role="dialog"]': [],
  };
  await page({ pathname: '/shop/checkout', phase: 'guest-clicked', selectors });
  assert.equal(alipay.clicks, 1);
  assert.equal(review.clicks, 0);
  assert.equal(order.clicks, 0);
});
