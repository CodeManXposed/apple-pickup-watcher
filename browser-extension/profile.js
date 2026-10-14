const STORAGE_KEY = 'apwCheckoutProfile';
const BRIDGE_KEY = 'apwBridgeToken';
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

chrome.storage.local.get([STORAGE_KEY, BRIDGE_KEY]).then((result) => {
  const saved = result[STORAGE_KEY];
  form.elements.namedItem('bridgeToken').value = result[BRIDGE_KEY] || '';
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
    await chrome.storage.local.set({
      [STORAGE_KEY]: saved,
      [BRIDGE_KEY]: token,
    });
    status.textContent = `资料已保存。${await checkBridge(token)}`;
  } catch {
    status.textContent = '保存失败，请重试。';
  }
});

document.getElementById('clear-profile').addEventListener('click', async () => {
  if (!confirm('删除此 Chrome 用户配置文件中保存的取货、配送资料和扩展连接码？')) return;
  try {
    await chrome.storage.local.remove([STORAGE_KEY, BRIDGE_KEY]);
    form.reset();
    status.textContent = '资料和连接码已删除。';
  } catch {
    status.textContent = '删除失败，请重试。';
  }
});
