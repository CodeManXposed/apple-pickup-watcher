import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import vm from 'node:vm';
import test from 'node:test';

const script = readFileSync(resolve('browser-extension/background.js'), 'utf8');
const captureScript = readFileSync(resolve('browser-extension/alipay-capture.js'), 'utf8');
const apple = 'https://www.apple.com.cn/shop/bag';
const alipay = 'https://excashier.alipay.com/payment.htm?order=123';
const taskId = 'b'.repeat(32);

function background({ taskActive = true } = {}) {
  const session = new Map();
  const local = new Map([['apwBridgeToken', 'a'.repeat(32)]]);
  const requests = [];
  let onMessage;
  let onCreated;
  let onNavigationTarget;
  const chrome = {
    runtime: { onMessage: { addListener(listener) { onMessage = listener; } } },
    tabs: {
      onCreated: { addListener(listener) { onCreated = listener; } },
      onRemoved: { addListener() {} },
    },
    webNavigation: {
      onCreatedNavigationTarget: { addListener(listener) { onNavigationTarget = listener; } },
    },
    storage: {
      session: {
        async get(key) { return { [key]: session.get(key) }; },
        async set(entries) { for (const [key, value] of Object.entries(entries)) session.set(key, value); },
        async remove(key) { session.delete(key); },
      },
      local: {
        async get(key) { return { [key]: local.get(key) }; },
      },
    },
  };
  vm.runInNewContext(script, {
    chrome, URL, Date, AbortSignal,
    async fetch(url, options) {
      requests.push({ url, options });
      if (url.endsWith('/task-check')) return { ok: taskActive };
      return { ok: true };
    },
  });
  function send(message, url, tabId = 1) {
    return new Promise((resolve) => {
      const accepted = onMessage(message, { url, tab: { id: tabId } }, resolve);
      if (!accepted) resolve(null);
    });
  }
  return {
    send, requests, local,
    createTab(tab) { onCreated(tab); },
    navigationTarget(details) { onNavigationTarget(details); },
  };
}

test('only an active Apple guest checkout forwards a payment link', async () => {
  const app = background();
  const request = { type: 'apw-payment-link', kind: 'cashier', url: alipay };
  assert.equal((await app.send(request, alipay))?.reason, 'no-active-checkout');
  assert.equal(app.requests.length, 0);

  await app.send({ type: 'apw-begin-checkout', storeNumber: 'R683', partNumber: 'MJYE4CH/A', taskId }, apple);
  await app.send({ type: 'apw-checkout-step', phase: 'bag-clicked' }, apple);
  await app.send({ type: 'apw-checkout-step', phase: 'guest-clicked' }, 'https://secure11.www.apple.com.cn/shop/signIn');
  assert.equal((await app.send(request, alipay, 2))?.reason, 'no-active-checkout');
  assert.equal((await app.send(request, alipay))?.reason, 'no-active-checkout');
  await app.send({ type: 'apw-checkout-step', phase: 'order-submitted' }, 'https://secure11.www.apple.com.cn/shop/checkout');
  assert.equal(app.requests.filter(({ url }) => url.endsWith('/task-status')).length, 0);
  assert.equal((await app.send(request, alipay))?.ok, true);
  assert.equal(app.requests.filter(({ url }) => url.endsWith('/task-status')).length, 1);
  const paymentRequests = app.requests.filter(({ url }) => url.endsWith('/payment-link'));
  assert.equal(paymentRequests.length, 1);
  assert.equal(JSON.parse(paymentRequests[0].options.body).url, alipay);
  assert.equal(JSON.parse(paymentRequests[0].options.body).partNumber, 'MJYE4CH/A');
  assert.equal(JSON.parse(paymentRequests[0].options.body).taskId, taskId);
  assert.equal(paymentRequests[0].options.headers.Authorization, `Bearer ${'a'.repeat(32)}`);
});

test('checkout cannot begin when the desktop task is stale or pairing is missing', async () => {
  const stale = background({ taskActive: false });
  assert.equal(await stale.send({ type: 'apw-begin-checkout', storeNumber: 'R683', partNumber: 'MJYE4CH/A', taskId }, apple), null);
  assert.equal(await stale.send({ type: 'apw-get-checkout' }, apple), null);
  const unpaired = background();
  unpaired.local.delete('apwBridgeToken');
  assert.equal(await unpaired.send({ type: 'apw-begin-checkout', storeNumber: 'R683', partNumber: 'MJYE4CH/A', taskId }, apple), null);
  assert.equal(unpaired.requests.length, 0);
});

