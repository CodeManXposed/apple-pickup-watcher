import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import vm from 'node:vm';
import test from 'node:test';

const script = readFileSync(resolve('browser-extension/profile-fill.js'), 'utf8');

function input({ name, label = '', value = '', type = 'text', parent = null }) {
  return {
    tagName: 'INPUT', type, name, id: '', value, parentElement: parent,
    disabled: false, readOnly: false,
    labels: [{ textContent: label }],
    events: [],
    getAttribute(key) { return key === 'name' ? name : null; },
    getClientRects() { return [1]; },
    dispatchEvent(event) { this.events.push(event.type); },
  };
}

function section(className) {
  return {
    id: '', className, children: [], parentElement: null,
    getAttribute() { return null; },
  };
}

async function run(fields, saved, pathname = '/shop/checkout') {
  let changed;
  let observer;
  const document = {
    documentElement: {},
    querySelectorAll() { return fields; },
    getElementById() { return null; },
  };
  class InputElement {}
  Object.defineProperty(InputElement.prototype, 'value', {
    set(value) { this.value = value; },
  });
  const chrome = {
    storage: {
      local: { async get() { return { apwCheckoutProfile: saved }; } },
      onChanged: { addListener(fn) { changed = fn; } },
    },
  };
  vm.runInNewContext(script, {
    chrome, document, location: { pathname },
    window: { addEventListener() {} },
    HTMLInputElement: InputElement, HTMLTextAreaElement: class {},
    Event: class { constructor(type) { this.type = type; } },
    MutationObserver: class { constructor(callback) { observer = callback; } observe() {} },
    setTimeout(callback) { callback(); },
  });
  await new Promise(setImmediate);
  return { changed, observer };
}

const profile = {
  enabled: true,
  pickup: { givenName: '小明', familyName: '王', phone: '13800000000', email: 'pickup@example.test' },
  delivery: { givenName: '小红', familyName: '李', phone: '13900000000', email: 'ship@example.test', address1: '人民路 1 号', postalCode: '100000' },
};

test('fills distinct pickup and delivery contacts without touching billing or existing values', async () => {
  const pickup = section('pickup-contact');
  const delivery = section('shipping-address');
  const billing = section('billing-address');
  const pickupName = input({ name: 'firstName', label: '名', parent: pickup });
  const pickupPhone = input({ name: 'phone', label: '联系电话', parent: pickup, value: '用户已经填写' });
  const deliveryName = input({ name: 'shippingAddress.lastName', parent: delivery });
  const deliveryAddress = input({ name: 'addressLine1', label: '详细地址', parent: delivery });
  const billingName = input({ name: 'firstName', label: '名', parent: billing });
  const card = input({ name: 'payment.cardNumber', label: '银行卡号', parent: delivery });
  await run([pickupName, pickupPhone, deliveryName, deliveryAddress, billingName, card], profile);
  assert.equal(pickupName.value, '小明');
  assert.equal(pickupPhone.value, '用户已经填写');
  assert.equal(deliveryName.value, '李');
  assert.equal(deliveryAddress.value, '人民路 1 号');
  assert.equal(billingName.value, '');
  assert.equal(card.value, '');
  assert.deepEqual(deliveryAddress.events, ['input', 'change']);
});

test('only runs in checkout, respects disable switch, and fills later form steps', async () => {
  const delivery = section('shipping-address');
  const fields = [input({ name: 'email', parent: delivery })];
  await run(fields, profile, '/shop/bag');
  assert.equal(fields[0].value, '');
  await run(fields, { ...profile, enabled: false });
  assert.equal(fields[0].value, '');

  const runtime = await run(fields, profile);
  assert.equal(fields[0].value, 'ship@example.test');
  const later = input({ name: 'postalCode', parent: delivery });
  fields.push(later);
  runtime.observer();
  assert.equal(later.value, '100000');
});
