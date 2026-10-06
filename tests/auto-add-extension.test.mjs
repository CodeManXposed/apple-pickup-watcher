import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import vm from 'node:vm';
import test from 'node:test';

const script = readFileSync(resolve('browser-extension/content.js'), 'utf8');

function control({ disabled = false, shown = true, checked = false } = {}) {
  return {
    disabled, shown, checked, clicks: 0,
    getAttribute() { return null; },
    getClientRects() { return this.shown ? [1] : []; },
    click() { this.clicks++; },
  };
}

function page({ hash = '#apw-auto-add=MJYE4CH%2FA&apw-store=R683', formPart = 'MJYE4CH/A',
  add = control({ disabled: true }), trade = control({ shown: false }),
  care = control({ disabled: true }), capacity = control() } = {}) {
  const nodes = new Map();
  const storage = new Map();
  const location = {
    pathname: '/shop/buy-iphone/iphone-18-pro/mjye4ch/a',
    search: '', hash, origin: 'https://www.apple.com.cn',
    assign(url) { this.assigned = url; },
  };
  const history = { state: null, replaceState(_state, _title, url) { this.replaced = url; location.hash = ''; } };
  const form = { getAttribute() { return formPart; }, querySelector(selector) {
    return selector === '[data-autom="add-to-cart"]' ? add : null;
  } };
  const document = {
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
    'https://www.apple.com.cn/shop/bag#apw-checkout=1&apw-store=R683&apw-part=MJYE4CH%2FA');
  assert.equal(p.storage.size, 0);
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
