const STORAGE_KEY = 'apwCheckoutProfile';
const FIELDS = {
  pickup: ['familyName', 'givenName', 'fullName', 'phone', 'email'],
  delivery: [
    'familyName', 'givenName', 'fullName', 'phone', 'email',
    'address1', 'address2', 'country', 'state', 'city', 'district', 'postalCode',
  ],
};

const form = document.getElementById('profile-form');
const status = document.getElementById('status');

chrome.storage.local.get(STORAGE_KEY).then((result) => {
  const saved = result[STORAGE_KEY];
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
    await chrome.storage.local.set({ [STORAGE_KEY]: saved });
    status.textContent = '已保存。';
  } catch {
    status.textContent = '保存失败，请重试。';
  }
});

document.getElementById('clear-profile').addEventListener('click', async () => {
  if (!confirm('删除此 Chrome 用户配置文件中保存的取货和配送资料？')) return;
  try {
    await chrome.storage.local.remove(STORAGE_KEY);
    form.reset();
    status.textContent = '已删除。';
  } catch {
    status.textContent = '删除失败，请重试。';
  }
});