test('rejects unrelated or forged cashier URLs and missing pairing code', async () => {
  const app = background();
  await app.send({ type: 'apw-begin-checkout', storeNumber: 'R683', partNumber: 'MJYE4CH/A', taskId }, apple);
  await app.send({ type: 'apw-checkout-step', phase: 'bag-clicked' }, apple);
  await app.send({ type: 'apw-checkout-step', phase: 'guest-clicked' }, 'https://secure11.www.apple.com.cn/shop/signIn');
  await app.send({ type: 'apw-checkout-step', phase: 'order-submitted' }, 'https://secure11.www.apple.com.cn/shop/checkout');
  assert.equal((await app.send({ type: 'apw-payment-link', kind: 'cashier', url: 'https://alipay.com.evil.example/pay' }, alipay))?.reason, 'invalid-link');
  assert.equal((await app.send({ type: 'apw-payment-link', kind: 'cashier', url: 'https://qr.alipay.com/abc' }, alipay))?.reason, 'invalid-link');
  app.local.delete('apwBridgeToken');
  assert.equal((await app.send({ type: 'apw-payment-link', kind: 'cashier', url: alipay }, alipay))?.reason, 'missing-pairing-code');
  assert.equal(app.requests.filter(({ url }) => url.endsWith('/payment-link')).length, 0);
});

test('forwards only stages from the active Apple checkout with its target', async () => {
  const app = background();
  const stage = (name) => ({ type: 'apw-task-stage', stage: name });
  assert.equal((await app.send(stage('bag'), apple))?.reason, 'no-active-checkout');
  await app.send({ type: 'apw-begin-checkout', storeNumber: 'R683', partNumber: 'MJYE4CH/A', taskId }, apple);
  assert.equal((await app.send(stage('bag'), apple))?.ok, true);
  assert.equal((await app.send(stage('review'), apple))?.reason, 'invalid-stage');
  assert.equal(await app.send(stage('bag'), 'https://evil.example/shop/bag'), null);
  await app.send({ type: 'apw-checkout-step', phase: 'bag-clicked' }, apple);
  assert.equal((await app.send(stage('checkout'), apple))?.ok, true);
  await app.send({ type: 'apw-checkout-step', phase: 'guest-clicked' }, 'https://secure11.www.apple.com.cn/shop/signIn');
  assert.equal((await app.send(stage('privacy'), 'https://secure11.www.apple.com.cn/shop/checkout'))?.ok, true);
  assert.equal((await app.send(stage('profile'), 'https://secure11.www.apple.com.cn/shop/checkout'))?.ok, true);
  assert.equal((await app.send(stage('profileAction'), 'https://secure11.www.apple.com.cn/shop/checkout'))?.ok, true);
  const statuses = app.requests.filter(({ url }) => url.endsWith('/task-status'))
    .map(({ options }) => JSON.parse(options.body));
  assert.deepEqual(statuses.map(({ stage }) => stage), ['bag', 'checkout', 'privacy', 'profile', 'profileAction']);
  assert.ok(statuses.every(({ partNumber, storeNumber }) => partNumber === 'MJYE4CH/A' && storeNumber === 'R683'));
  assert.ok(statuses.every((status) => status.taskId === taskId));
});

test('cart error reporting requires the matching Apple add URL and task identity', async () => {
  const app = background();
  const url = 'https://www.apple.com.cn/shop/buy-iphone/iphone-18-pro/mjye4ch/a?product=MJYE4CH%2FA&add-to-cart=add-to-cart';
  const message = { type: 'apw-cart-failed', partNumber: 'MJYE4CH/A', storeNumber: 'R683', taskId };
  assert.equal((await app.send(message, apple))?.reason, 'invalid-cart-failure');
  assert.equal((await app.send({ ...message, partNumber: 'OTHERCH/A' }, url))?.reason, 'invalid-cart-failure');
  assert.equal((await app.send(message, url))?.ok, true);
  const statuses = app.requests.filter(({ url: endpoint }) => endpoint.endsWith('/task-status'));
  assert.equal(statuses.length, 1);
  assert.deepEqual(JSON.parse(statuses[0].options.body), {
    partNumber: 'MJYE4CH/A', storeNumber: 'R683', taskId, stage: 'failed',
  });
});

