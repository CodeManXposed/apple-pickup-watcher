import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import vm from 'node:vm';
import test from 'node:test';

const script = readFileSync(resolve('browser-extension/content.js'), 'utf8');
const failureScript = readFileSync(resolve('browser-extension/cart-failure.js'), 'utf8');
const taskId = 'b'.repeat(32);

function control({ disabled = false, shown = true, checked = false } = {}) {
  return {
    disabled, shown, checked, clicks: 0,
    getAttribute() { return null; },
    getClientRects() { return this.shown ? [1] : []; },
    click() { this.clicks++; },
  };
}

function page({ hash = `#apw-auto-add=MJYE4CH%2FA&apw-store=R683&apw-task=${taskId}`,
  search = '', initialIntent = null, title = '', formPart = 'MJYE4CH/A',
  add = control({ disabled: true }), trade = control({ shown: false }),
  care = control({ disabled: true }), capacity = control() } = {}) {
  const nodes = new Map();
  const storage = new Map(initialIntent ? [['apwAutoAddIntent', JSON.stringify(initialIntent)]] : []);
  const location = {
    pathname: '/shop/buy-iphone/iphone-18-pro/mjye4ch/a',
    search, hash, origin: 'https://www.apple.com.cn',
    assign(url) { this.assigned = url; },
  };
  const history = { state: null, replaceState(_state, _title, url) { this.replaced = url; location.hash = ''; } };
  const form = { getAttribute() { return formPart; }, querySelector(selector) {
    return selector === '[data-autom="add-to-cart"]' ? add : null;
  } };
  const document = {
    title,
    body: { appendChild(node) { nodes.set(node.id, node); } },
    createElement() { return { style: {}, setAttribute() {}, textContent: '' }; },
    getElementById(id) { return nodes.get(id) ?? null; },
    querySelectorAll(selector) { return selector === 'form[data-part-number]' ? [form] : []; },
    querySelector(selector) {
      return {
        'input[data-autom="choose-noTradeIn"]': trade,
        'input[data-autom="noapplecare"]': care,
        'input[name="dimensionCapacity"]:checked': capacity,
      }[selector] ?? null;
    },
  };
  const sessionStorage = {
    getItem(key) { return storage.get(key) ?? null; },
    setItem(key, value) { storage.set(key, value); },
    removeItem(key) { storage.delete(key); },
  };
  const window = {};
  window.top = window;
  let interval;
  vm.runInNewContext(script, {
    window, location, history, document, sessionStorage, URLSearchParams, URL, Date,
    setInterval(fn) { interval = fn; return 1; }, clearInterval() { interval = null; },
  });
  return { location, history, storage, nodes, add, trade, care, capacity, tick() { interval?.(); } };
}

test('matching iPhone SKU chooses required options, clicks official add once, then verifies attach URL', () => {
  const p = page();
  assert.equal(p.history.replaced, '/shop/buy-iphone/iphone-18-pro/mjye4ch/a');
  assert.equal(p.capacity.clicks, 1);
  p.trade.shown = true;
  p.tick();
  assert.equal(p.trade.clicks, 1);
  p.trade.checked = true;
  p.care.disabled = false;
  p.tick();
  assert.equal(p.care.clicks, 1);
  p.care.checked = true;
  p.add.disabled = false;
  p.tick();
  p.tick();
  assert.equal(p.add.clicks, 1);
  assert.equal(p.location.assigned, undefined);
  p.location.pathname = '/shop/buy-iphone/iphone-18-pro';
  p.location.search = '?product=mjye4ch/a&step=attach';
  p.tick();
  assert.equal(p.location.assigned,
    `https://www.apple.com.cn/shop/bag#apw-checkout=1&apw-store=R683&apw-part=MJYE4CH%2FA&apw-task=${taskId}`);
  assert.equal(p.storage.size, 0);
});

test('pickup area survives product to bag navigation', () => {
  const hash = `#apw-auto-add=MJYE4CH%2FA&apw-store=R683&apw-task=${taskId}` +
    '&apw-province=%E4%B8%8A%E6%B5%B7&apw-city=%E4%B8%8A%E6%B5%B7&apw-district=%E6%99%AE%E9%99%80%E5%8C%BA';
  const p = page({ hash, add: control() });
  p.location.pathname = '/shop/buy-iphone/iphone-18-pro';
  p.location.search = '?product=mjye4ch/a&step=attach';
  p.tick();
  const bag = new URL(p.location.assigned);
  const checkout = new URLSearchParams(bag.hash.slice(1));
  assert.equal(checkout.get('apw-store'), 'R683');
  assert.equal(checkout.get('apw-province'), '上海');
  assert.equal(checkout.get('apw-city'), '上海');
  assert.equal(checkout.get('apw-district'), '普陀区');
});

test('invalid marker and mismatched SKU never click add', () => {
  const invalid = page({ hash: '#apw-auto-add=MJYE4CH%2FA&apw-store=%3Cbad%3E' });
  invalid.tick();
  assert.equal(invalid.add.clicks, 0);
  const mismatch = page({ formPart: 'OTHERCH/A', add: control() });
  mismatch.tick();
  assert.equal(mismatch.add.clicks, 0);
});

test('mismatched attach product never starts checkout', () => {
  const p = page({ add: control() });
  assert.equal(p.add.clicks, 1);
  p.location.pathname = '/shop/buy-iphone/iphone-18-pro';
  p.location.search = '?product=otherch/a&step=attach';
  p.tick();
  assert.equal(p.location.assigned, undefined);
});

test('Apple add error page stops the cart flow and reports the active task', async () => {
  const intent = {
    part: 'MJYE4CH/A', storeNumber: 'R683', taskId,
    phase: 'submitting', startedAt: Date.now(), submittedAt: Date.now(),
  };
  const p = page({
    hash: '', search: '?product=MJYE4CH%2FA&add-to-cart=add-to-cart',
    initialIntent: intent, title: 'Page Not Found - Apple',
  });
  assert.equal(JSON.parse(p.storage.get('apwAutoAddIntent')).phase, 'failed');
  assert.equal(p.add.clicks, 0);

  const sent = [];
  const window = {};
  window.top = window;
  vm.runInNewContext(failureScript, {
    window, location: p.location, document: { title: 'Page Not Found - Apple' },
    sessionStorage: {
      getItem(key) { return p.storage.get(key) ?? null; },
      removeItem(key) { p.storage.delete(key); },
    },
    chrome: { runtime: { async sendMessage(message) { sent.push(message); } } },
    URLSearchParams, Date,
  });
  await new Promise(setImmediate);
  assert.deepEqual(sent.map(({ type, partNumber, storeNumber, taskId: id }) =>
    ({ type, partNumber, storeNumber, taskId: id })), [
    { type: 'apw-cart-failed', partNumber: 'MJYE4CH/A', storeNumber: 'R683', taskId },
  ]);
  assert.equal(p.storage.size, 0);
});
