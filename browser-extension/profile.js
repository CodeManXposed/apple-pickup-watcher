const STORAGE_KEY = 'apwCheckoutProfile';
const BRIDGE_KEY = 'apwBridgeToken';
const ORDER_KEY = 'apwOrderConfig';
const PAIRING_URL = 'http://127.0.0.1:43849/pairing-check';
const FIELDS = {
  pickup: ['familyName', 'givenName', 'fullName', 'phone', 'email'],
  delivery: [
    'familyName', 'givenName', 'fullName', 'phone', 'email',
    'address1', 'address2', 'country', 'state', 'city', 'district', 'postalCode',
  ],
};

const form = document.getElementById('profile-form');
const status = document.getElementById('status');

async function checkBridge(token) {
  if (!/^[a-f0-9]{32}$/i.test(token)) return '连接码格式不正确，请复制桌面应用中的连接码。';
  try {
    const response = await fetch(PAIRING_URL, {
      method: 'POST',
      headers: { Authorization: `Bearer ${token}` },
      signal: AbortSignal.timeout(3000),
    });
    if (response.ok) return '已连接桌面应用。';
    if (response.status === 401) return '连接码不匹配，请重新从桌面应用复制。';
    return `桌面应用连接失败（HTTP ${response.status}）。`;
  } catch {
    return '无法连接桌面应用，请确认应用正在运行。';
  }
}

chrome.storage.local.get([STORAGE_KEY, BRIDGE_KEY, ORDER_KEY]).then((result) => {
  const saved = result[STORAGE_KEY];
  form.elements.namedItem('bridgeToken').value = result[BRIDGE_KEY] || '';
  form.elements.namedItem('autoPlaceOrder').checked = result[ORDER_KEY]?.enabled === true;
  form.elements.namedItem('maxOrderCny').value = result[ORDER_KEY]?.maxCny || '';
  if (!saved) return;
  form.elements.namedItem('enabled').checked = saved.enabled !== false;
  for (const [section, keys] of Object.entries(FIELDS)) {
    for (const key of keys) {
      form.elements.namedItem(`${section}.${key}`).value = saved[section]?.[key] || '';
    }
  }
}).catch(() => { status.textContent = '读取资料失败，请刷新后重试。'; });

form.addEventListener('submit', async (event) => {
  event.preventDefault();
  const saved = { enabled: form.elements.namedItem('enabled').checked };
  for (const [section, keys] of Object.entries(FIELDS)) {
    saved[section] = {};
    for (const key of keys) {
      saved[section][key] = form.elements.namedItem(`${section}.${key}`).value.trim();
    }
  }
  try {
    const token = form.elements.namedItem('bridgeToken').value.trim();
    const autoPlaceOrder = form.elements.namedItem('autoPlaceOrder').checked;
    const maxCny = Number(form.elements.namedItem('maxOrderCny').value);
    if (autoPlaceOrder && (!Number.isSafeInteger(maxCny) || maxCny < 1 || maxCny > 1_000_000)) {
      status.textContent = '请填写 1 至 1000000 元之间的整数金额上限。';
      return;
    }
    if (autoPlaceOrder && (!saved.enabled || !saved.pickup.phone || !saved.pickup.email ||
        !(saved.pickup.fullName || saved.pickup.familyName && saved.pickup.givenName))) {
      status.textContent = '自动下单前，请启用资料填充，并填写取货人的姓名、电话和邮箱。';
      return;
    }
    const bridgeStatus = await checkBridge(token);
    if (autoPlaceOrder && bridgeStatus !== '已连接桌面应用。') {
      status.textContent = bridgeStatus;
      return;
    }
    await chrome.storage.local.set({
      [STORAGE_KEY]: saved,
      [BRIDGE_KEY]: token,
      [ORDER_KEY]: { enabled: autoPlaceOrder, maxCny: autoPlaceOrder ? maxCny : null },
    });
    status.textContent = `资料已保存。${bridgeStatus}`;
  } catch {
    status.textContent = '保存失败，请重试。';
  }
});

document.getElementById('clear-profile').addEventListener('click', async () => {
  if (!confirm('删除此 Chrome 用户配置文件中保存的取货、配送资料、自动下单设置和扩展连接码？')) return;
  try {
    await chrome.storage.local.remove([STORAGE_KEY, BRIDGE_KEY, ORDER_KEY]);
    form.reset();
    status.textContent = '资料和连接码已删除。';
  } catch {
    status.textContent = '删除失败，请重试。';
  }
});
