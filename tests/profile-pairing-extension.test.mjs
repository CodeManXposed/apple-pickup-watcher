import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import vm from 'node:vm';
import test from 'node:test';

const script = readFileSync(resolve('browser-extension/profile.js'), 'utf8');

async function saveProfile(token, response = { ok: true, status: 200 }, configure = () => {}) {
  const handlers = new Map();
  const requests = [];
  const saved = [];
  const fields = new Map();
  const namedItem = (name) => {
    if (!fields.has(name)) fields.set(name, { value: '', checked: true });
    return fields.get(name);
  };
  const form = {
    elements: { namedItem },
    addEventListener(type, handler) { handlers.set(type, handler); },
  };
  const status = { textContent: '' };
  const clear = { addEventListener() {} };
  const document = { getElementById(id) {
    return { 'profile-form': form, status, 'clear-profile': clear }[id];
  } };
  const chrome = { storage: { local: {
    async get() { return {}; },
    async set(value) { saved.push(value); },
  } } };
  vm.runInNewContext(script, {
    document, chrome, AbortSignal,
    async fetch(url, options) { requests.push({ url, options }); return response; },
  });
  await Promise.resolve();
  namedItem('bridgeToken').value = token;
  namedItem('pickup.givenName').value = '测试';
  configure(namedItem);
  await handlers.get('submit')({ preventDefault() {} });
  return { status: status.textContent, requests, saved };
}

test('saving a profile verifies the local pairing code without sending contact details', async () => {
  const token = 'a'.repeat(32);
  const result = await saveProfile(token);
  assert.match(result.status, /资料已保存。已连接桌面应用。/);
  assert.equal(result.requests.length, 1);
  assert.equal(result.requests[0].url, 'http://127.0.0.1:43849/pairing-check');
  assert.equal(result.requests[0].options.headers.Authorization, `Bearer ${token}`);
  assert.equal(result.requests[0].options.body, undefined);
  assert.equal(result.saved[0].apwCheckoutProfile.pickup.givenName, '测试');
});

test('an invalid pairing code leaves the profile saved and reports the problem', async () => {
  const result = await saveProfile('bad');
  assert.match(result.status, /资料已保存。连接码格式不正确/);
  assert.equal(result.requests.length, 0);
  assert.equal(result.saved[0].apwBridgeToken, 'bad');
});

test('a valid-looking but wrong code reports the bridge rejection', async () => {
  const result = await saveProfile('a'.repeat(32), { ok: false, status: 401 });
  assert.match(result.status, /连接码不匹配/);
  assert.equal(result.requests.length, 1);
});

test('auto order requires a connected bridge, complete pickup contact and an integer cap', async () => {
  const token = 'a'.repeat(32);
  const configure = (field) => {
    field('autoPlaceOrder').checked = true;
    field('maxOrderCny').value = '9999';
    field('pickup.familyName').value = '测';
    field('pickup.phone').value = '13800000000';
    field('pickup.email').value = 'buyer@example.test';
  };
  const ok = await saveProfile(token, { ok: true, status: 200 }, configure);
  assert.equal(ok.saved[0].apwOrderConfig.enabled, true);
  assert.equal(ok.saved[0].apwOrderConfig.maxCny, 9999);

  const badBridge = await saveProfile(token, { ok: false, status: 401 }, configure);
  assert.equal(badBridge.saved.length, 0);
  assert.match(badBridge.status, /连接码不匹配/);

  const badCap = await saveProfile(token, { ok: true, status: 200 }, (field) => {
    configure(field);
    field('maxOrderCny').value = '9999.5';
  });
  assert.equal(badCap.saved.length, 0);
  assert.match(badCap.status, /整数金额上限/);
});
