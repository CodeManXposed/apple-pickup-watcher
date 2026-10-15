import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import test from 'node:test';
import vm from 'node:vm';

const script = readFileSync(resolve('browser-extension/release-ready.js'), 'utf8');

function control({ enabled = true, checked = false } = {}) {
  return {
    disabled: !enabled, checked, clicks: 0,
    getAttribute(name) { return name === 'aria-disabled' ? null : null; },
    getClientRects() { return [1]; },
    click() { this.clicks++; this.checked = true; },
  };
}

function page({ sku = 'MJT74CH/A', button = control({ enabled: false }), noTradeIn = null, noCare = null, capacity = null } = {}) {
  const attributes = new Set();
  const form = {
    getAttribute(name) { return name === 'data-part-number' ? sku : null; },
    querySelector(selector) { return selector === '[data-autom="add-to-cart"]' ? button : null; },
  };
  const document = {
    documentElement: {
      hasAttribute(name) { return attributes.has(name); },
      setAttribute(name) { attributes.add(name); },
    },
    querySelectorAll(selector) { return selector === 'form[data-part-number]' ? [form] : []; },
    querySelector(selector) {
      if (selector === 'input[data-autom="choose-noTradeIn"]') return noTradeIn;
      if (selector === 'input[data-autom="noapplecare"]') return noCare;
      if (selector === 'input[name="dimensionCapacity"]:checked') return capacity;
      return null;
    },
  };
  const ready = (part) => vm.runInNewContext(`(${script})(${JSON.stringify(part)})`, { document });
  return { ready, button, noTradeIn, noCare, capacity };
}

test('API release prepares official options and waits until add is enabled', () => {
  const noTradeIn = control();
  const noCare = control();
  const fixture = page({ noTradeIn, noCare });
  assert.equal(fixture.ready('MJT74CH/A'), false);
  assert.equal(noTradeIn.clicks, 1);
  assert.equal(noCare.clicks, 0);
  assert.equal(fixture.ready('MJT74CH/A'), false);
  assert.equal(noCare.clicks, 1);
  assert.equal(fixture.button.clicks, 0);
  fixture.button.disabled = false;
  assert.equal(fixture.ready('MJT74CH/A'), true);
  assert.equal(fixture.button.clicks, 0);
});

test('API release ignores another SKU and expands capacity only once', () => {
  const capacity = control({ checked: true });
  const fixture = page({ capacity });
  assert.equal(fixture.ready('OTHERCH/A'), false);
  assert.equal(capacity.clicks, 0);
  assert.equal(fixture.ready('MJT74CH/A'), false);
  assert.equal(fixture.ready('MJT74CH/A'), false);
  assert.equal(capacity.clicks, 1);
});
