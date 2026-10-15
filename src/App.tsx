import { type KeyboardEvent, type ReactNode, useEffect, useMemo, useState, useSyncExternalStore } from "react";
import {
  AlertTriangle,
  BellRing,
  ChevronDown,
  ChevronUp,
  Download,
  ListTodo,
  Pause,
  Play,
  Plus,
  Radar,
  RefreshCw,
  Trash2,
  UserRound,
  X,
} from "lucide-react";

import { Combobox } from "@/components/Combobox";
import { MultiCombobox } from "@/components/MultiCombobox";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { ScrollArea } from "@/components/ui/scroll-area";
import { Switch } from "@/components/ui/switch";
import {
  capacityOptions,
  colorOptions,
  familyOptions,
  keepAvailable,
  productsForSelection,
} from "@/lib/product-selection";
import {
  cleanPushRows,
  PUSH_KIND_LABEL,
  type PushKind,
  pushSummary,
  samePushUrls,
  savedPushAddresses,
} from "@/lib/push";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import {
  Tooltip,
  TooltipContent,
  TooltipProvider,
  TooltipTrigger,
} from "@/components/ui/tooltip";

import {
  changeLocale,
  connect,
  copyBridgePairingCode,
  copyPaymentLink,
  dismissUpdate,
  installUpdate,
  openBotBrowser,
  openExtensionFolder,
  prepareBotBrowser,
  refreshProducts,
  saveSettings,
  setCategory,
  setIntervalSeconds,
  setPushUrls,
  setTargets,
  startWatching,
  stopWatching,
  testNotify,
  watcherStore,
} from "@/lib/store";
import {
  type Availability,
  type BotStatus,
  type Category,
  describeAdvice,
  describeAvailability,
  formatTime,
  isUntrusted,
  type Product,
  type Settings,
  type StatusTone,
  type TaskPlan,
  type Target,
  targetKey,
} from "@/lib/types";
import { cn } from "@/lib/utils";

/** 四种展示状态各自的样式。 */
const TONE_CLASS: Record<StatusTone, string> = {
  inStock: "bg-in-stock/15 text-in-stock border-in-stock/30 font-medium",
  outOfStock: "bg-muted text-muted-foreground border-transparent",
  // 「未知」必须和「无货」长得完全不一样。这是整个项目的意义所在：上游把查询
  // 失败显示成「无货」，用户对着一个早已失效的程序空等了大半年。
  unknown: "bg-unknown/15 text-unknown border-unknown/40 font-medium",
  pending: "bg-transparent text-muted-foreground/60 border-dashed",
};

const BOT_STAGE_LABEL: Record<BotStatus["stage"], string> = {
  starting: "正在启动独立浏览器",
  waitingRelease: "已打开商品页，等待开售时间",
  waitingRoom: "等待 Apple 放行或验证",
  launched: "已打开商品页",
  bag: "已进入购物袋",
  checkout: "正在进入结账",
  guest: "已选择访客结账",
  privacy: "等待你在浏览器处理 Apple 隐私同意书",
  profile: "正在填写结账资料",
  profileAction: "等待你在浏览器补全资料或身份验证",
  payment: "正在选择支付宝",
  review: "正在核对订单总额",
  orderSubmitted: "已尝试提交订单，等待支付宝链接",
  paymentLink: "已收到支付宝付款链接",
  failed: "自动结账任务失败，请查看日志",
};

function StatusBadge({ availability }: { availability: Availability }) {
  const { label, tone, detail } = describeAvailability(availability);
  const badge = (
    <Badge variant="outline" className={`min-w-18 justify-center ${TONE_CLASS[tone]}`}>
      {label}
    </Badge>
  );
  if (!detail) return badge;
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <span className="cursor-help">{badge}</span>
      </TooltipTrigger>
      <TooltipContent className="max-w-90">{detail}</TooltipContent>
    </Tooltip>
  );
}

/**
 * 一块带标题的分区。「在哪儿加什么」和「怎么跑」分开放，控件不再挤成一排。
 *
 * 传 `onToggle` 就可以折叠：标题行整行可点，折叠后只剩标题和一行摘要。
 */
function Panel({
  title,
  hint,
  open = true,
  onToggle,
  children,
}: {
  title: string;
  hint?: string;
  open?: boolean;
  onToggle?: () => void;
  children: ReactNode;
}) {
  const heading = (
    <>
      <h2 className="shrink-0 text-sm font-medium">{title}</h2>
      {hint !== undefined && (
        <p className="text-muted-foreground min-w-0 flex-1 truncate text-xs" title={hint}>
          {hint}
        </p>
      )}
      {onToggle !== undefined &&
        (open ? (
          <ChevronUp className="text-muted-foreground size-4 shrink-0" />
        ) : (
          <ChevronDown className="text-muted-foreground size-4 shrink-0" />
        ))}
    </>
  );
  return (
    <section className="bg-card text-card-foreground overflow-hidden rounded-xl border shadow-sm shadow-black/10">
      {onToggle !== undefined ? (
        <button
          type="button"
          aria-expanded={open}
          onClick={onToggle}
          className={cn(
            "hover:bg-muted/40 flex w-full items-center gap-3 px-4 py-3 text-left",
            open ? "rounded-t-xl" : "rounded-xl",
          )}
        >
          {heading}
        </button>
      ) : (
        <div className="flex items-center gap-3 px-4 py-3">{heading}</div>
      )}
      {open && <div className="grid gap-2.5 border-t px-4 pt-3 pb-4">{children}</div>}
    </section>
  );
}

const SETTINGS_OPEN_KEY = "apw.settingsOpen";
const WORKSPACE_TAB_KEY = "apw.workspaceTab";
type WorkspaceTab = "monitor" | "profile" | "tasks";
const WORKSPACE_TABS = ["monitor", "profile", "tasks"] as const;

function readWorkspaceTab(): WorkspaceTab {
  try {
    const tab = localStorage.getItem(WORKSPACE_TAB_KEY);
    if (tab === "monitor" || tab === "profile" || tab === "tasks") return tab;
  } catch {
    // 本地存储不可用时仍能切换分页。
  }
  return "monitor";
}

/** 运行设置保留用户上次的展开状态；首次启动优先展示库存列表。 */
function readSettingsOpen(): boolean {
  try {
    return localStorage.getItem(SETTINGS_OPEN_KEY) === "1";
  } catch {
    return false;
  }
}

/** 收起时的一行摘要：设置折起来了，但它们是什么一眼还在。 */
function settingsSummary(s: Settings): string {
  return [
    `每 ${s.intervalSeconds} 秒查一轮`,
    pushSummary(savedPushAddresses(s)),
    (s.proxies ?? []).length > 0 ? `代理 ${s.proxies.length} 条` : "无代理",
    s.soundEnabled ? "提示音开" : "提示音关",
    s.autoAddToBag
      ? "有货时自动加车"
      : s.openBagOnHit ? "有货时开购物袋" : "有货时不开购物袋",
  ].join(" · ");
}

function localDateTime(ms: number | null | undefined): string {
  if (!ms) return "";
  const date = new Date(ms);
  if (!Number.isFinite(date.getTime())) return "";
  const two = (value: number) => String(value).padStart(2, "0");
  return `${date.getFullYear()}-${two(date.getMonth() + 1)}-${two(date.getDate())}T${two(date.getHours())}:${two(date.getMinutes())}`;
}

