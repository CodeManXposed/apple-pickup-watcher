import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import vm from 'node:vm';
import test from 'node:test';

const script = readFileSync(resolve('browser-extension/content.js'), 'utf8');

function page(hash, store) {
  const notices = [];
  const nodes = new Map();
  const location = {
    pathname: '/shop/buy-iphone/iphone-18-pro/mjye4ch/a',
    search: '', hash, origin: 'https://www.apple.com.cn',
    assign(url) { this.assigned = url; },
  };
  const history = { state: null, replaceState(_state, _title, url) { this.replaced = url; } };
  const document = {
    body: { appendChild(node) { nodes.set(node.id, node); notices.push(node); } },
    createElement() { return { style: {}, setAttribute() {}, textContent: '' }; },
    getElementById(id) { return nodes.get(id) ?? null; },
  };
  const window = { acStore: store };
  window.top = window;
  let interval;
  let cleared = false;
  vm.runInNewContext(script, {
    window, location, history, document, URLSearchParams, Date,
    setInterval(fn) { interval = fn; return 1; },
    clearInterval() { cleared = true; },
  });
  return { location, history, notices, tick: () => interval?.(), get cleared() { return cleared; } };
}

test('only marked Apple product pages add the target once and open that bag', async () => {
  const calls = [];
  const store = { isDisabled: false, async addItem(part, qty) { calls.push([part, qty]); } };
  const p = page('#apw-auto-add=MJYE4CH%2FA', store);
  assert.equal(p.history.replaced, '/shop/buy-iphone/iphone-18-pro/mjye4ch/a');
  await p.tick();
  assert.deepEqual(calls, [['MJYE4CH/A', 1]]);
  assert.equal(p.location.assigned, 'https://www.apple.com.cn/shop/bag');
  assert.equal(p.cleared, true);

  const unmarked = page('', store);
  await unmarked.tick();
  assert.equal(unmarked.history.replaced, undefined);
  assert.deepEqual(calls, [['MJYE4CH/A', 1]]);
});

test('Apple rejection leaves the product page open and reports failure', async () => {
  const p = page('#apw-auto-add=MJYE4CH%2FA', {
    isDisabled: false,
    async addItem() { throw new Error('out of stock'); },
  });
  await p.tick();
  assert.equal(p.location.assigned, undefined);
  assert.match(p.notices[0].textContent, /out of stock/);
  assert.equal(p.cleared, true);
});