test('order confirmations stay bound to the monitored store and checkout tab', async () => {
  const app = background();
  const checkout = 'https://secure11.www.apple.com.cn/shop/checkout';
  await app.send({ type: 'apw-begin-checkout', storeNumber: 'R683', partNumber: 'MJYE4CH/A', taskId }, apple);
  await app.send({ type: 'apw-checkout-step', phase: 'bag-clicked' }, apple);
  await app.send({ type: 'apw-checkout-step', phase: 'guest-clicked' }, 'https://secure11.www.apple.com.cn/shop/signIn');
  assert.equal(await app.send({ type: 'apw-confirm-store', storeNumber: 'R999' }, checkout), null);
  assert.equal(await app.send({ type: 'apw-confirm-alipay' }, checkout), null);
  assert.equal(await app.send({ type: 'apw-confirm-store', storeNumber: 'R683' }, checkout, 2), null);
  assert.equal((await app.send({ type: 'apw-confirm-store', storeNumber: 'R683' }, checkout))?.pickupConfirmed, true);
  assert.equal((await app.send({ type: 'apw-confirm-alipay' }, checkout))?.alipayConfirmed, true);
});

test('new Alipay tab follows the parent order submission even when opened first', async () => {
  const app = background();
  await app.send({ type: 'apw-begin-checkout', storeNumber: 'R683', partNumber: 'MJYE4CH/A', taskId }, apple);
  await app.send({ type: 'apw-checkout-step', phase: 'bag-clicked' }, apple);
  await app.send({ type: 'apw-checkout-step', phase: 'guest-clicked' }, 'https://secure11.www.apple.com.cn/shop/signIn');
  app.createTab({ id: 2, openerTabId: 1 });
  await new Promise(setImmediate);
  await app.send({ type: 'apw-checkout-step', phase: 'order-submitted' }, 'https://secure11.www.apple.com.cn/shop/checkout');
  assert.equal((await app.send({ type: 'apw-get-checkout' }, alipay, 2))?.phase, 'order-submitted');
  assert.equal((await app.send({ type: 'apw-payment-link', kind: 'cashier', url: alipay }, alipay, 2))?.ok, true);
});

test('Alipay navigation without an opener follows only its active Apple checkout source', async () => {
  const app = background();
  await app.send({ type: 'apw-begin-checkout', storeNumber: 'R683', partNumber: 'MJYE4CH/A', taskId }, apple);
  await app.send({ type: 'apw-checkout-step', phase: 'bag-clicked' }, apple);
  await app.send({ type: 'apw-checkout-step', phase: 'guest-clicked' }, 'https://secure11.www.apple.com.cn/shop/signIn');
  app.navigationTarget({ tabId: 2, sourceTabId: 1, url: alipay });
  app.navigationTarget({ tabId: 3, sourceTabId: 99, url: alipay });
  app.navigationTarget({ tabId: 4, sourceTabId: 1, url: 'https://evil.example/pay' });
  await new Promise(setImmediate);
  assert.equal(await app.send({ type: 'apw-get-checkout' }, alipay, 3), null);
  assert.equal(await app.send({ type: 'apw-get-checkout' }, alipay, 4), null);
  await app.send({ type: 'apw-checkout-step', phase: 'order-submitted' }, 'https://secure11.www.apple.com.cn/shop/checkout');
  assert.equal((await app.send({ type: 'apw-get-checkout' }, alipay, 2))?.phase, 'order-submitted');
  assert.equal((await app.send({ type: 'apw-payment-link', kind: 'cashier', url: alipay }, alipay, 2))?.ok, true);
});

async function capture({ flow, nodes, now = 0 }) {
  const sent = [];
  const clock = { now: () => now };
  const document = {
    documentElement: {},
    querySelectorAll(selector) {
      if (selector === 'canvas, img') return [];
      return nodes;
    },
    getElementById() { return null; },
    createElement() { return { style: {}, setAttribute() {}, textContent: '' }; },
    body: { appendChild() {} },
  };
  const window = {};
  window.top = window;
  vm.runInNewContext(captureScript, {
    window, document, URL, Date: clock,
    location: { href: alipay },
    chrome: { runtime: { async sendMessage(message) {
      if (message.type === 'apw-get-checkout') return flow;
      sent.push(message);
      return { ok: true };
    } } },
    MutationObserver: class { observe() {} },
    setTimeout(callback) { callback(); },
    setInterval() {},
  });
  await new Promise(setImmediate);
  return sent;
}

test('captures the QR target encoded in a visible cashier image', async () => {
  const qr = 'https://qr.alipay.com/bax123';
  const image = {
    getClientRects() { return [1]; }, closest() { return null; },
    getAttribute(key) {
      return key === 'src' ? `https://excashier.alipay.com/qrcode?url=${encodeURIComponent(qr)}` : null;
    },
  };
  const sent = await capture({ flow: { phase: 'order-submitted' }, nodes: [image] });
  assert.equal(sent.length, 1);
  assert.equal(sent[0].kind, 'qr');
  assert.equal(sent[0].url, qr);
});

test('a separate Alipay visit never forwards a cashier or QR link', async () => {
  const sent = await capture({ flow: null, nodes: [], now: 6000 });
  assert.equal(sent.length, 0);
});