/** 运行状态。一个会呼吸的点比一行灰字更像「它还活着」。 */
function RunState({ running }: { running: boolean }) {
  return (
    <span className={cn("inline-flex items-center gap-2 rounded-full border px-3 py-1.5 text-xs font-medium", running ? "border-in-stock/25 bg-in-stock/10 text-in-stock" : "border-border bg-muted/50 text-muted-foreground")}>
      <span className="relative flex size-2">
        {running && (
          <span className="bg-in-stock/60 absolute inline-flex size-full animate-ping rounded-full [animation-duration:2.4s]" />
        )}
        <span
          className={cn(
            "relative inline-flex size-2 rounded-full",
            running ? "bg-in-stock" : "bg-muted-foreground/40",
          )}
        />
      </span>
      {running ? "监听中" : "已暂停"}
    </span>
  );
}

/** 一个统计数字。数字等宽：每轮查询都会变，几个并排时不该来回抖。 */
function Stat({
  label,
  value,
  tone,
}: {
  label: string;
  value: number;
  tone?: "inStock" | "unknown";
}) {
  return (
    <span className="bg-card inline-flex min-w-25 items-baseline justify-between gap-3 rounded-lg border px-3 py-2 text-sm">
      <span className="text-muted-foreground text-xs font-medium">{label}</span>
      <span
        className={cn(
          "text-lg font-semibold leading-none tabular-nums",
          value > 0 && tone === "inStock" && "text-in-stock",
          value > 0 && tone === "unknown" && "text-unknown",
        )}
      >
        {value}
      </span>
    </span>
  );
}

/** 日志行。时间戳弱化，到货那几行用有货色标出来，一屏灰字里一眼能找到。 */
function LogLine({ line }: { line: string }) {
  const match = /^\[(\d{2}:\d{2}:\d{2})\] ?(.*)$/s.exec(line);
  if (!match) return <div className="whitespace-pre-wrap">{line}</div>;
  const [, stamp, text = ""] = match;
  return (
    <div className="whitespace-pre-wrap">
      <span className="text-muted-foreground/60">{stamp}</span>{" "}
      <span className={text.startsWith("有货") ? "text-in-stock font-medium" : "text-muted-foreground"}>
        {text}
      </span>
    </div>
  );
}

