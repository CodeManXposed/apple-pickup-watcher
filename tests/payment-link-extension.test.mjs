import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import vm from 'node:vm';
import test from 'node:test';

const script = readFileSync(resolve('browser-extension/background.js'), 'utf8');
const captureScript = readFileSync(resolve('browser-extension/alipay-capture.js'), 'utf8');
const apple = 'https://www.apple.com.cn/shop/bag';
const alipay = 'https://excashier.alipay.com/payment.htm?order=123';

function background() {
  const session = new Map();
  const local = new Map([['apwBridgeToken', 'a'.repeat(32)]]);
  const requests = [];
  let onMessage;
  let onCreated;
  const chrome = {
    runtime: { onMessage: { addListener(listener) { onMessage = listener; } } },
    tabs: {
      onCreated: { addListener(listener) { onCreated = listener; } },
      onRemoved: { addListener() {} },
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
      return { ok: true };
    },
  });
  function send(message, url, tabId = 1) {
    return new Promise((resolve) => {
      const accepted = onMessage(message, { url, tab: { id: tabId } }, resolve);
      if (!accepted) resolve(null);
    });
  }
  return { send, requests, local, createTab(tab) { onCreated(tab); } };
}

test('only an active Apple guest checkout forwards a payment link', async () => {
  const app = background();
  const request = { type: 'apw-payment-link', kind: 'cashier', url: alipay };
  assert.equal((await app.send(request, alipay))?.reason, 'no-active-checkout');
  assert.equal(app.requests.length, 0);

  await app.send({ type: 'apw-begin-checkout', storeNumber: 'R683' }, apple);
  await app.send({ type: 'apw-checkout-step', phase: 'bag-clicked' }, apple);
  await app.send({ type: 'apw-checkout-step', phase: 'guest-clicked' }, 'https://secure11.www.apple.com.cn/shop/signIn');
  assert.equal((await app.send(request, alipay, 2))?.reason, 'no-active-checkout');
  assert.equal((await app.send(request, alipay))?.reason, 'no-active-checkout');
  await app.send({ type: 'apw-checkout-step', phase: 'order-submitted' }, 'https://secure11.www.apple.com.cn/shop/checkout');
  assert.equal((await app.send(request, alipay))?.ok, true);
  assert.equal(app.requests.length, 1);
  assert.equal(JSON.parse(app.requests[0].options.body).url, alipay);
  assert.equal(app.requests[0].options.headers.Authorization, `Bearer ${'a'.repeat(32)}`);
});

test('rejects unrelated or forged cashier URLs and missing pairing code', async () => {
  const app = background();
  await app.send({ type: 'apw-begin-checkout', storeNumber: 'R683' }, apple);
  await app.send({ type: 'apw-checkout-step', phase: 'bag-clicked' }, apple);
  await app.send({ type: 'apw-checkout-step', phase: 'guest-clicked' }, 'https://secure11.www.apple.com.cn/shop/signIn');
  await app.send({ type: 'apw-checkout-step', phase: 'order-submitted' }, 'https://secure11.www.apple.com.cn/shop/checkout');
  assert.equal((await app.send({ type: 'apw-payment-link', kind: 'cashier', url: 'https://alipay.com.evil.example/pay' }, alipay))?.reason, 'invalid-link');
  assert.equal((await app.send({ type: 'apw-payment-link', kind: 'cashier', url: 'https://qr.alipay.com/abc' }, alipay))?.reason, 'invalid-link');
  app.local.delete('apwBridgeToken');
  assert.equal((await app.send({ type: 'apw-payment-link', kind: 'cashier', url: alipay }, alipay))?.reason, 'missing-pairing-code');
  assert.equal(app.requests.length, 0);
});

test('new Alipay tab follows the parent order submission even when opened first', async () => {
  const app = background();
  await app.send({ type: 'apw-begin-checkout', storeNumber: 'R683' }, apple);
  await app.send({ type: 'apw-checkout-step', phase: 'bag-clicked' }, apple);
  await app.send({ type: 'apw-checkout-step', phase: 'guest-clicked' }, 'https://secure11.www.apple.com.cn/shop/signIn');
  app.createTab({ id: 2, openerTabId: 1 });
  await new Promise(setImmediate);
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