export default function App() {
  const ui = useSyncExternalStore(watcherStore.subscribe, watcherStore.getSnapshot);
  const currentPaymentLink = ui.botStatus && ui.paymentLink?.taskId === ui.botStatus.taskId
    ? ui.paymentLink : null;
  const [activeTab, setActiveTab] = useState<WorkspaceTab>(readWorkspaceTab());
  const [profileFeedback, setProfileFeedback] = useState("");
  const [browserPreparing, setBrowserPreparing] = useState(false);
  const [watchStarting, setWatchStarting] = useState(false);
  const [watchStartError, setWatchStartError] = useState("");
  const [taskSaving, setTaskSaving] = useState(false);
  const [taskFeedback, setTaskFeedback] = useState("");
  const [releaseDrafts, setReleaseDrafts] = useState<Record<string, string>>({});
  const [retryDrafts, setRetryDrafts] = useState<Record<string, string>>({});

  async function onStartWatching() {
    setWatchStarting(true);
    setWatchStartError("");
    try {
      await startWatching();
    } catch (error) {
      setWatchStartError(String(error));
    } finally {
      setWatchStarting(false);
    }
  }

  async function onAutoAddChange(enabled: boolean) {
    setTaskSaving(true);
    setTaskFeedback("");
    try {
      setTaskFeedback((await saveSettings({ ...ui.settings, autoAddToBag: enabled })) ?? "");
    } finally {
      setTaskSaving(false);
    }
  }

  async function onTaskPlanChange(target: Target, patch: Partial<TaskPlan>) {
    const key = targetKey(target);
    const plans = ui.settings.taskPlans ?? [];
    const existing = plans.find((plan) => plan.targetKey === key);
    const next: TaskPlan = {
      targetKey: key,
      mode: existing?.mode ?? "restock",
      addMethod: existing?.addMethod ?? "browser",
      productUrl: existing?.productUrl ?? null,
      releaseAtMs: existing?.releaseAtMs ?? null,
      retryDelayMs: existing?.retryDelayMs ?? 3000,
      ...patch,
    };
    setTaskSaving(true);
    setTaskFeedback("");
    try {
      setTaskFeedback((await saveSettings({
        ...ui.settings,
        taskPlans: [...plans.filter((plan) => plan.targetKey !== key), next],
      })) ?? "");
    } finally {
      setTaskSaving(false);
    }
  }

  function saveReleaseTime(target: Target, value: string) {
    if (!value) {
      void onTaskPlanChange(target, { releaseAtMs: null });
      return;
    }
    const at = new Date(value).getTime();
    if (!Number.isFinite(at) || at <= Date.now()) {
      setTaskFeedback("开售时间需填写未来的本地日期和时间。");
      return;
    }
    void onTaskPlanChange(target, { releaseAtMs: at });
  }

  function saveRetryDelay(target: Target, value: string) {
    const milliseconds = Number(value);
    if (!Number.isInteger(milliseconds) || milliseconds < 1000 || milliseconds > 60000) {
      setTaskFeedback("重试延迟须为 1000–60000 毫秒。");
      return;
    }
    void onTaskPlanChange(target, { retryDelayMs: milliseconds });
  }

  async function onPrepareBrowser() {
    setBrowserPreparing(true);
    setProfileFeedback("正在准备独立浏览器，请稍候…");
    try {
      setProfileFeedback(await prepareBotBrowser());
    } finally {
      setBrowserPreparing(false);
    }
  }

  function selectTab(tab: WorkspaceTab) {
    setActiveTab(tab);
    try {
      localStorage.setItem(WORKSPACE_TAB_KEY, tab);
    } catch {
      // 分页选择在当前窗口仍然有效。
    }
  }

  function onTabKeyDown(event: KeyboardEvent<HTMLButtonElement>, tab: WorkspaceTab) {
    const index = WORKSPACE_TABS.indexOf(tab);
    const next = event.key === "ArrowRight"
      ? WORKSPACE_TABS[(index + 1) % WORKSPACE_TABS.length]
      : event.key === "ArrowLeft"
        ? WORKSPACE_TABS[(index + WORKSPACE_TABS.length - 1) % WORKSPACE_TABS.length]
        : event.key === "Home" ? WORKSPACE_TABS[0]
          : event.key === "End" ? WORKSPACE_TABS[WORKSPACE_TABS.length - 1]
            : undefined;
    if (!next) return;
    event.preventDefault();
    selectTab(next);
    event.currentTarget.ownerDocument.getElementById(`workspace-tab-${next}`)?.focus();
  }

  useEffect(() => {
    void connect();
    // 刻意不在清理函数里断开：这是应用级的单一连接，窗口活着它就该活着。
    // StrictMode 的重复调用由 connect 内部去重。
  }, []);

  // 门店可以一次选好几家：同城门店合并成一次请求之后，多选不增加请求量。
  const [storeNumbers, setStoreNumbers] = useState<string[]>([]);
  const [manualSku, setManualSku] = useState("");
  // 非 iPhone 品类直接选一个完整型号。
  const [partNumber, setPartNumber] = useState("");
  // iPhone 按机型 → 容量 → 颜色三级选择，三级都可多选：发售当晚常见的是
  // 「这两款、这三个颜色，哪个有货要哪个」，一台一台加太慢。三级组合里目录中
  // 存在的型号全部加入。
  const [families, setFamilies] = useState<string[]>([]);
  const [capacities, setCapacities] = useState<string[]>([]);
  const [colors, setColors] = useState<string[]>([]);
  // 推送地址：各渠道共用一个列表，一行一个。null 表示还没动过，跟着后端走；
  // 动过之后以这份为准，保持用户填写的顺序，渠道标签按后端的归类显示。
  const [pushDraft, setPushDraft] = useState<string[] | null>(null);
  // 代理地址和推送地址一样一行一个，草稿的规矩也相同，见 pushDraft。
  const [proxyDraft, setProxyDraft] = useState<string[] | null>(null);
  const [intervalDraft, setIntervalDraft] = useState<number | null>(null);
  // 设置改一次就放着不动，折起来把纵向空间还给表格；记住上次的选择。
  const [settingsOpen, setSettingsOpen] = useState(readSettingsOpen());
  function toggleSettings() {
    const next = !settingsOpen;
    setSettingsOpen(next);
    try {
      localStorage.setItem(SETTINGS_OPEN_KEY, next ? "1" : "0");
    } catch {
      // 存不下就每次都展开，不值得报错。
    }
  }

  const savedPush = useMemo(() => savedPushAddresses(ui.settings), [ui.settings]);
  const savedPushKind = new Map<string, PushKind>(savedPush.map((p) => [p.url, p.kind]));
  const pushRows = pushDraft ?? savedPush.map((p) => p.url);
  // 一个地址都没有时留一个空行，直接粘贴就行，不用先点「添加」。
  const visiblePushRows = pushRows.length > 0 ? pushRows : [""];
  const lastPushRow = visiblePushRows[visiblePushRows.length - 1] ?? "";

  function editPushRow(index: number, value: string) {
    setPushDraft(visiblePushRows.map((row, i) => (i === index ? value : row)));
  }

  // 失焦和删除都走这里：整理后整组交给后端归类保存；没有实际改动就不写盘。
  // 整理后的列表留作草稿：保存是异步的，这时退回「跟着后端走」，界面会先闪回
  // 旧地址，紧接着点「添加」还会以旧列表为底，把刚填的地址弄丢。
  function commitPushRows(rows: string[]) {
    const urls = cleanPushRows(rows);
    setPushDraft(urls);
    if (!samePushUrls(urls, savedPush.map((p) => p.url))) {
      void setPushUrls(urls);
    }
  }
  // 旧设置文件没有 proxies 字段，读上来是 undefined，当作空列表。
  const savedProxies = ui.settings.proxies ?? [];
  const proxyRows = proxyDraft ?? savedProxies;
  const visibleProxyRows = proxyRows.length > 0 ? proxyRows : [""];
  const lastProxyRow = visibleProxyRows[visibleProxyRows.length - 1] ?? "";

  function editProxyRow(index: number, value: string) {
    setProxyDraft(visibleProxyRows.map((row, i) => (i === index ? value : row)));
  }

  // 每个代理是一条额外的出口线路：Apple 的配额按出口 IP 计，多一条线路多一份配额，
  // 被拦时自动换下一条。后端只留能解析的地址，留不下的那行会标「未生效」。
  function commitProxyRows(rows: string[]) {
    const proxies = cleanPushRows(rows);
    setProxyDraft(proxies);
    if (!samePushUrls(proxies, savedProxies)) {
      void saveSettings({ ...ui.settings, proxies });
    }
  }
  const intervalValue = intervalDraft ?? ui.settings.intervalSeconds;

  const storeOptions = useMemo(
    () => ui.stores.map((s) => ({ value: s.number, label: s.title })),
    [ui.stores],
  );
  // 只列当前品类。四个品类的型号加起来好几百条，全堆进一个下拉框，
  // 想找一台 Mac 得先划过所有 iPhone。
  const productOptions = useMemo(
    () =>
      ui.products
        .filter((p) => p.category === ui.category)
        .map((p) => ({ value: p.partNumber, label: p.title })),
    [ui.products, ui.category],
  );
  const iphoneFamilyOptions = useMemo(
    () => familyOptions(ui.products, ui.category),
    [ui.products, ui.category],
  );
  const iphoneCapacityOptions = useMemo(
    () => capacityOptions(ui.products, ui.category, families),
    [ui.products, ui.category, families],
  );
  const iphoneColorOptions = useMemo(
    () => colorOptions(ui.products, ui.category, families, capacities),
    [ui.products, ui.category, families, capacities],
  );
  // 点「添加」时会加入的型号。iPhone 是三级选择的组合，其他品类就是选中的那一个。
  const selectedProducts = useMemo<Product[]>(() => {
    if (ui.category === "iphone") {
      return productsForSelection(ui.products, ui.category, families, capacities, colors);
    }
    const product = ui.products.find((p) => p.partNumber === partNumber);
    return product ? [product] : [];
  }, [ui.products, ui.category, families, capacities, colors, partNumber]);

  function resetProductSelection() {
    setPartNumber("");
    setFamilies([]);
    setCapacities([]);
    setColors([]);
  }

  // 上级变了，下级只去掉不再可选的项，仍然成立的留着（理由见 keepAvailable）。
  function onFamiliesChange(next: string[]) {
    setFamilies(next);
    const nextCapacities = keepAvailable(
      capacities,
      capacityOptions(ui.products, ui.category, next),
    );
    setCapacities(nextCapacities);
    setColors(
      keepAvailable(colors, colorOptions(ui.products, ui.category, next, nextCapacities)),
    );
  }

  function onCapacitiesChange(next: string[]) {
    setCapacities(next);
    setColors(keepAvailable(colors, colorOptions(ui.products, ui.category, families, next)));
  }

  const targets = useMemo(() => ui.rows.map((r) => r.target), [ui.rows]);

  const summary = useMemo(() => {
    let inStock = 0;
    let outOfStock = 0;
    let untrusted = 0;
    for (const r of ui.rows) {
      if (r.availability.kind === "in_stock") inStock += 1;
      else if (r.availability.kind === "out_of_stock") outOfStock += 1;
      if (isUntrusted(r.availability)) untrusted += 1;
    }
    return { inStock, outOfStock, untrusted };
  }, [ui.rows]);

  const canAdd = storeNumbers.length > 0 && selectedProducts.length > 0;
  const normalizedManualSku = manualSku.trim().toUpperCase();
  const canAddManualSku = storeNumbers.length > 0 && /^[A-Z0-9]{4,20}\/[A-Z]$/.test(normalizedManualSku);

  async function onAdd() {
    if (!canAdd) return;

    // 门店 × 型号，每个组合一条目标；已经在列表里的跳过，不重复。
    // 按门店分组排，表格里同一家店的几台机器挨在一起。
    const existing = new Set(targets.map(targetKey));
    const added: Target[] = [];
    for (const number of storeNumbers) {
      const store = ui.stores.find((s) => s.number === number);
      if (!store) continue;
      for (const product of selectedProducts) {
        const next: Target = {
          locale: ui.settings.locale,
          storeNumber: store.number,
          storeTitle: store.title,
          partNumber: product.partNumber,
          productName: product.title,
          ...(product.companionPart ? { companionPart: product.companionPart } : {}),
        };
        const key = targetKey(next);
        if (existing.has(key)) continue;
        existing.add(key);
        added.push(next);
      }
    }
    if (added.length === 0) return;
    await setTargets([...targets, ...added]);
    // 只清型号，门店留着：接着给同一批门店加下一个型号是最常见的操作。
    resetProductSelection();
  }

  async function onAddManualSku() {
    if (!canAddManualSku) return;
    const existing = new Set(targets.map(targetKey));
    const product = ui.products.find((item) => item.partNumber.toUpperCase() === normalizedManualSku);
    const added: Target[] = [];
    for (const number of storeNumbers) {
      const store = ui.stores.find((item) => item.number === number);
      if (!store) continue;
      const target: Target = {
        locale: ui.settings.locale,
        storeNumber: store.number,
        storeTitle: store.title,
        partNumber: normalizedManualSku,
        productName: product?.title ?? `SKU ${normalizedManualSku}`,
        ...(product?.companionPart ? { companionPart: product.companionPart } : {}),
      };
      if (existing.has(targetKey(target))) continue;
      existing.add(targetKey(target));
      added.push(target);
    }
    if (added.length === 0) return;
    await setTargets([...targets, ...added]);
    setManualSku("");
  }

  async function onRemove(t: Target) {
    await setTargets(targets.filter((x) => targetKey(x) !== targetKey(t)));
  }

  return (
    <TooltipProvider delayDuration={200}>
      <div className="app-shell mx-auto flex h-screen max-w-[1500px] flex-col gap-3 overflow-hidden p-4">
        <header className="app-header flex shrink-0 items-center justify-between gap-4 rounded-xl border px-5 py-4">
          <div className="flex min-w-0 items-center gap-3">
            <span className="border-primary/30 bg-primary/10 text-primary flex size-10 shrink-0 items-center justify-center rounded-xl border"><Radar className="size-5" /></span>
            <div className="min-w-0">
              <p className="text-primary text-[10px] font-semibold tracking-[0.2em]">APPLE CONTROL DESK</p>
              <h1 className="truncate text-lg font-semibold tracking-tight">Apple Bot</h1>
            </div>
          </div>
          <div className="flex items-center gap-4">
            <RunState running={ui.running} />
            {ui.running ? (
              <Button variant="secondary" className="min-w-28" onClick={() => void stopWatching()}>
                <Pause /> 暂停
              </Button>
            ) : (
              <Button className="min-w-28" aria-label={watchStarting ? "正在检查自动监控环境" : "开始监控"} onClick={() => void onStartWatching()} disabled={ui.rows.length === 0 || watchStarting || taskSaving}>
                <Play /> {watchStarting ? "正在检查…" : "开始监控"}
              </Button>
            )}
          </div>
        </header>

        {watchStartError && (
          <div role="alert" className="border-destructive/30 bg-destructive/5 rounded-lg border px-4 py-3 text-sm">
            <p className="text-destructive font-medium">未能开始自动监控</p>
            <p className="mt-1">{watchStartError}</p>
            {ui.settings.autoAddToBag && (
              <Button size="sm" variant="outline" className="mt-2" onClick={() => selectTab("profile")}>前往 Profile</Button>
            )}
          </div>
        )}

        <nav aria-label="工作区" className="app-nav flex shrink-0 items-center gap-1 rounded-lg border px-2" role="tablist">
          {([
            ["monitor", "监控", Radar],
            ["profile", "Profile", UserRound],
            ["tasks", "任务", ListTodo],
          ] as const).map(([tab, label, Icon]) => (
            <button
              key={tab}
              type="button"
              id={`workspace-tab-${tab}`}
              role="tab"
              aria-controls={`workspace-panel-${tab}`}
              aria-selected={activeTab === tab}
              data-active={activeTab === tab}
              tabIndex={activeTab === tab ? 0 : -1}
              onClick={() => selectTab(tab)}
              onKeyDown={(event) => onTabKeyDown(event, tab)}
              className={cn(
                "workspace-tab flex min-w-36 items-center justify-center gap-2 rounded-t-md px-4 py-3 text-sm font-medium transition-colors focus-visible:outline-2 focus-visible:outline-offset-2",
                activeTab === tab
                  ? "text-foreground"
                  : "text-muted-foreground hover:bg-muted/50 hover:text-foreground",
              )}
            >
              <Icon className="size-4" aria-hidden="true" />
              {label}
              {tab === "monitor" && ui.rows.length > 0 && (
                <span className="bg-muted text-muted-foreground rounded-full px-1.5 text-xs tabular-nums">{ui.rows.length}</span>
              )}
              {tab === "tasks" && (currentPaymentLink || ui.botStatus) && (
                <span className="bg-muted text-muted-foreground rounded-full px-1.5 text-xs">
                  {currentPaymentLink ? "付款链接" : "进度"}
                </span>
              )}
            </button>
          ))}
        </nav>

        {ui.trouble !== null && (
          // 只有标题是红的。原因和建议各占一行、用正常的字色：三段红字堆在一起，
          // 用户反而分不清哪句是原因、哪句是该做的事。
          <div
            role="alert"
            className="border-destructive/30 bg-destructive/5 border-l-destructive grid gap-1.5 rounded-lg border border-l-4 px-4 py-3 text-sm"
          >
            <p className="text-destructive flex flex-wrap items-center gap-x-2 font-medium">
              <AlertTriangle className="size-4" />
              监控当前不可信
              <span className="text-muted-foreground font-normal">
                恢复之前，表格里的状态不代表门店的真实库存
              </span>
            </p>
            <p className="pl-6">{ui.trouble.reason}</p>
            {ui.trouble.advice !== null && (
              // 用户自己能做的那件事单独拎出来，而且要短：issue #3 里那位对着反复
              // 告警的窗口干等了三个小时，缺的不是解释，是一句「换条网络」。
              <p className="text-muted-foreground pl-6">
                <span className="text-foreground font-medium">建议</span>{" "}
                {describeAdvice(ui.trouble.advice)}
              </p>
            )}
          </div>
        )}

        {ui.update !== null && (
          // 只提示，不自作主张安装。用户可能正等着抢购，被强制重启是灾难。
          <div className="bg-card flex flex-wrap items-center gap-x-4 gap-y-2 rounded-lg border px-4 py-2.5 text-sm shadow-xs">
            <p className="flex items-center gap-2 font-medium">
              <Download className="size-4" /> 有新版本 {ui.update.version}
            </p>
            <p className="text-muted-foreground">
              当前 {ui.update.currentVersion}，安装后需重启应用生效。
            </p>
            <div className="ml-auto flex items-center gap-2">
              <Button size="sm" disabled={ui.installing} onClick={() => void installUpdate()}>
                {ui.installing ? "正在下载…" : "下载并安装"}
              </Button>
              <Button size="sm" variant="ghost" onClick={dismissUpdate}>
                <X /> 稍后
              </Button>
            </div>
          </div>
        )}

        {activeTab === "monitor" && (
        <main
          id="workspace-panel-monitor"
          role="tabpanel"
          aria-labelledby="workspace-tab-monitor"
          className="workspace-panel monitor-workspace min-h-0 flex-1"
        >
        <div className="monitor-heading flex items-end justify-between gap-3">
          <div>
            <p className="text-primary text-[10px] font-semibold tracking-[0.18em]">LIVE INVENTORY</p>
            <h2 className="text-xl font-semibold tracking-tight">库存监控</h2>
          </div>
          <p className="text-muted-foreground text-xs">Apple 直营店 · 到店取货</p>
        </div>
        <aside className="monitor-builder flex min-h-0 flex-col gap-3 overflow-y-auto pr-1" aria-label="监控配置">
        <Panel
          title="添加监控目标"
          hint="门店和型号可多选"
        >
          <div className="grid grid-cols-2 gap-3">
            <div className="grid min-w-0 gap-1.5">
              <Label>地区</Label>
              <Combobox
                className="w-full"
                options={ui.regions.map((r) => ({ value: r.locale, label: r.title }))}
                value={ui.settings.locale}
                onChange={(locale) => {
                  // 换地区后旧的门店和型号都不再适用，清掉待添加的选择。
                  setStoreNumbers([]);
                  resetProductSelection();
                  void changeLocale(locale);
                }}
                placeholder="选择地区"
                searchPlaceholder="搜索地区…"
                emptyText="没有匹配的地区"
              />
            </div>

            <div className="grid min-w-0 gap-1.5">
              <Label>品类</Label>
              <Combobox
                className="w-full"
                options={ui.categories.map((c) => ({ value: c.value, label: c.title }))}
                value={ui.category}
                onChange={(value) => {
                  // 换品类后旧的型号不再在下拉框里，清掉待添加的选择。门店不用清，
                  // 它跟品类无关。
                  resetProductSelection();
                  setCategory(value as Category);
                }}
                placeholder="选择品类"
                searchPlaceholder="搜索品类…"
                emptyText="没有匹配的品类"
                disabled={ui.categories.length === 0}
              />
            </div>

            <div className="col-span-2 grid min-w-0 gap-1.5">
              <Label>门店（可多选）</Label>
              <MultiCombobox
                className="w-full"
                options={storeOptions}
                values={storeNumbers}
                onChange={setStoreNumbers}
                placeholder="选择自提门店"
                searchPlaceholder="搜索门店…"
                emptyText="没有匹配的门店"
                countLabel={(n) => `已选 ${n} 家门店`}
                disabled={storeOptions.length === 0}
              />
            </div>
          </div>

          <div className="grid grid-cols-2 items-end gap-3">
            {ui.category === "iphone" ? (
              <>
                <div className="col-span-2 grid min-w-0 gap-1.5">
                  <Label>机型（可多选）</Label>
                  <MultiCombobox
                    className="w-full"
                    options={iphoneFamilyOptions}
                    values={families}
                    onChange={onFamiliesChange}
                    placeholder="选择机型"
                    searchPlaceholder="搜索机型…"
                    emptyText="没有匹配的机型"
                    countLabel={(n) => `已选 ${n} 款机型`}
                    disabled={iphoneFamilyOptions.length === 0}
                  />
                </div>

                <div className="grid min-w-0 gap-1.5">
                  <Label>容量（可多选）</Label>
                  <MultiCombobox
                    className="w-full"
                    options={iphoneCapacityOptions}
                    values={capacities}
                    onChange={onCapacitiesChange}
                    placeholder="选择容量"
                    searchPlaceholder="搜索容量…"
                    emptyText="没有匹配的容量"
                    countLabel={(n) => `已选 ${n} 种容量`}
                    disabled={families.length === 0 || iphoneCapacityOptions.length === 0}
                  />
                </div>

                <div className="grid min-w-0 gap-1.5">
                  <Label>颜色（可多选）</Label>
                  <MultiCombobox
                    className="w-full"
                    options={iphoneColorOptions}
                    values={colors}
                    onChange={setColors}
                    placeholder="选择颜色"
                    searchPlaceholder="搜索颜色…"
                    emptyText="没有匹配的颜色"
                    countLabel={(n) => `已选 ${n} 种颜色`}
                    disabled={capacities.length === 0 || iphoneColorOptions.length === 0}
                  />
                </div>
              </>
            ) : (
              <div className="col-span-2 grid min-w-0 gap-1.5">
                <Label>型号</Label>
                <Combobox
                  className="w-full"
                  options={productOptions}
                  value={partNumber}
                  onChange={setPartNumber}
                  placeholder="选择型号"
                  searchPlaceholder="搜索型号…"
                  emptyText="没有匹配的型号"
                  disabled={productOptions.length === 0}
                />
              </div>
            )}

            <Button className="col-span-2 w-full" onClick={() => void onAdd()} disabled={!canAdd}>
              <Plus /> 添加到监控
            </Button>
            <div className="col-span-2 grid gap-1.5 border-t pt-3">
              <Label htmlFor="manual-sku">手动 SKU · 新品尚未入目录时使用</Label>
              <div className="flex gap-2">
                <Input
                  id="manual-sku"
                  value={manualSku}
                  onChange={(event) => setManualSku(event.target.value)}
                  placeholder="例如 MJT74CH/A"
                  spellCheck={false}
                  className="min-w-0"
                />
                <Button variant="outline" onClick={() => void onAddManualSku()} disabled={!canAddManualSku}>
                  添加 SKU
                </Button>
              </div>
              <p className="text-muted-foreground text-xs">沿用上方选中的门店；添加后在任务页选择 API 或 Chrome 加车。</p>
            </div>
            {selectedProducts.length > 1 && (
              // 多选时把要加的规模点明：三级各勾几项之后，组合数并不直观。
              <span
                className="text-muted-foreground col-span-2 text-xs"
                title={selectedProducts.map((p) => p.title).join("、")}
              >
                {selectedProducts.length} 个型号 × {storeNumbers.length} 家门店
              </span>
            )}

            <Tooltip>
              <TooltipTrigger asChild>
                <Button
                  variant="ghost"
                  size="icon"
                  className="col-start-2 justify-self-end"
                  aria-label="从 Apple 官网更新当前品类的型号列表"
                  disabled={ui.refreshing}
                  onClick={() => void refreshProducts()}
                >
                  <RefreshCw className={ui.refreshing ? "animate-spin" : undefined} />
                </Button>
              </TooltipTrigger>
              <TooltipContent>
                从 Apple 官网更新当前品类的型号列表。新机发布后用这个，不必等程序更新。
              </TooltipContent>
            </Tooltip>
          </div>
        </Panel>

        <Panel
          title="运行设置"
          hint={settingsOpen ? "改动即时保存" : settingsSummary(ui.settings)}
          open={settingsOpen}
          onToggle={toggleSettings}
        >
          {/* 两栏：左边是查询与本机提醒，右边是推送地址。推送地址一行一个会占好几行，
              和别的设置叠成一列的话，默认窗口高度下监控列表会被挤得看不见。 */}
          <div className="grid min-w-0 gap-4">
            <div className="grid min-w-0 gap-3">
              <div className="grid gap-1.5">
                {/* 标题行和右栏「推送地址」一样高，两栏的输入框才能对齐。 */}
                <div className="flex h-3.5 items-center gap-x-2">
                  <Label htmlFor="proxy-0" className="shrink-0 whitespace-nowrap">
                    代理地址（可选）
                  </Label>
                  <span className="text-muted-foreground min-w-0 truncate text-xs leading-none">
                    http / https / socks5，一行一个
                  </span>
                  {lastProxyRow.trim() !== "" && (
                    <Button
                      variant="link"
                      size="sm"
                      className="text-muted-foreground hover:text-foreground ml-auto h-3.5 shrink-0 gap-1 p-0 text-xs leading-none has-[>svg]:px-0 [&_svg]:size-3"
                      onClick={() => setProxyDraft([...cleanPushRows(visibleProxyRows), ""])}
                    >
                      <Plus /> 添加代理
                    </Button>
                  )}
                </div>
                {visibleProxyRows.map((row, index) => {
                  // 保存后后端没留下这一行：地址解析不了，这条线路不存在。
                  const rejected =
                    proxyDraft !== null && row.trim() !== "" && !savedProxies.includes(row.trim());
                  return (
                    <div key={index} className="flex items-center gap-2">
                      <Input
                        id={`proxy-${index}`}
                        aria-label={`代理地址 ${index + 1}`}
                        aria-invalid={rejected || undefined}
                        className="select-text"
                        placeholder="socks5://127.0.0.1:1080 或 http://user:pass@host:port"
                        value={row}
                        autoFocus={proxyDraft !== null && row === "" && index > 0}
                        onChange={(e) => editProxyRow(index, e.target.value)}
                        onBlur={() => commitProxyRows(visibleProxyRows)}
                      />
                      {rejected && (
                        <Badge
                          variant="outline"
                          className="text-destructive border-destructive/40 shrink-0"
                          title="地址格式不对，这条线路没有生效。支持 http、https、socks5、socks5h，例如 socks5://127.0.0.1:1080"
                        >
                          未生效
                        </Badge>
                      )}
                      <Button
                        variant="ghost"
                        size="icon"
                        aria-label={`删除代理地址 ${index + 1}`}
                        disabled={visibleProxyRows.length === 1 && row === ""}
                        onClick={() =>
                          commitProxyRows(visibleProxyRows.filter((_, i) => i !== index))
                        }
                      >
                        <Trash2 />
                      </Button>
                    </div>
                  );
                })}
              </div>

              <div className="flex flex-wrap items-center gap-x-5 gap-y-2">
                <div className="flex items-center gap-2">
                  <Label htmlFor="interval" className="whitespace-nowrap">
                    查询间隔
                  </Label>
                  <Input
                    id="interval"
                    type="number"
                    min={5}
                    className="h-8 w-20 select-text"
                    value={intervalValue}
                    onChange={(e) => setIntervalDraft(e.target.valueAsNumber)}
                    onBlur={() => {
                      const s = Number.isFinite(intervalValue) ? Math.round(intervalValue) : 30;
                      setIntervalDraft(null);
                      void setIntervalSeconds(s);
                    }}
                  />
                  <span className="text-muted-foreground text-sm">秒</span>
                </div>
                <div className="flex items-center gap-2">
                  <Switch
                    id="sound"
                    checked={ui.settings.soundEnabled}
                    onCheckedChange={(v) =>
                      void saveSettings({ ...ui.settings, soundEnabled: v })
                    }
                  />
                  <Label htmlFor="sound">提示音</Label>
                </div>
                <div className="flex items-center gap-2">
                  <Switch
                    id="openbag"
                    checked={ui.settings.openBagOnHit}
                    disabled={ui.settings.autoAddToBag}
                    onCheckedChange={(v) =>
                      void saveSettings({ ...ui.settings, openBagOnHit: v })
                    }
                  />
                  <Label htmlFor="openbag">有货时打开购物袋</Label>
                </div>
              </div>
            </div>

            <div className="grid min-w-0 gap-1.5">
              {/* 标题行和左栏的标签一样高，两栏的第一行输入框才能对齐。 */}
              <div className="flex h-3.5 items-center gap-x-2">
                <Label htmlFor="push-0" className="shrink-0 whitespace-nowrap">
                  推送地址（可选）
                </Label>
                <span className="text-muted-foreground min-w-0 truncate text-xs leading-none">
                  Bark、飞书或 Discord webhook，一行一个，自动识别
                </span>
                {lastPushRow.trim() !== "" && (
                  <Button
                    variant="link"
                    size="sm"
                    className="text-muted-foreground hover:text-foreground ml-auto h-3.5 shrink-0 gap-1 p-0 text-xs leading-none has-[>svg]:px-0 [&_svg]:size-3"
                    onClick={() => setPushDraft([...cleanPushRows(visiblePushRows), ""])}
                  >
                    <Plus /> 添加推送地址
                  </Button>
                )}
                {/* 测试的就是这些推送（连同系统通知和提示音），放在它们的标题旁边。 */}
                <Button
                  variant="link"
                  size="sm"
                  className={`text-muted-foreground hover:text-foreground h-3.5 shrink-0 gap-1 p-0 text-xs leading-none has-[>svg]:px-0 [&_svg]:size-3 ${lastPushRow.trim() !== "" ? "" : "ml-auto"}`}
                  onClick={() => void testNotify()}
                >
                  <BellRing /> 测试提醒
                </Button>
              </div>
              {visiblePushRows.map((row, index) => {
                const kind = savedPushKind.get(row.trim());
                return (
                  <div key={index} className="flex items-center gap-2">
                    {/* 渠道标签占固定宽度：还没保存的行留空，输入框照样对齐。 */}
                    <span className="flex w-12 shrink-0 justify-center">
                      {kind !== undefined && (
                        <Badge variant="outline" className="text-muted-foreground">
                          {PUSH_KIND_LABEL[kind]}
                        </Badge>
                      )}
                    </span>
                    <Input
                      id={`push-${index}`}
                      aria-label={`推送地址 ${index + 1}`}
                      className="select-text"
                      placeholder="粘贴 Bark、飞书或 Discord webhook 地址"
                      value={row}
                      // 只有点「添加」新开的那一行自动聚焦；启动时的空行不抢焦点。
                      autoFocus={pushDraft !== null && row === "" && index > 0}
                      onChange={(e) => editPushRow(index, e.target.value)}
                      onBlur={() => commitPushRows(visiblePushRows)}
                    />
                    <Button
                      variant="ghost"
                      size="icon"
                      aria-label={`删除推送地址 ${index + 1}`}
                      disabled={visiblePushRows.length === 1 && row === ""}
                      onClick={() =>
                        commitPushRows(visiblePushRows.filter((_, i) => i !== index))
                      }
                    >
                      <Trash2 />
                    </Button>
                  </div>
                );
              })}
              {savedPush.some((p) => p.kind === "feishu") && (
                // 飞书机器人最常见的配置错误是安全设置，只在真填了飞书地址时提醒。
                <p className="text-muted-foreground pl-14 text-xs">
                  飞书机器人的安全设置请选「自定义关键词」，填「有货」。
                </p>
              )}
            </div>
          </div>
        </Panel>
        </aside>

        <div className="monitor-overview flex flex-wrap items-center gap-2">
          <Stat label="监控" value={ui.rows.length} />
          <Stat label="有货" value={summary.inStock} tone="inStock" />
          <Stat label="无货" value={summary.outOfStock} />
          {summary.untrusted > 0 && (
            // 「其中多少项查不到」单独点出来：这个数字大于 0 时，
            // 表格里那些「无货」也未必反映真实情况。
            <Stat label="查不到" value={summary.untrusted} tone="unknown" />
          )}
          {ui.running && ui.pacing !== null && ui.pacing.paced && (
            // 请求预算在拉长间隔时明说，否则用户会以为设的 30 秒没生效。
            <span className="text-muted-foreground ml-auto text-sm">
              受 Apple 频率限制，下一轮 {ui.pacing.nextCheckInSecs} 秒后
            </span>
          )}
        </div>

        <section className="monitor-list bg-card flex min-h-0 flex-col overflow-hidden rounded-xl border shadow-sm shadow-black/10">
          <div className="flex shrink-0 items-center justify-between border-b px-4 py-3">
            <h3 className="text-sm font-semibold">监控目标</h3>
            <span className="text-muted-foreground text-xs tabular-nums">{ui.rows.length} 个目标</span>
          </div>
          <ScrollArea className="min-h-0 flex-1">
            <Table>
              <TableHeader>
                <TableRow className="hover:bg-transparent">
                  <TableHead className="text-muted-foreground w-24 pl-4 text-xs">状态</TableHead>
                  <TableHead className="text-muted-foreground text-xs">门店</TableHead>
                  <TableHead className="text-muted-foreground text-xs">型号</TableHead>
                  <TableHead className="text-muted-foreground w-24 text-xs">最后检查</TableHead>
                  <TableHead className="w-12" />
                </TableRow>
              </TableHeader>
              <TableBody>
                {ui.rows.length === 0 ? (
                  <TableRow className="hover:bg-transparent">
                    <TableCell colSpan={5} className="text-muted-foreground h-28 text-center">
                      {ui.ready ? "还没有监控目标。在上面选好门店和型号，点「添加」。" : "正在载入…"}
                    </TableCell>
                  </TableRow>
                ) : (
                  ui.rows.map((row) => (
                    <TableRow
                      key={targetKey(row.target)}
                      // 有货的行整行铺一层有货色。这一刻是这个程序存在的全部理由，
                      // 不该只靠一枚小徽标。
                      className={cn(
                        "odd:bg-muted/30",
                        row.availability.kind === "in_stock" &&
                          "bg-in-stock/10 odd:bg-in-stock/10 hover:bg-in-stock/15",
                      )}
                    >
                      <TableCell className="pl-4">
                        <StatusBadge availability={row.availability} />
                      </TableCell>
                      <TableCell className="font-medium">{row.target.storeTitle}</TableCell>
                      <TableCell className="text-muted-foreground">
                        {row.target.productName}
                      </TableCell>
                      <TableCell className="text-muted-foreground tabular-nums">
                        {formatTime(row.lastCheckedMs)}
                      </TableCell>
                      <TableCell>
                        <Button
                          variant="ghost"
                          size="icon"
                          aria-label="删除这条监控"
                          onClick={() => void onRemove(row.target)}
                        >
                          <Trash2 />
                        </Button>
                      </TableCell>
                    </TableRow>
                  ))
                )}
              </TableBody>
            </Table>
          </ScrollArea>
        </section>

        </main>
        )}

        {activeTab === "profile" && (
          <main
            id="workspace-panel-profile"
            role="tabpanel"
            aria-labelledby="workspace-tab-profile"
            className="workspace-panel min-h-0 flex-1 overflow-y-auto pr-1"
          >
            <div className="mb-4 flex items-end justify-between gap-3">
              <div>
                <p className="text-primary text-[10px] font-semibold tracking-[0.18em]">CHECKOUT IDENTITY</p>
                <h2 className="text-xl font-semibold tracking-tight">Profile</h2>
              </div>
              <p className="text-muted-foreground text-xs">独立 Chrome · 本机保存</p>
            </div>
            <div className="grid gap-4 pb-5 lg:grid-cols-3">
              <section className="bg-card border-primary/25 relative overflow-hidden rounded-xl border p-6 shadow-lg shadow-black/10 lg:col-span-3">
                <div className="bg-primary/10 pointer-events-none absolute -top-24 -right-12 size-72 rounded-full blur-3xl" />
                <div className="relative grid gap-6 lg:grid-cols-[minmax(0,1fr)_auto] lg:items-center">
                  <div>
                    <p className="text-primary mb-2 text-xs font-semibold tracking-wide">结账资料中心</p>
                    <h3 className="text-2xl font-semibold tracking-tight">准备好你的取货和配送资料</h3>
                    <p className="text-muted-foreground mt-2 max-w-2xl text-sm leading-6">
                      资料保存在独立浏览器的扩展中。监控命中后，任务会使用这份 Profile 填写 Apple 结账表单。
                    </p>
                  </div>
                  <div className="flex flex-wrap gap-2 lg:max-w-80 lg:justify-end">
                    <Button disabled={browserPreparing} onClick={() => void onPrepareBrowser()}>
                      {browserPreparing ? "正在准备…" : "准备独立浏览器"}
                    </Button>
                    <Button variant="secondary" disabled={browserPreparing} onClick={() => void openBotBrowser().then(setProfileFeedback)}>打开 Profile 编辑器</Button>
                  </div>
                </div>
                {profileFeedback && <p className="relative mt-4 text-sm" role="status" aria-live="polite">{profileFeedback}</p>}
              </section>

              <section className="bg-card rounded-xl border p-5 shadow-sm shadow-black/10">
                <span className="text-primary text-xs font-semibold">01 / PICKUP</span>
                <div className="mt-3 flex items-center gap-2"><UserRound className="text-primary size-4" /><h3 className="font-semibold">取货联系人</h3></div>
                <p className="text-muted-foreground mt-2 text-sm leading-6">
                  姓名、手机号和邮箱。证件后四位与短信验证仍在 Apple 页面完成。
                </p>
              </section>

              <section className="bg-card rounded-xl border p-5 shadow-sm shadow-black/10">
                <span className="text-primary text-xs font-semibold">02 / DELIVERY</span>
                <div className="mt-3 flex items-center gap-2"><UserRound className="text-primary size-4" /><h3 className="font-semibold">配送收货资料</h3></div>
                <p className="text-muted-foreground mt-2 text-sm leading-6">
                  配送联系人、地址和邮编。无法确认的选项留给你在 Apple 页面选择。
                </p>
              </section>

              <section className="bg-card rounded-xl border p-5 shadow-sm shadow-black/10">
                <span className="text-primary text-xs font-semibold">03 / SAFETY</span>
                <h3 className="mt-3 font-semibold">下单上限与连接</h3>
                <p className="text-muted-foreground mt-2 text-sm leading-6">
                  扩展中设置自动下单开关与单笔金额上限。连接码用于关联当前桌面任务。
                </p>
                <div className="mt-3 flex flex-wrap gap-2">
                  <Button variant="outline" size="sm" onClick={() => void copyBridgePairingCode().then(setProfileFeedback)}>复制扩展连接码</Button>
                  <Button variant="ghost" size="sm" onClick={() => void openExtensionFolder()}>查看扩展目录</Button>
                </div>
              </section>
            </div>
          </main>
        )}

        {activeTab === "tasks" && (
          <main
            id="workspace-panel-tasks"
            role="tabpanel"
            aria-labelledby="workspace-tab-tasks"
            className="workspace-panel tasks-workspace min-h-0 flex-1"
          >
            <div className="tasks-heading flex items-end justify-between gap-3">
              <div>
                <p className="text-primary text-[10px] font-semibold tracking-[0.18em]">AUTOMATION</p>
                <h2 className="text-xl font-semibold tracking-tight">任务</h2>
              </div>
              <p className="text-muted-foreground text-xs">发售待命 / 补货监听 → 加车 → 结账 → 支付宝</p>
            </div>
            <section className="tasks-control bg-card rounded-xl border px-4 py-3 shadow-sm shadow-black/10">
              <div className="flex flex-wrap items-center gap-3">
                <Switch
                  id="autoadd"
                  checked={ui.settings.autoAddToBag}
                  disabled={taskSaving}
                  onCheckedChange={(value) => void onAutoAddChange(value)}
                />
                <Label htmlFor="autoadd" className="font-medium">启用自动任务</Label>
                <Button variant="outline" size="sm" className="ml-auto" onClick={() => selectTab("profile")}>设置 Profile</Button>
              </div>
              {taskFeedback && <p role="alert" className="text-destructive mt-2 text-sm">{taskFeedback}</p>}
              <p className="text-muted-foreground mt-2 text-xs">
                每轮最多执行一个购买任务。发售模式提前 10 分钟打开独立浏览器等待 Apple 放行；补货模式在库存命中时启动。加车方式可逐项选择。
              </p>
            </section>

            <section className="tasks-list bg-card flex min-h-0 flex-col overflow-hidden rounded-xl border shadow-sm shadow-black/10">
              <div className="border-b px-4 py-3">
                <h2 className="font-semibold">任务队列</h2>
                <p className="text-muted-foreground text-xs">每条任务可单独选择触发模式、加车方式、开售时间和重试延迟。先到的任务占用本轮执行名额。</p>
              </div>
              <ScrollArea className="min-h-0 flex-1">
                <div className="grid gap-3 p-3">
                  {ui.rows.length === 0 ? (
                    <p className="text-muted-foreground py-10 text-center text-sm">尚无任务候选。请到监控页添加目标。</p>
                  ) : ui.rows.map((row) => {
                    const key = targetKey(row.target);
                    const plan = (ui.settings.taskPlans ?? []).find((item) => item.targetKey === key);
                    const mode = plan?.mode ?? "restock";
                    const addMethod = plan?.addMethod ?? "browser";
                    const current = ui.botStatus?.partNumber === row.target.partNumber &&
                      ui.botStatus.storeNumber === row.target.storeNumber;
                    const taskLabel = current && ui.botStatus
                      ? BOT_STAGE_LABEL[ui.botStatus.stage]
                      : !ui.settings.autoAddToBag ? "自动任务未启用"
                        : mode === "release" ? !plan?.releaseAtMs ? "请填写开售时间"
                          : plan.releaseAtMs + 4 * 60 * 60 * 1000 < Date.now() ? "本次发售窗口已过"
                            : "等待发售时段" : "等待库存命中";
                    return (
                      <article key={key} className="bg-muted/20 rounded-lg border p-4">
                        <div className="flex flex-wrap items-start justify-between gap-3">
                          <div className="min-w-0">
                            <p className="truncate text-sm font-semibold" title={row.target.productName}>{row.target.productName}</p>
                            <p className="text-muted-foreground mt-1 text-xs">{row.target.storeTitle} · {row.target.partNumber}</p>
                          </div>
                          <StatusBadge availability={row.availability} />
                        </div>
                        <div className="mt-4 grid gap-3 sm:grid-cols-2 2xl:grid-cols-[minmax(130px,0.8fr)_minmax(130px,0.8fr)_minmax(190px,1.3fr)_minmax(130px,0.8fr)]">
                          <div>
                            <Label htmlFor={`task-mode-${key}`} className="text-muted-foreground text-xs">任务模式</Label>
                            <select
                              id={`task-mode-${key}`}
                              className="border-input bg-background mt-1 h-9 w-full rounded-md border px-2 text-sm"
                              value={mode}
                              disabled={taskSaving}
                              onChange={(event) => void onTaskPlanChange(row.target, { mode: event.target.value as TaskPlan["mode"] })}
                            >
                              <option value="restock">Restock · 补货</option>
                              <option value="release">Release · 发售</option>
                            </select>
                          </div>
                          <div>
                            <Label htmlFor={`task-add-method-${key}`} className="text-muted-foreground text-xs">加车方式</Label>
                            <select
                              id={`task-add-method-${key}`}
                              className="border-input bg-background mt-1 h-9 w-full rounded-md border px-2 text-sm"
                              value={addMethod}
                              disabled={taskSaving}
                              onChange={(event) => void onTaskPlanChange(row.target, { addMethod: event.target.value as TaskPlan["addMethod"] })}
                            >
                              <option value="browser">Chrome · 页面操作</option>
                              <option value="api">API · 直接请求</option>
                            </select>
                          </div>
                          <div>
                            <Label htmlFor={`task-release-${key}`} className="text-muted-foreground text-xs">开售时间 · 本地时区</Label>
                            <Input
                              id={`task-release-${key}`}
                              type="datetime-local"
                              className="mt-1 h-9"
                              value={releaseDrafts[key] ?? localDateTime(plan?.releaseAtMs)}
                              disabled={taskSaving || mode !== "release"}
                              onChange={(event) => setReleaseDrafts({ ...releaseDrafts, [key]: event.target.value })}
                              onBlur={(event) => {
                                const value = event.target.value;
                                if (value !== localDateTime(plan?.releaseAtMs)) saveReleaseTime(row.target, value);
                              }}
                            />
                          </div>
                          <div>
                            <Label htmlFor={`task-retry-${key}`} className="text-muted-foreground text-xs">Retry delay · 毫秒</Label>
                            <Input
                              id={`task-retry-${key}`}
                              type="number"
                              min={1000}
                              max={60000}
                              step={1000}
                              className="mt-1 h-9"
                              value={retryDrafts[key] ?? String(plan?.retryDelayMs ?? 3000)}
                              disabled={taskSaving}
                              onChange={(event) => setRetryDrafts({ ...retryDrafts, [key]: event.target.value })}
                              onBlur={(event) => {
                                const value = event.target.value;
                                if (value !== String(plan?.retryDelayMs ?? 3000)) saveRetryDelay(row.target, value);
                              }}
                            />
                          </div>
                        </div>
                        {addMethod === "api" && (
                          <div className="mt-3">
                            <Label htmlFor={`task-product-url-${key}`} className="text-muted-foreground text-xs">Apple 商品或机型 URL · 可选，留空按 SKU 解析</Label>
                            <Input
                              id={`task-product-url-${key}`}
                              type="url"
                              placeholder="https://www.apple.com.cn/shop/buy-iphone/…"
                              className="mt-1 h-9"
                              defaultValue={plan?.productUrl ?? ""}
                              disabled={taskSaving}
                              onBlur={(event) => {
                                const value = event.target.value.trim();
                                if (value !== (plan?.productUrl ?? "")) void onTaskPlanChange(row.target, { productUrl: value || null });
                              }}
                            />
                            <p className="text-muted-foreground mt-1 text-xs">机型页会与本任务 SKU 组合成具体商品页。API 模式使用独立 HTTP 会话加车；成功后在独立浏览器核对购物袋并继续结账。Apple 验证或拒绝请求时会停止。</p>
                          </div>
                        )}
                        <p className="text-muted-foreground mt-3 text-xs" role="status">{taskLabel}</p>
                      </article>
                    );
                  })}
                </div>
              </ScrollArea>
            </section>

        <div className="tasks-side flex min-h-0 flex-col gap-3 overflow-y-auto pr-1">
        {ui.botStatus && (
          <section className="bg-card shrink-0 rounded-xl border p-4 shadow-xs" aria-live="polite">
            <h2 className="font-semibold">最近自动结账任务</h2>
            <p className="text-sm">{BOT_STAGE_LABEL[ui.botStatus.stage]}</p>
            <p className="text-muted-foreground text-xs">
              {ui.botStatus.partNumber} · 门店 {ui.botStatus.storeNumber} · {formatTime(ui.botStatus.updatedAt * 1000)}
            </p>
          </section>
        )}

        {currentPaymentLink && (
          <section className="bg-card shrink-0 rounded-xl border p-4 shadow-xs">
            <div className="flex flex-wrap items-center justify-between gap-2">
              <div>
                <h2 className="font-semibold">最近收到的支付宝付款链接</h2>
                <p className="text-muted-foreground text-xs">
                  {currentPaymentLink.kind === "qr" ? "二维码目标地址可能很快失效" : "支付宝收银台页面"}；付款前请核对商家与金额。
                </p>
              </div>
              <Button variant="outline" size="sm" onClick={() => void copyPaymentLink(currentPaymentLink.url)}>
                复制链接
              </Button>
            </div>
            <a className="text-primary mt-2 block truncate text-sm underline" href={currentPaymentLink.url} target="_blank" rel="noreferrer">
              {currentPaymentLink.url}
            </a>
          </section>
        )}

        {!currentPaymentLink && (
          <section className="bg-card rounded-xl border p-4 shadow-xs">
            <h2 className="font-semibold">支付宝付款链接</h2>
            <p className="text-muted-foreground mt-1 text-sm">提交 Apple 订单并进入支付宝后，链接会显示在这里。</p>
          </section>
        )}
        </div>

        <section className="tasks-log bg-card flex min-h-0 flex-col overflow-hidden rounded-xl border shadow-sm shadow-black/10">
          <div className="border-b px-4 py-2 text-xs font-semibold">运行日志</div>
          <ScrollArea className="min-h-0 flex-1">
            <div className="p-3 font-mono text-xs leading-5 select-text">
              {ui.logs.length === 0 ? (
                <p className="text-muted-foreground">日志会显示在这里。</p>
              ) : (
                ui.logs.map((line, i) => <LogLine key={i} line={line} />)
              )}
            </div>
          </ScrollArea>
        </section>
          </main>
        )}
      </div>
    </TooltipProvider>
  );
}
