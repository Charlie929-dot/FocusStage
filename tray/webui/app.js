(() => {
  'use strict';

  const FOLDER_PREFIX = 'folder:';
  const q = String.fromCharCode(34);
  const fontName = (name) => `${q}${name}${q}`;

  // ---------------- WebView2 RPC Bridge ----------------
  let rpcSeq = 0;
  const pendingRpc = new Map();
  const eventListeners = new Map();

  const bridge = {
    invoke(method, params = {}) {
      if (!window.chrome || !window.chrome.webview) {
        return Promise.reject(new Error('WebView2 bridge unavailable'));
      }
      const id = String(++rpcSeq);
      return new Promise((resolve, reject) => {
        pendingRpc.set(id, { resolve, reject });
        window.chrome.webview.postMessage({ id, method, params });
      });
    },
    on(eventName, handler) {
      if (!eventListeners.has(eventName)) eventListeners.set(eventName, new Set());
      eventListeners.get(eventName).add(handler);
    }
  };

  if (window.chrome && window.chrome.webview) {
    window.chrome.webview.addEventListener('message', (event) => {
      const msg = typeof event.data === 'string' ? JSON.parse(event.data) : event.data;
      if (!msg || typeof msg !== 'object') return;
      if (msg.type === 'response' && msg.id != null) {
        const slot = pendingRpc.get(String(msg.id));
        if (!slot) return;
        pendingRpc.delete(String(msg.id));
        if (msg.ok) slot.resolve(msg.result);
        else slot.reject(new Error(msg.error || '操作失败'));
      } else if (msg.type === 'event' && msg.event) {
        const set = eventListeners.get(msg.event);
        if (set) for (const fn of set) fn(msg.payload);
      }
    });
  }

  // ---------------- Design Tokens ----------------
  const THEMES = [
    {
      id: 'mist', name: '云幕', tag: '清冷 · 默认',
      colors: { background: '#e9edf3', surface: '#f9fbfd', ink: '#1b263b', accent: '#3d66d5' },
      signals: { emerald: '#059669', agent: '#7c3aed', amber: '#b45309', danger: '#dc2626' }
    },
    {
      id: 'ink', name: '幕布', tag: '深色 · 石墨蓝',
      colors: { background: '#101725', surface: '#182133', ink: '#e8eef8', accent: '#8ba8ff' },
      signals: { emerald: '#34d399', agent: '#a78bfa', amber: '#fbbf24', danger: '#fb7185' }
    },
    {
      id: 'paper', name: '纸墨', tag: '暖白 · 赭石',
      colors: { background: '#efe9dd', surface: '#faf7ef', ink: '#302b24', accent: '#a15d3b' },
      signals: { emerald: '#2f7d5b', agent: '#7651a8', amber: '#a16207', danger: '#b44034' }
    },
    {
      id: 'forest', name: '深港', tag: '深色 · 松绿',
      colors: { background: '#0e1d1a', surface: '#162925', ink: '#e4f2ea', accent: '#5fc2a2' },
      signals: { emerald: '#6ee7b7', agent: '#c4a7ff', amber: '#fcd34d', danger: '#fb7185' }
    },
    {
      id: 'violet', name: '夜幕', tag: '深色 · 紫晶',
      colors: { background: '#19162b', surface: '#241f3c', ink: '#eeeaff', accent: '#b79aff' },
      signals: { emerald: '#5eead4', agent: '#c4b5fd', amber: '#fcd34d', danger: '#fb7185' }
    }
  ];

  const FONT_PRESETS = {
    system: {
      ui: `${fontName('Segoe UI Variable Text')}, ${fontName('Segoe UI')}, ${fontName('Microsoft YaHei UI')}, system-ui, sans-serif`,
      heading: `${fontName('Segoe UI Variable Display')}, ${fontName('Segoe UI')}, ${fontName('Microsoft YaHei UI')}, system-ui, sans-serif`,
      mono: `${fontName('Cascadia Mono')}, ${fontName('Consolas')}, monospace`
    },
    rounded: {
      ui: `${fontName('MiSans')}, ${fontName('HarmonyOS Sans SC')}, ${fontName('Microsoft YaHei UI')}, ${fontName('Segoe UI')}, sans-serif`,
      heading: `${fontName('MiSans')}, ${fontName('HarmonyOS Sans SC')}, ${fontName('Microsoft YaHei UI')}, sans-serif`,
      mono: `${fontName('Cascadia Mono')}, ${fontName('Consolas')}, monospace`
    },
    serif: {
      ui: `${fontName('Noto Serif CJK SC')}, ${fontName('Source Han Serif SC')}, ${fontName('Songti SC')}, ${fontName('SimSun')}, serif`,
      heading: `${fontName('Noto Serif CJK SC')}, ${fontName('Source Han Serif SC')}, ${fontName('Songti SC')}, serif`,
      mono: `${fontName('Sarasa Mono SC')}, ${fontName('Cascadia Mono')}, monospace`
    },
    mono: {
      ui: `${fontName('Cascadia Code')}, ${fontName('Sarasa Mono SC')}, ${fontName('Consolas')}, monospace`,
      heading: `${fontName('Cascadia Code')}, ${fontName('Sarasa Mono SC')}, ${fontName('Consolas')}, monospace`,
      mono: `${fontName('Cascadia Code')}, ${fontName('Consolas')}, monospace`
    },
    custom: {
      ui: '',
      heading: '',
      mono: `${fontName('Cascadia Mono')}, ${fontName('Consolas')}, monospace`
    }
  };

  const DEFAULT_APPEARANCE = {
    theme: 'mist',
    customColors: false,
    background: '#e9edf3',
    surface: '#f9fbfd',
    ink: '#1b263b',
    accent: '#3d66d5',
    fontPreset: 'system',
    customFont: '',
    fontSize: 13,
    columns: 8,
    tileScale: 100,
    showGrid: false,
    showDock: true,
    dockLabels: true
  };

  const DOCK_TARGETS = [
    { kind: 'view', target: 'view:stages', label: '舞台中心', group: '设置' },
    { kind: 'view', target: 'view:daemon', label: '守护设置', group: '设置' },
    { kind: 'view', target: 'view:linux', label: 'Linux 设置', group: '设置' },
    { kind: 'view', target: 'view:appearance', label: '外观设置', group: '设置' },
    { kind: 'action', target: 'action:appCenter', label: '应用中心', group: '功能' },
    { kind: 'action', target: 'action:newFolder', label: '新建文件夹', group: '功能' },
    { kind: 'action', target: 'action:runCommand', label: '运行命令', group: '功能' },
    { kind: 'action', target: 'action:refreshApps', label: '刷新应用', group: '功能' },
    { kind: 'action', target: 'action:autoOrganize', label: '整理桌面', group: '功能' },
    { kind: 'action', target: 'action:syncStartMenu', label: '同步快捷方式', group: '功能' }
  ];

  // ---------------- Application State ----------------
  const state = {
    distro: 'Ubuntu',
    daemonAlive: false,
    startupEnabled: false,
    clipboardSyncEnabled: true,
    hotkeyRegistered: true,
    maximized: false,
    stages: [],
    apps: [],
    appById: new Map(),
    layout: normalizeLayout(null),
    activeView: 'apps',
    activeTab: 'stages',
    searchQuery: '',
    selectedKey: null,
    openFolderId: null,
    linuxSettings: null,
    linuxSettingsBusy: false,
    dragPayload: null,
    dropTarget: null,
    dropMode: null,
    appsLoaded: false
  };

  // ---------------- DOM References ----------------
  const $ = (id) => document.getElementById(id);
  const els = {
    daemonPillDot: $('daemon-pill-dot'),
    navApps: $('nav-apps'),
    navControl: $('nav-control'),
    navStageBadge: $('nav-stage-badge'),
    searchWrap: $('search-wrap'),
    searchInput: $('search-input'),
    searchKbd: $('search-kbd'),
    searchClear: $('search-clear'),
    btnAppearance: $('btn-appearance'),
    btnMinimize: $('btn-minimize'),
    btnMaximize: $('btn-maximize'),
    iconMaximize: $('icon-maximize'),
    iconRestore: $('icon-restore'),
    btnClose: $('btn-close'),

    viewApps: $('view-apps'),
    viewControl: $('view-control'),
    appsScroll: $('apps-scroll'),
    appsGrid: $('apps-grid'),
    appsEmpty: $('apps-empty'),
    desktopHint: $('desktop-hint'),
    dockContainer: $('dock-container'),
    dock: $('dock'),

    sidebarStageCount: $('sidebar-stage-count'),
    sidebarDaemonDot: $('sidebar-daemon-dot'),
    sidebarDaemonSub: $('sidebar-daemon-sub'),
    btnSidebarDaemon: $('btn-sidebar-daemon'),

    stagesSubtitle: $('stages-subtitle'),
    btnRefreshStages: $('btn-refresh-stages'),
    btnTakeoverAll: $('btn-takeover-all'),
    stagesSummary: $('stages-summary'),
    stagesList: $('stages-list'),

    daemonDetailText: $('daemon-detail-text'),
    btnDaemonToggle: $('btn-daemon-toggle'),
    hotkeyDetailText: $('hotkey-detail-text'),
    hotkeyStatusPill: $('hotkey-status-pill'),
    toggleClipboard: $('toggle-clipboard'),
    toggleStartup: $('toggle-startup'),
    btnRefreshApps: $('btn-refresh-apps'),
    btnAutoOrganize: $('btn-auto-organize'),
    btnSyncStartMenu: $('btn-sync-startmenu'),

    btnLinuxReload: $('btn-linux-reload'),
    btnLinuxSync: $('btn-linux-sync'),
    selectBrowser: $('select-browser'),
    selectTerminal: $('select-terminal'),
    windowsHostHint: $('windows-host-hint'),
    healthXdg: $('health-xdg'),
    healthPortal: $('health-portal'),
    healthMimeapps: $('health-mimeapps'),
    protocolsList: $('protocols-list'),

    themeGrid: $('theme-grid'),
    toggleCustomColors: $('toggle-custom-colors'),
    colorGrid: $('color-grid'),
    colorBackground: $('color-background'),
    colorSurface: $('color-surface'),
    colorInk: $('color-ink'),
    colorAccent: $('color-accent'),
    selectFontPreset: $('select-font-preset'),
    inputCustomFont: $('input-custom-font'),
    rangeFontSize: $('range-font-size'),
    outputFontSize: $('output-font-size'),
    rangeColumns: $('range-columns'),
    outputColumns: $('output-columns'),
    rangeTileScale: $('range-tile-scale'),
    outputTileScale: $('output-tile-scale'),
    toggleShowGrid: $('toggle-show-grid'),
    toggleShowDock: $('toggle-show-dock'),
    toggleDockLabels: $('toggle-dock-labels'),
    btnManageDock: $('btn-manage-dock'),
    btnResetAppearance: $('btn-reset-appearance'),

    folderOverlay: $('folder-overlay'),
    folderCard: $('folder-card'),
    folderModalTitle: $('folder-modal-title'),
    folderModalCount: $('folder-modal-count'),
    folderGrid: $('folder-grid'),
    btnFolderRename: $('btn-folder-rename'),
    btnFolderStartMenu: $('btn-folder-startmenu'),
    btnFolderDissolve: $('btn-folder-dissolve'),
    btnFolderClose: $('btn-folder-close'),

    modalOverlay: $('modal-overlay'),
    modalTitle: $('modal-title'),
    modalBody: $('modal-body'),
    modalFooter: $('modal-footer'),
    modalClose: $('modal-close'),

    contextMenu: $('context-menu'),
    toast: $('toast'),
    toastText: $('toast-text')
  };

  // ---------------- Toast ----------------
  let toastTimer = null;
  function showToast(message, tone = 'info') {
    if (!message) return;
    els.toastText.textContent = message;
    els.toast.className = `toast tone-${tone}`;
    if (toastTimer) clearTimeout(toastTimer);
    toastTimer = setTimeout(() => els.toast.classList.add('is-hidden'), 3200);
  }

  // ---------------- Window Drag & Controls ----------------
  document.addEventListener('mousedown', (e) => {
    if (e.button !== 0) return;
    const dragRegion = e.target.closest('[data-drag-region]');
    if (!dragRegion) return;
    if (e.target.closest('button, input, select, a, .mode-switch, .search-wrap')) return;
    bridge.invoke('window.drag').catch(() => {});
  });

  document.addEventListener('dblclick', (e) => {
    const dragRegion = e.target.closest('[data-drag-region]');
    if (!dragRegion) return;
    if (e.target.closest('button, input, select, a, .mode-switch, .search-wrap')) return;
    bridge.invoke('window.toggleMaximize').catch(() => {});
  });

  els.btnMinimize.addEventListener('click', () => bridge.invoke('window.minimize'));
  els.btnMaximize.addEventListener('click', () => bridge.invoke('window.toggleMaximize'));
  els.btnClose.addEventListener('click', () => {
    closeContextMenu();
    closeModal();
    closeFolder();
    bridge.invoke('window.hide');
  });
  els.btnAppearance.addEventListener('click', () => setView('control', 'appearance'));

  function updateMaximizedUi(maximized) {
    state.maximized = Boolean(maximized);
    els.iconMaximize.classList.toggle('is-hidden', state.maximized);
    els.iconRestore.classList.toggle('is-hidden', !state.maximized);
    els.btnMaximize.title = state.maximized ? '还原' : '最大化';
  }

  // ---------------- Appearance Helpers ----------------
  function themeById(id) {
    return THEMES.find((t) => t.id === id) || THEMES[0];
  }

  function clampInt(value, min, max, fallback) {
    const n = Number.parseInt(String(value), 10);
    if (Number.isNaN(n)) return fallback;
    return Math.min(max, Math.max(min, n));
  }

  function normalizeHex(value, fallback) {
    let text = String(value || '').trim();
    if (!text.startsWith('#')) text = '#' + text;
    if (/^#[0-9a-f]{6}$/i.test(text)) return text.toLowerCase();
    if (/^#[0-9a-f]{3}$/i.test(text)) {
      return ('#' + text[1] + text[1] + text[2] + text[2] + text[3] + text[3]).toLowerCase();
    }
    return fallback;
  }

  function hexToRgb(hex) {
    const value = normalizeHex(hex, '#000000').slice(1);
    return [
      Number.parseInt(value.slice(0, 2), 16),
      Number.parseInt(value.slice(2, 4), 16),
      Number.parseInt(value.slice(4, 6), 16)
    ];
  }

  function mixHex(a, b, amountB) {
    const ca = hexToRgb(a);
    const cb = hexToRgb(b);
    const mixed = ca.map((v, i) => Math.round(v * (1 - amountB) + cb[i] * amountB));
    return '#' + mixed.map((v) => v.toString(16).padStart(2, '0')).join('');
  }

  function rgba(rgb, alpha) {
    return `rgba(${rgb[0]}, ${rgb[1]}, ${rgb[2]}, ${alpha})`;
  }

  function luminance(hex) {
    const [r, g, b] = hexToRgb(hex).map((v) => {
      const c = v / 255;
      return c <= 0.03928 ? c / 12.92 : Math.pow((c + 0.055) / 1.055, 2.4);
    });
    return 0.2126 * r + 0.7152 * g + 0.0722 * b;
  }

  function readableOn(hex) {
    return luminance(hex) > 0.55 ? '#101725' : '#ffffff';
  }

  function normalizeAppearance(raw) {
    const input = raw && typeof raw === 'object' ? raw : {};
    const a = { ...DEFAULT_APPEARANCE, ...input };
    const preset = themeById(a.theme);
    a.theme = preset.id;
    a.background = normalizeHex(a.background, preset.colors.background);
    a.surface = normalizeHex(a.surface, preset.colors.surface);
    a.ink = normalizeHex(a.ink, preset.colors.ink);
    a.accent = normalizeHex(a.accent, preset.colors.accent);
    a.customColors = Boolean(a.customColors);
    a.fontPreset = FONT_PRESETS[a.fontPreset] ? a.fontPreset : 'system';
    a.customFont = String(a.customFont || '').trim().slice(0, 180);
    a.fontSize = clampInt(a.fontSize, 11, 18, 13);
    a.columns = clampInt(a.columns, 6, 12, 8);
    a.tileScale = clampInt(a.tileScale, 80, 130, 100);
    a.showGrid = Boolean(a.showGrid);
    a.showDock = Boolean(a.showDock);
    a.dockLabels = Boolean(a.dockLabels);
    return a;
  }

  function applyAppearance(rawAppearance) {
    const a = normalizeAppearance(rawAppearance);
    state.layout.appearance = a;
    const preset = themeById(a.theme);
    const base = a.customColors
      ? { background: a.background, surface: a.surface, ink: a.ink, accent: a.accent }
      : preset.colors;

    const bgRgb = hexToRgb(base.background);
    const surfaceRgb = hexToRgb(base.surface);
    const inkRgb = hexToRgb(base.ink);
    const accentRgb = hexToRgb(base.accent);
    const isDark = luminance(base.background) < 0.45;
    const surfaceRaised = isDark
      ? mixHex(base.surface, '#ffffff', 0.07)
      : mixHex(base.surface, '#ffffff', 0.45);
    const surfaceSubtle = mixHex(base.surface, base.background, isDark ? 0.35 : 0.45);
    const inkSecondary = mixHex(base.ink, base.surface, 0.3);
    const inkMuted = mixHex(base.ink, base.surface, 0.5);
    const accentSoft = mixHex(base.surface, base.accent, isDark ? 0.22 : 0.12);
    const accentHover = isDark
      ? mixHex(base.accent, '#ffffff', 0.14)
      : mixHex(base.accent, base.ink, 0.18);

    const root = document.documentElement;
    root.dataset.theme = a.theme;
    root.style.setProperty('--bg-canvas', base.background);
    root.style.setProperty('--surface', base.surface);
    root.style.setProperty('--surface-raised', surfaceRaised);
    root.style.setProperty('--surface-subtle', surfaceSubtle);
    root.style.setProperty('--surface-glass', rgba(surfaceRgb, 0.88));
    root.style.setProperty('--titlebar-bg', rgba(surfaceRgb, 0.78));
    root.style.setProperty('--dock-bg', rgba(surfaceRgb, 0.9));
    root.style.setProperty('--overlay-card-bg', rgba(surfaceRgb, 0.96));
    root.style.setProperty('--overlay-bg', isDark ? 'rgba(0, 0, 0, 0.55)' : rgba(inkRgb, 0.32));
    root.style.setProperty('--slot-bg', rgba(surfaceRgb, isDark ? 0.045 : 0.18));
    root.style.setProperty('--ink', base.ink);
    root.style.setProperty('--ink-secondary', inkSecondary);
    root.style.setProperty('--ink-muted', inkMuted);
    root.style.setProperty('--ink-rgb', inkRgb.join(', '));
    root.style.setProperty('--surface-rgb', surfaceRgb.join(', '));
    root.style.setProperty('--accent-rgb', accentRgb.join(', '));
    root.style.setProperty('--shadow-rgb', isDark ? '0, 0, 0' : inkRgb.join(', '));
    root.style.setProperty('--accent', base.accent);
    root.style.setProperty('--accent-hover', accentHover);
    root.style.setProperty('--accent-soft', accentSoft);
    root.style.setProperty('--accent-border', rgba(accentRgb, 0.35));
    root.style.setProperty('--on-accent', readableOn(base.accent));
    root.style.setProperty('--border', rgba(inkRgb, isDark ? 0.14 : 0.1));
    root.style.setProperty('--border-strong', rgba(inkRgb, isDark ? 0.26 : 0.2));
    root.style.setProperty('--border-soft', rgba(inkRgb, isDark ? 0.1 : 0.06));
    root.style.setProperty('--grid-line', rgba(inkRgb, isDark ? 0.09 : 0.055));
    root.style.setProperty('--grid-line-strong', rgba(accentRgb, 0.4));
    root.style.setProperty('--canvas-grid', rgba(inkRgb, isDark ? 0.035 : 0.022));
    root.style.setProperty('--canvas-glow', rgba(accentRgb, isDark ? 0.11 : 0.075));

    for (const [key, color] of Object.entries(preset.signals)) {
      const rgb = hexToRgb(color);
      root.style.setProperty(`--${key}`, color);
      root.style.setProperty(`--${key}-soft`, rgba(rgb, isDark ? 0.16 : 0.1));
      root.style.setProperty(`--${key}-border`, rgba(rgb, 0.3));
    }

    const font = FONT_PRESETS[a.fontPreset];
    const customStack = a.customFont || FONT_PRESETS.system.ui;
    root.style.setProperty('--font-ui', a.fontPreset === 'custom' ? customStack : font.ui);
    root.style.setProperty('--font-heading', a.fontPreset === 'custom' ? customStack : font.heading);
    root.style.setProperty('--font-mono', font.mono || FONT_PRESETS.system.mono);
    root.style.setProperty('--app-font-size', `${a.fontSize}px`);
    root.style.setProperty('--grid-cols', String(a.columns));
    root.style.setProperty('--tile-scale', String(a.tileScale / 100));

    document.body.classList.toggle('hide-grid', !a.showGrid);
    document.body.classList.toggle('hide-dock', !a.showDock);
    els.dock.classList.toggle('no-labels', !a.dockLabels);
    els.dockContainer.classList.toggle('is-hidden', !a.showDock);
  }

  function normalizeLayout(raw) {
    const layout = raw && typeof raw === 'object' ? raw : {};
    return {
      folders: Array.isArray(layout.folders)
        ? layout.folders.map((f) => ({
            id: String(f.id || ''),
            name: String(f.name || '文件夹'),
            appIds: Array.isArray(f.appIds) ? f.appIds.map(String) : []
          }))
        : [],
      rootOrder: Array.isArray(layout.rootOrder) ? layout.rootOrder.map(String) : [],
      positions: layout.positions && typeof layout.positions === 'object' ? { ...layout.positions } : {},
      dockItems: Array.isArray(layout.dockItems)
        ? layout.dockItems.map((d) => ({
            id: String(d.id || ''),
            kind: String(d.kind || 'app'),
            target: String(d.target || ''),
            label: String(d.label || '')
          }))
        : [],
      appearance: normalizeAppearance(layout.appearance)
    };
  }

  function setLayout(layout) {
    state.layout = normalizeLayout(layout);
    applyAppearance(state.layout.appearance);
  }

  function renderLayoutViews() {
    renderAppsGrid();
    renderDock();
    renderAppearanceSettings();
    if (state.openFolderId) renderFolderModal();
  }

  function applyLayoutPayload(layout) {
    setLayout(layout);
    renderLayoutViews();
  }

  function applyAppsPayload(payload) {
    state.apps = Array.isArray(payload.apps) ? payload.apps : [];
    state.appById = new Map(state.apps.map((a) => [a.id, a]));
    state.appsLoaded = true;
    setLayout(payload.layout || state.layout);
    renderLayoutViews();
    if (state.activeView === 'control' && state.activeTab === 'stages') renderStages();
  }

  // ---------------- View & Search ----------------
  function setView(viewName, controlTab = null) {
    state.activeView = viewName;
    closeContextMenu();
    els.navApps.classList.toggle('is-active', viewName === 'apps');
    els.navControl.classList.toggle('is-active', viewName === 'control');
    els.viewApps.classList.toggle('is-active', viewName === 'apps');
    els.viewControl.classList.toggle('is-active', viewName === 'control');
    els.searchWrap.classList.toggle('is-hidden', viewName !== 'apps');

    if (viewName === 'control') {
      closeFolder();
      if (controlTab) setControlTab(controlTab);
      if (state.activeTab === 'linux' && !state.linuxSettings) loadLinuxSettings();
      renderControlCenter();
    }
  }

  function setControlTab(tabName) {
    state.activeTab = tabName;
    document.querySelectorAll('.sidebar-item').forEach((btn) => {
      const active = btn.dataset.tab === tabName;
      btn.classList.toggle('is-active', active);
      btn.setAttribute('aria-selected', active ? 'true' : 'false');
    });
    document.querySelectorAll('.control-pane').forEach((pane) => {
      pane.classList.toggle('is-active', pane.id === `tab-${tabName}`);
    });
    if (tabName === 'linux' && !state.linuxSettings) loadLinuxSettings();
    if (tabName === 'appearance') renderAppearanceSettings();
  }

  function updateSearchUi() {
    const hasQuery = state.searchQuery.length > 0;
    els.searchKbd.classList.toggle('is-hidden', hasQuery);
    els.searchClear.classList.toggle('is-hidden', !hasQuery);
  }

  els.navApps.addEventListener('click', () => setView('apps'));
  els.navControl.addEventListener('click', () => setView('control'));
  document.querySelectorAll('.sidebar-item').forEach((btn) => {
    btn.addEventListener('click', () => setControlTab(btn.dataset.tab));
  });

  els.searchInput.addEventListener('input', () => {
    state.searchQuery = els.searchInput.value;
    updateSearchUi();
    renderAppsGrid();
  });

  els.searchClear.addEventListener('click', () => {
    state.searchQuery = '';
    els.searchInput.value = '';
    updateSearchUi();
    renderAppsGrid();
    els.searchInput.focus();
  });

  // ---------------- Shared Helpers ----------------
  function isFolderToken(token) {
    return typeof token === 'string' && token.startsWith(FOLDER_PREFIX);
  }

  function folderIdFromToken(token) {
    return token.slice(FOLDER_PREFIX.length);
  }

  function findFolder(folderId) {
    return state.layout.folders.find((f) => f.id === folderId) || null;
  }

  function clearSelection() {
    state.selectedKey = null;
    document.querySelectorAll('.tile.is-selected').forEach((el) => el.classList.remove('is-selected'));
  }

  function selectTile(key) {
    state.selectedKey = key;
    document.querySelectorAll('.tile').forEach((el) => {
      el.classList.toggle('is-selected', el.dataset.token === key);
    });
  }

  // ---------------- Free-position Desktop Grid ----------------
  function positionKey(row, col) {
    return `${row}:${col}`;
  }

  function calculatePlacements() {
    const columns = state.layout.appearance.columns;
    const positions = state.layout.positions || {};
    const used = new Set();
    const placements = [];

    const findFree = () => {
      let row = 0;
      let col = 0;
      while (used.has(positionKey(row, col))) {
        col += 1;
        if (col >= columns) {
          col = 0;
          row += 1;
        }
      }
      return { row, col };
    };

    state.layout.rootOrder.forEach((token, index) => {
      const saved = positions[token];
      let row = saved ? Number(saved.row) : Math.floor(index / columns);
      let col = saved ? Number(saved.col) : index % columns;
      if (!Number.isInteger(row) || !Number.isInteger(col) || row < 0 || col < 0 || col >= columns || used.has(positionKey(row, col))) {
        const free = findFree();
        row = free.row;
        col = free.col;
      }
      used.add(positionKey(row, col));
      placements.push({ token, row, col });
    });

    return placements;
  }

  function getSlot(row, col) {
    return els.appsGrid.querySelector(`.grid-slot[data-row='${row}'][data-col='${col}']`);
  }

  function renderAppsGrid() {
    const query = state.searchQuery.trim().toLowerCase();
    const appearance = state.layout.appearance;
    els.appsGrid.innerHTML = '';
    els.appsGrid.className = 'apps-grid';
    els.appsGrid.classList.remove('is-dragging');
    els.appsEmpty.classList.add('is-hidden');
    els.appsEmpty.innerHTML = '';

    if (!state.appsLoaded) {
      els.desktopHint.classList.add('is-hidden');
      const empty = document.createElement('p');
      empty.className = 'empty-title';
      empty.textContent = '正在读取 Linux 应用…';
      els.appsEmpty.appendChild(empty);
      els.appsEmpty.classList.remove('is-hidden');
      return;
    }

    if (query) {
      els.desktopHint.classList.add('is-hidden');
      const matched = state.apps.filter((app) =>
        (app.displayName || '').toLowerCase().includes(query) ||
        (app.name || '').toLowerCase().includes(query) ||
        (app.cmd || '').toLowerCase().includes(query)
      );
      els.appsGrid.classList.add('search-results');
      if (matched.length === 0) {
        renderEmptyState(
          `未找到匹配 “${state.searchQuery.trim()}” 的应用`,
          '你可以尝试其他关键词，或直接运行自定义 Linux 命令。',
          [
            { label: '清空搜索', className: 'btn btn-secondary', action: () => {
              state.searchQuery = '';
              els.searchInput.value = '';
              updateSearchUi();
              renderAppsGrid();
            } },
            { label: '运行自定义命令', className: 'btn btn-primary', action: () => openCustomCommandModal(state.searchQuery.trim()) }
          ]
        );
        return;
      }
      const frag = document.createDocumentFragment();
      for (const app of matched) frag.appendChild(createAppTile(app, null, 'search'));
      els.appsGrid.appendChild(frag);
      return;
    }

    if (state.apps.length === 0) {
      els.desktopHint.classList.add('is-hidden');
      renderEmptyState(
        '暂未发现已安装的 Linux 应用',
        '打开 Ubuntu 应用中心安装桌面软件，或将下载的 .AppImage / .deb 放入下载目录后即可自动识别。',
        [
          { label: '打开应用中心', className: 'btn btn-primary', action: () => handleOpenAppCenter() },
          { label: '重新扫描', className: 'btn btn-secondary', action: () => handleRefreshApps() }
        ]
      );
      return;
    }

    els.desktopHint.classList.remove('is-hidden');
    els.appsGrid.classList.add('desktop-grid');

    const columns = appearance.columns;
    const placements = calculatePlacements();
    const maxRow = placements.reduce((max, p) => Math.max(max, p.row), 0);
    const cellHeight = 98 * (appearance.tileScale / 100);
    const availableHeight = els.appsScroll.clientHeight || 520;
    const visibleRows = Math.max(4, Math.ceil((availableHeight - 94) / cellHeight));
    const rows = Math.max(5, visibleRows, maxRow + 2);
    els.appsGrid.style.setProperty('--grid-rows', String(rows));

    const slotFrag = document.createDocumentFragment();
    for (let row = 0; row < rows; row += 1) {
      for (let col = 0; col < columns; col += 1) {
        const slot = document.createElement('div');
        slot.className = 'grid-slot';
        slot.dataset.row = String(row);
        slot.dataset.col = String(col);
        slot.style.gridRow = String(row + 1);
        slot.style.gridColumn = String(col + 1);
        slot.setAttribute('aria-hidden', 'true');
        slotFrag.appendChild(slot);
      }
    }
    els.appsGrid.appendChild(slotFrag);

    const tileFrag = document.createDocumentFragment();
    for (const placement of placements) {
      let tile = null;
      if (isFolderToken(placement.token)) {
        const folder = findFolder(folderIdFromToken(placement.token));
        if (folder) tile = createFolderTile(folder, 'root');
      } else {
        const app = state.appById.get(placement.token);
        if (app) tile = createAppTile(app, null, 'root');
      }
      if (!tile) continue;
      tile.dataset.row = String(placement.row);
      tile.dataset.col = String(placement.col);
      tile.dataset.rootToken = placement.token;
      tile.style.gridRow = String(placement.row + 1);
      tile.style.gridColumn = String(placement.col + 1);
      tileFrag.appendChild(tile);
    }
    els.appsGrid.appendChild(tileFrag);
  }

  function renderEmptyState(title, description, actions) {
    const container = document.createElement('div');
    container.className = 'empty-state';
    const h = document.createElement('p');
    h.className = 'empty-title';
    h.textContent = title;
    const p = document.createElement('p');
    p.className = 'empty-desc';
    p.textContent = description;
    const bar = document.createElement('div');
    bar.className = 'empty-actions';
    for (const item of actions) {
      const btn = document.createElement('button');
      btn.type = 'button';
      btn.className = item.className;
      btn.textContent = item.label;
      btn.onclick = item.action;
      bar.appendChild(btn);
    }
    container.append(h, p, bar);
    els.appsEmpty.appendChild(container);
    els.appsEmpty.classList.remove('is-hidden');
  }

  function createAppTile(app, inFolderId, context = 'root') {
    const tile = document.createElement('div');
    tile.className = 'tile' + (state.selectedKey === app.id ? ' is-selected' : '');
    tile.setAttribute('role', 'listitem');
    tile.tabIndex = 0;
    tile.draggable = context !== 'search';
    tile.dataset.kind = 'app';
    tile.dataset.id = app.id;
    tile.dataset.token = app.id;
    tile.title = app.args && app.args.length > 0
      ? `${app.displayName}\n${app.cmd} ${app.args.join(' ')}`
      : `${app.displayName}\n${app.cmd}`;

    const iconFrame = document.createElement('div');
    iconFrame.className = 'tile-icon-frame';
    const img = document.createElement('img');
    img.className = 'tile-icon-img';
    img.src = app.iconUrl;
    img.alt = '';
    img.loading = 'lazy';
    img.draggable = false;
    iconFrame.appendChild(img);

    const src = (app.source || '').toLowerCase();
    if (src === 'deb' || src === 'appimage') {
      const badge = document.createElement('span');
      badge.className = `tile-badge ${src === 'deb' ? 'tile-badge-deb' : 'tile-badge-app'}`;
      badge.textContent = src === 'deb' ? 'DEB' : 'APP';
      iconFrame.appendChild(badge);
    }

    const label = document.createElement('div');
    label.className = 'tile-title';
    label.textContent = app.displayName;
    tile.append(iconFrame, label);

    tile.addEventListener('click', (e) => {
      e.stopPropagation();
      selectTile(app.id);
    });
    tile.addEventListener('dblclick', (e) => {
      e.stopPropagation();
      handleActivateApp(app);
    });
    tile.addEventListener('keydown', (e) => {
      if (e.key === 'Enter') {
        e.preventDefault();
        handleActivateApp(app);
      }
    });
    tile.addEventListener('contextmenu', (e) => {
      e.preventDefault();
      e.stopPropagation();
      selectTile(app.id);
      openAppContextMenu(app, inFolderId, e.clientX, e.clientY);
    });

    if (context !== 'search') {
      tile.addEventListener('dragstart', (e) => {
        state.dragPayload = {
          kind: 'app',
          appId: app.id,
          token: app.id,
          fromFolderId: inFolderId,
          fromRoot: context === 'root'
        };
        tile.classList.add('is-dragging');
        if (context === 'root') els.appsGrid.classList.add('is-dragging');
        e.dataTransfer.effectAllowed = 'move';
        e.dataTransfer.setData('text/plain', app.id);
      });
      tile.addEventListener('dragend', () => {
        tile.classList.remove('is-dragging');
        els.appsGrid.classList.remove('is-dragging');
        clearDropVisuals();
        state.dragPayload = null;
      });
    }

    if (context === 'folder') {
      tile.addEventListener('dragover', (e) => {
        const payload = state.dragPayload;
        if (!payload || payload.kind !== 'app' || payload.token === app.id) return;
        e.preventDefault();
        e.dataTransfer.dropEffect = 'move';
        clearDropIndicators(tile);
        const rect = tile.getBoundingClientRect();
        const relX = (e.clientX - rect.left) / rect.width;
        tile.classList.add(relX <= 0.5 ? 'is-drop-before' : 'is-drop-after');
      });
      tile.addEventListener('dragleave', () => {
        tile.classList.remove('is-drop-before', 'is-drop-after');
      });
      tile.addEventListener('drop', async (e) => {
        e.preventDefault();
        e.stopPropagation();
        const payload = state.dragPayload;
        if (!payload || payload.kind !== 'app' || payload.token === app.id) return;
        const insertBefore = tile.classList.contains('is-drop-before');
        clearDropIndicators();
        if (payload.fromFolderId !== inFolderId) {
          await handleMoveAppToFolder(payload.appId, inFolderId);
        } else {
          await handleReorderFolder(inFolderId, payload.appId, app.id, insertBefore);
        }
      });
    }

    return tile;
  }

  function createFolderTile(folder, context = 'root') {
    const token = FOLDER_PREFIX + folder.id;
    const tile = document.createElement('div');
    tile.className = 'tile' + (state.selectedKey === token ? ' is-selected' : '');
    tile.setAttribute('role', 'listitem');
    tile.tabIndex = 0;
    tile.draggable = context === 'root';
    tile.dataset.kind = 'folder';
    tile.dataset.id = folder.id;
    tile.dataset.token = token;
    tile.title = `${folder.name} (${folder.appIds.length} 个应用)`;

    const preview = document.createElement('div');
    preview.className = 'folder-preview';
    const previewApps = folder.appIds.slice(0, 4).map((id) => state.appById.get(id)).filter(Boolean);
    for (let i = 0; i < 4; i += 1) {
      if (previewApps[i]) {
        const img = document.createElement('img');
        img.className = 'folder-mini-icon';
        img.src = previewApps[i].iconUrl;
        img.alt = '';
        img.draggable = false;
        preview.appendChild(img);
      } else {
        const empty = document.createElement('div');
        empty.className = 'folder-mini-empty';
        preview.appendChild(empty);
      }
    }

    const label = document.createElement('div');
    label.className = 'tile-title';
    label.textContent = folder.name;
    tile.append(preview, label);

    tile.addEventListener('click', (e) => {
      e.stopPropagation();
      selectTile(token);
      openFolder(folder.id);
    });
    tile.addEventListener('keydown', (e) => {
      if (e.key === 'Enter' || e.key === ' ') {
        e.preventDefault();
        openFolder(folder.id);
      }
    });
    tile.addEventListener('contextmenu', (e) => {
      e.preventDefault();
      e.stopPropagation();
      selectTile(token);
      openFolderContextMenu(folder, e.clientX, e.clientY);
    });

    if (context === 'root') {
      tile.addEventListener('dragstart', (e) => {
        state.dragPayload = {
          kind: 'folder',
          folderId: folder.id,
          token,
          fromFolderId: null,
          fromRoot: true
        };
        tile.classList.add('is-dragging');
        els.appsGrid.classList.add('is-dragging');
        e.dataTransfer.effectAllowed = 'move';
        e.dataTransfer.setData('text/plain', token);
      });
      tile.addEventListener('dragend', () => {
        tile.classList.remove('is-dragging');
        els.appsGrid.classList.remove('is-dragging');
        clearDropVisuals();
        state.dragPayload = null;
      });
    }

    return tile;
  }

  function clearDropIndicators(except = null) {
    document.querySelectorAll('.tile.is-drop-merge, .tile.is-drop-into-folder, .tile.is-drop-before, .tile.is-drop-after').forEach((el) => {
      if (el !== except) el.classList.remove('is-drop-merge', 'is-drop-into-folder', 'is-drop-before', 'is-drop-after');
    });
  }

  function clearGridDropIndicators() {
    clearDropIndicators();
    document.querySelectorAll('.grid-slot.is-drop-target').forEach((el) => el.classList.remove('is-drop-target'));
  }

  function setDockDropActive(active) {
    els.dock.classList.toggle('is-drop-target', active);
  }

  function clearAvoidance() {
    document.querySelectorAll('.tile.is-avoiding').forEach((el) => {
      el.classList.remove('is-avoiding');
      el.style.removeProperty('--avoid-x');
      el.style.removeProperty('--avoid-y');
    });
  }

  function applyAvoidance(cell) {
    clearAvoidance();
    const payload = state.dragPayload;
    if (!cell || !payload || state.searchQuery.trim()) return;
    const tiles = Array.from(els.appsGrid.querySelectorAll('.tile[data-root-token]'));
    for (const tile of tiles) {
      if (tile.dataset.token === payload.token || tile === cell.element) continue;
      const row = Number(tile.dataset.row);
      const col = Number(tile.dataset.col);
      if (!Number.isInteger(row) || !Number.isInteger(col)) continue;
      const dx = col - cell.col;
      const dy = row - cell.row;
      if (Math.max(Math.abs(dx), Math.abs(dy)) !== 1) continue;
      tile.style.setProperty('--avoid-x', `${dx * 14}px`);
      tile.style.setProperty('--avoid-y', `${dy * 14}px`);
      tile.classList.add('is-avoiding');
    }
  }

  function clearDropVisuals() {
    clearGridDropIndicators();
    setDockDropActive(false);
    clearAvoidance();
    state.dropTarget = null;
    state.dropMode = null;
  }

  function handleDockDragOver(e) {
    const payload = state.dragPayload;
    if (!payload || (payload.kind !== 'app' && payload.kind !== 'folder')) return;
    e.preventDefault();
    e.stopPropagation();
    e.dataTransfer.dropEffect = 'move';
    clearGridDropIndicators();
    clearAvoidance();
    setDockDropActive(true);
    state.dropTarget = null;
    state.dropMode = 'dock';
  }

  function handleDockDragLeave(e) {
    if (!els.dock.contains(e.relatedTarget)) setDockDropActive(false);
  }

  async function handleDockDrop(e) {
    const payload = state.dragPayload;
    if (!payload || (payload.kind !== 'app' && payload.kind !== 'folder')) return;
    e.preventDefault();
    e.stopPropagation();
    els.appsGrid.classList.remove('is-dragging');
    clearDropVisuals();
    if (payload.kind === 'app') await pinDockItem('app', payload.appId);
    else await pinDockItem('folder', payload.folderId);
  }

  function resolveDropCell(e) {
    const el = e.target.closest('.grid-slot, .tile[data-root-token]');
    if (!el) return null;
    const row = Number(el.dataset.row);
    const col = Number(el.dataset.col);
    if (!Number.isInteger(row) || !Number.isInteger(col)) return null;
    return { row, col, tile: el.classList.contains('tile') ? el : null, element: el };
  }

  function isCenterDrop(e, tile) {
    if (!tile) return false;
    const rect = tile.getBoundingClientRect();
    const relX = (e.clientX - rect.left) / rect.width;
    const relY = (e.clientY - rect.top) / rect.height;
    return relX > 0.28 && relX < 0.72 && relY > 0.2 && relY < 0.8;
  }

  function handleGridDragOver(e) {
    const payload = state.dragPayload;
    const cell = resolveDropCell(e);
    if (!payload || !cell || state.searchQuery.trim()) return;
    e.preventDefault();
    e.dataTransfer.dropEffect = 'move';
    clearDropVisuals();

    const canMerge = payload.kind === 'app'
      && payload.fromFolderId === null
      && payload.fromRoot
      && cell.tile
      && cell.tile.dataset.token !== payload.token
      && isCenterDrop(e, cell.tile);

    if (canMerge && cell.tile.dataset.kind === 'folder') {
      state.dropMode = 'into-folder';
      cell.tile.classList.add('is-drop-into-folder');
    } else if (canMerge && cell.tile.dataset.kind === 'app') {
      state.dropMode = 'merge';
      cell.tile.classList.add('is-drop-merge');
    } else {
      state.dropMode = 'move';
      const slot = getSlot(cell.row, cell.col);
      if (slot) slot.classList.add('is-drop-target');
      else cell.tile?.classList.add('is-drop-into-folder');
    }
    state.dropTarget = { row: cell.row, col: cell.col, token: cell.tile?.dataset.token || null };
    applyAvoidance(cell);
  }

  async function handleGridDrop(e) {
    const payload = state.dragPayload;
    const cell = resolveDropCell(e);
    if (!payload || !cell || state.searchQuery.trim()) return;
    e.preventDefault();
    e.stopPropagation();

    const mode = state.dropMode || 'move';
    const targetTile = cell.tile;
    const targetToken = targetTile?.dataset.token || null;
    const targetId = targetTile?.dataset.id || null;
    clearDropVisuals();
    els.appsGrid.classList.remove('is-dragging');

    if (mode === 'merge' && targetTile && targetTile.dataset.kind === 'app' && payload.kind === 'app') {
      await handleMergeApps(payload.appId, targetId);
      return;
    }
    if (mode === 'into-folder' && targetTile && targetTile.dataset.kind === 'folder' && payload.kind === 'app') {
      await handleMoveAppToFolder(payload.appId, targetId);
      return;
    }
    if (targetToken === payload.token) return;
    await handleRootPlacement(payload, cell.row, cell.col);
  }

  async function handleRootPlacement(payload, row, col) {
    try {
      let res;
      if (payload.kind === 'app' && payload.fromFolderId) {
        res = await bridge.invoke('layout.moveAppToRootAt', { appId: payload.appId, row, col });
      } else if (payload.fromRoot) {
        res = await bridge.invoke('layout.setRootPosition', { token: payload.token, row, col });
      } else {
        return;
      }
      applyLayoutPayload(res.layout);
    } catch (err) {
      showToast(`移动失败：${err.message}`, 'error');
    }
  }

  els.appsGrid.addEventListener('dragover', handleGridDragOver);
  els.appsGrid.addEventListener('drop', handleGridDrop);
  els.appsGrid.addEventListener('dragleave', (e) => {
    if (!els.appsGrid.contains(e.relatedTarget)) clearDropVisuals();
  });

  els.dock.addEventListener('dragover', handleDockDragOver);
  els.dock.addEventListener('dragleave', handleDockDragLeave);
  els.dock.addEventListener('drop', handleDockDrop);

  els.appsScroll.addEventListener('click', () => clearSelection());
  els.appsScroll.addEventListener('contextmenu', (e) => {
    if (e.target.closest('.tile')) return;
    e.preventDefault();
    openDesktopContextMenu(e.clientX, e.clientY);
  });

  // ---------------- Folder Overlay ----------------
  function openFolder(folderId) {
    const folder = findFolder(folderId);
    if (!folder) return;
    state.openFolderId = folderId;
    renderFolderModal();
    els.folderOverlay.classList.remove('is-hidden');
  }

  function closeFolder() {
    state.openFolderId = null;
    els.folderOverlay.classList.add('is-hidden');
  }

  function renderFolderModal() {
    if (!state.openFolderId) return;
    const folder = findFolder(state.openFolderId);
    if (!folder) {
      closeFolder();
      return;
    }
    els.folderModalTitle.textContent = folder.name;
    els.folderModalCount.textContent = String(folder.appIds.length);
    els.folderGrid.innerHTML = '';
    if (folder.appIds.length === 0) {
      const empty = document.createElement('div');
      empty.className = 'muted-text';
      empty.style.gridColumn = '1 / -1';
      empty.style.padding = '20px 4px';
      empty.textContent = '文件夹为空，可拖拽应用到此文件夹或右键应用选择“移动到文件夹”。';
      els.folderGrid.appendChild(empty);
      return;
    }
    const frag = document.createDocumentFragment();
    for (const appId of folder.appIds) {
      const app = state.appById.get(appId);
      if (app) frag.appendChild(createAppTile(app, folder.id, 'folder'));
    }
    els.folderGrid.appendChild(frag);
  }

  els.btnFolderClose.addEventListener('click', closeFolder);
  els.folderOverlay.addEventListener('click', (e) => {
    if (e.target === els.folderOverlay) closeFolder();
  });
  els.folderOverlay.addEventListener('dragover', (e) => {
    if (state.dragPayload && state.dragPayload.fromFolderId && e.target === els.folderOverlay) {
      e.preventDefault();
      e.dataTransfer.dropEffect = 'move';
    }
  });
  els.folderOverlay.addEventListener('drop', async (e) => {
    if (state.dragPayload && state.dragPayload.fromFolderId && e.target === els.folderOverlay) {
      e.preventDefault();
      const appId = state.dragPayload.appId;
      state.dragPayload = null;
      await handleMoveAppToRoot(appId);
    }
  });
  els.btnFolderRename.addEventListener('click', () => {
    const folder = findFolder(state.openFolderId);
    if (folder) openRenameFolderModal(folder);
  });
  els.btnFolderStartMenu.addEventListener('click', async () => {
    const folder = findFolder(state.openFolderId);
    if (!folder) return;
    try {
      const res = await bridge.invoke('shortcut.addFolderToStartMenu', { folderId: folder.id });
      showToast(res.message, 'success');
    } catch (err) {
      showToast(err.message, 'error');
    }
  });
  els.btnFolderDissolve.addEventListener('click', () => {
    const folder = findFolder(state.openFolderId);
    if (folder) handleDissolveFolder(folder);
  });

  // ---------------- Customizable Dock ----------------
  function dockIconSvg(target) {
    switch (target) {
      case 'view:stages':
        return `<svg viewBox='0 0 18 18' fill='none'><rect x='2.2' y='3' width='13.6' height='9.2' rx='2' stroke='currentColor' stroke-width='1.4'/><path d='M6 15h6M9 12.2V15' stroke='currentColor' stroke-width='1.4' stroke-linecap='round'/></svg>`;
      case 'view:daemon':
        return `<svg viewBox='0 0 18 18' fill='none'><circle cx='9' cy='9' r='6.2' stroke='currentColor' stroke-width='1.4'/><path d='M9 5.5v3.6l2.4 1.6' stroke='currentColor' stroke-width='1.4' stroke-linecap='round' stroke-linejoin='round'/></svg>`;
      case 'view:linux':
        return `<svg viewBox='0 0 18 18' fill='none'><path d='M4.2 5.2h9.6M4.2 9h9.6M4.2 12.8h6.4' stroke='currentColor' stroke-width='1.4' stroke-linecap='round'/><circle cx='13' cy='12.8' r='1.6' stroke='currentColor' stroke-width='1.4'/></svg>`;
      case 'view:appearance':
        return `<svg viewBox='0 0 18 18' fill='none'><path d='M3 5h12M3 9h12M3 13h12' stroke='currentColor' stroke-width='1.4' stroke-linecap='round'/><circle cx='7' cy='5' r='1.4' fill='currentColor'/><circle cx='11.5' cy='9' r='1.4' fill='currentColor'/><circle cx='6.5' cy='13' r='1.4' fill='currentColor'/></svg>`;
      case 'action:appCenter':
        return `<svg viewBox='0 0 18 18' fill='none'><path d='M4.5 7h9l-.7 7.2a1.4 1.4 0 0 1-1.4 1.3H6.6a1.4 1.4 0 0 1-1.4-1.3L4.5 7Z' stroke='currentColor' stroke-width='1.4'/><path d='M7 7V5.7a2 2 0 0 1 4 0V7' stroke='currentColor' stroke-width='1.4' stroke-linecap='round'/></svg>`;
      case 'action:newFolder':
        return `<svg viewBox='0 0 18 18' fill='none'><path d='M2.8 5.6A1.6 1.6 0 0 1 4.4 4h2.8l1.4 1.6h5A1.6 1.6 0 0 1 15.2 7v6.4a1.6 1.6 0 0 1-1.6 1.6H4.4a1.6 1.6 0 0 1-1.6-1.6V5.6Z' stroke='currentColor' stroke-width='1.4'/><path d='M9 8.4v3.2M7.4 10h3.2' stroke='currentColor' stroke-width='1.4' stroke-linecap='round'/></svg>`;
      case 'action:runCommand':
        return `<svg viewBox='0 0 18 18' fill='none'><rect x='2.5' y='3.4' width='13' height='11.2' rx='2.2' stroke='currentColor' stroke-width='1.4'/><path d='M5.7 7.3l2.2 1.8-2.2 1.8M9.4 11h3' stroke='currentColor' stroke-width='1.4' stroke-linecap='round' stroke-linejoin='round'/></svg>`;
      case 'action:refreshApps':
        return `<svg viewBox='0 0 18 18' fill='none'><path d='M14.2 7.2A5.4 5.4 0 1 0 14 11' stroke='currentColor' stroke-width='1.4' stroke-linecap='round'/><path d='M14.4 3.8v3.6h-3.6' stroke='currentColor' stroke-width='1.4' stroke-linecap='round' stroke-linejoin='round'/></svg>`;
      case 'action:autoOrganize':
        return `<svg viewBox='0 0 18 18' fill='none'><rect x='3' y='3' width='5' height='5' rx='1' stroke='currentColor' stroke-width='1.3'/><rect x='10' y='3' width='5' height='5' rx='1' stroke='currentColor' stroke-width='1.3'/><rect x='3' y='10' width='5' height='5' rx='1' stroke='currentColor' stroke-width='1.3'/><rect x='10' y='10' width='5' height='5' rx='1' stroke='currentColor' stroke-width='1.3'/></svg>`;
      case 'action:syncStartMenu':
        return `<svg viewBox='0 0 18 18' fill='none'><path d='M3.5 9.4l3.1 3.1 7.9-7.9' stroke='currentColor' stroke-width='1.6' stroke-linecap='round' stroke-linejoin='round'/></svg>`;
      default:
        return `<svg viewBox='0 0 18 18' fill='none'><circle cx='9' cy='9' r='5.8' stroke='currentColor' stroke-width='1.4'/><circle cx='9' cy='9' r='1.7' fill='currentColor'/></svg>`;
    }
  }

  function inferDockLabel(kind, target) {
    if (kind === 'app') return state.appById.get(target)?.displayName || '应用';
    if (kind === 'folder') return findFolder(target)?.name || '文件夹';
    return DOCK_TARGETS.find((d) => d.target === target)?.label || '快捷方式';
  }

  function getDockDisplay(item) {
    const label = item.label || inferDockLabel(item.kind, item.target);
    if (item.kind === 'app') {
      const app = state.appById.get(item.target);
      return { label, iconUrl: app?.iconUrl || null, valid: Boolean(app), folder: false };
    }
    if (item.kind === 'folder') {
      const folder = findFolder(item.target);
      const first = folder?.appIds.map((id) => state.appById.get(id)).find(Boolean);
      return { label, iconUrl: first?.iconUrl || null, valid: Boolean(folder), folder: true };
    }
    return { label, iconUrl: null, valid: true, folder: false };
  }

  function createDockIconNode(item, className) {
    const display = getDockDisplay(item);
    const icon = document.createElement('span');
    icon.className = className + (display.folder ? ' is-folder' : '');
    if (display.iconUrl) {
      const img = document.createElement('img');
      img.src = display.iconUrl;
      img.alt = '';
      img.draggable = false;
      icon.appendChild(img);
    } else {
      icon.innerHTML = dockIconSvg(item.target);
    }
    return icon;
  }

  function renderDock() {
    const appearance = state.layout.appearance;
    els.dock.innerHTML = '';
    els.dock.classList.toggle('no-labels', !appearance.dockLabels);
    els.dockContainer.classList.toggle('is-hidden', !appearance.showDock);

    if (state.layout.dockItems.length === 0) {
      const hint = document.createElement('span');
      hint.className = 'dock-empty-hint';
      hint.textContent = '快捷栏为空 · 右键应用或点击 + 添加';
      els.dock.appendChild(hint);
    }

    for (const item of state.layout.dockItems) {
      const display = getDockDisplay(item);
      if (!display.valid) continue;
      const btn = document.createElement('button');
      btn.type = 'button';
      btn.className = 'dock-item';
      btn.dataset.dockId = item.id;
      btn.dataset.dockKind = item.kind;
      btn.dataset.dockTarget = item.target;
      btn.title = display.label;
      btn.appendChild(createDockIconNode(item, 'dock-icon'));

      const label = document.createElement('span');
      label.className = 'dock-label';
      label.textContent = display.label;
      btn.appendChild(label);

      if (item.target === 'view:stages') {
        const dot = document.createElement('span');
        dot.className = 'dock-live-dot is-hidden';
        btn.querySelector('.dock-icon').appendChild(dot);
      }

      btn.addEventListener('click', () => activateDockItem(item));
      btn.addEventListener('contextmenu', (e) => {
        e.preventDefault();
        e.stopPropagation();
        openDockItemMenu(item, e.clientX, e.clientY);
      });
      els.dock.appendChild(btn);
    }

    const add = document.createElement('button');
    add.type = 'button';
    add.className = 'dock-add';
    add.title = '自定义快捷栏';
    add.setAttribute('aria-label', '自定义快捷栏');
    add.innerHTML = `<svg viewBox='0 0 14 14' fill='none'><path d='M7 2.5v9M2.5 7h9' stroke='currentColor' stroke-width='1.5' stroke-linecap='round'/></svg>`;
    add.addEventListener('click', () => openDockManager());
    els.dock.appendChild(add);
  }

  function activateDockItem(item) {
    if (item.kind === 'app') {
      const app = state.appById.get(item.target);
      if (app) handleActivateApp(app);
      else showToast('该应用已不存在，已从快捷栏移除', 'error');
      return;
    }
    if (item.kind === 'folder') {
      openFolder(item.target);
      return;
    }
    if (item.kind === 'view') {
      setView('control', item.target.slice(5));
      return;
    }
    if (item.kind === 'action') {
      switch (item.target) {
        case 'action:appCenter': handleOpenAppCenter(); break;
        case 'action:newFolder': openCreateFolderModal(); break;
        case 'action:runCommand': openCustomCommandModal(); break;
        case 'action:refreshApps': handleRefreshApps(); break;
        case 'action:autoOrganize': handleAutoOrganize(); break;
        case 'action:syncStartMenu': handleSyncStartMenu(); break;
        default: showToast('未知的快捷方式', 'error');
      }
    }
  }

  async function saveDockItems(items, quiet = false) {
    try {
      const res = await bridge.invoke('layout.setDockItems', { items });
      setLayout(res.layout);
      renderLayoutViews();
      if (!quiet) showToast('快捷栏已更新', 'success');
      return true;
    } catch (err) {
      showToast(`保存快捷栏失败：${err.message}`, 'error');
      return false;
    }
  }

  function isPinned(kind, target) {
    return state.layout.dockItems.some((item) => item.kind === kind && item.target === target);
  }

  async function pinDockItem(kind, target) {
    if (isPinned(kind, target)) {
      showToast('已经固定在快捷栏', 'info');
      return;
    }
    const items = state.layout.dockItems.map((item) => ({ ...item }));
    items.push({ id: '', kind, target, label: inferDockLabel(kind, target) });
    if (await saveDockItems(items, true)) showToast(`已固定 “${inferDockLabel(kind, target)}”`, 'success');
  }

  async function unpinDockTarget(kind, target) {
    const items = state.layout.dockItems.filter((item) => !(item.kind === kind && item.target === target));
    if (await saveDockItems(items, true)) showToast('已从快捷栏移除', 'info');
  }

  function openDockItemMenu(item, x, y) {
    const items = state.layout.dockItems.map((d) => ({ ...d }));
    const index = items.findIndex((d) => d.id === item.id);
    if (index < 0) return;
    const menu = [
      { label: '打开', primary: true, onClick: () => activateDockItem(item) },
      { label: '重命名…', onClick: () => openDockRenameModal(item, index) }
    ];
    if (index > 0) {
      menu.push({ label: '向前移动', onClick: async () => {
        [items[index - 1], items[index]] = [items[index], items[index - 1]];
        await saveDockItems(items, true);
      } });
    }
    if (index < items.length - 1) {
      menu.push({ label: '向后移动', onClick: async () => {
        [items[index + 1], items[index]] = [items[index], items[index + 1]];
        await saveDockItems(items, true);
      } });
    }
    menu.push(
      { type: 'sep' },
      { label: '从快捷栏移除', danger: true, onClick: () => unpinDockTarget(item.kind, item.target) }
    );
    showContextMenu(menu, x, y);
  }

  function openDockRenameModal(item, index) {
    const wrap = document.createElement('div');
    const label = document.createElement('label');
    label.className = 'field-label';
    label.textContent = '快捷方式名称';
    const input = document.createElement('input');
    input.type = 'text';
    input.className = 'text-input';
    input.value = item.label || inferDockLabel(item.kind, item.target);
    wrap.append(label, input);

    const submit = async () => {
      const items = state.layout.dockItems.map((d) => ({ ...d }));
      if (!items[index]) return;
      items[index].label = input.value.trim() || inferDockLabel(items[index].kind, items[index].target);
      closeModal();
      await saveDockItems(items, true);
    };
    input.addEventListener('keydown', (e) => {
      if (e.key === 'Enter') {
        e.preventDefault();
        submit();
      }
    });
    openModal({
      title: '重命名快捷方式',
      bodyNode: wrap,
      buttons: [
        { label: '取消', className: 'btn btn-secondary', onClick: closeModal },
        { label: '保存', className: 'btn btn-primary', onClick: submit }
      ]
    });
  }

  function parseDockSelection(value) {
    if (!value) return null;
    if (value.startsWith('app:')) {
      const target = value.slice(4);
      return { id: '', kind: 'app', target, label: inferDockLabel('app', target) };
    }
    if (value.startsWith('folder:')) {
      const target = value.slice(7);
      return { id: '', kind: 'folder', target, label: inferDockLabel('folder', target) };
    }
    if (value.startsWith('view:')) {
      return { id: '', kind: 'view', target: value, label: inferDockLabel('view', value) };
    }
    if (value.startsWith('action:')) {
      return { id: '', kind: 'action', target: value, label: inferDockLabel('action', value) };
    }
    return null;
  }

  function openDockManager() {
    const wrap = document.createElement('div');
    wrap.className = 'dock-manager';
    const intro = document.createElement('p');
    intro.className = 'dock-manager-intro';
    intro.textContent = '快捷栏只放你选择的项目。可固定 Linux 应用、文件夹，或快速进入舞台中心与各项设置；拖动顺序与名称都会保存。';
    wrap.appendChild(intro);

    const addRow = document.createElement('div');
    addRow.className = 'dock-manager-add';

    const appSelect = document.createElement('select');
    appSelect.className = 'select-input';
    appSelect.setAttribute('aria-label', '选择应用或文件夹');
    appSelect.appendChild(new Option('选择应用或文件夹…', ''));
    const appGroup = document.createElement('optgroup');
    appGroup.label = 'Linux 应用';
    for (const app of state.apps) {
      appGroup.appendChild(new Option(app.displayName, `app:${app.id}`));
    }
    appSelect.appendChild(appGroup);
    const folderGroup = document.createElement('optgroup');
    folderGroup.label = '文件夹';
    for (const folder of state.layout.folders) {
      folderGroup.appendChild(new Option(folder.name, `folder:${folder.id}`));
    }
    appSelect.appendChild(folderGroup);

    const targetSelect = document.createElement('select');
    targetSelect.className = 'select-input';
    targetSelect.setAttribute('aria-label', '选择设置或功能');
    targetSelect.appendChild(new Option('选择设置或功能…', ''));
    for (const groupName of ['设置', '功能']) {
      const group = document.createElement('optgroup');
      group.label = groupName;
      for (const target of DOCK_TARGETS.filter((d) => d.group === groupName)) {
        group.appendChild(new Option(target.label, target.target));
      }
      targetSelect.appendChild(group);
    }

    const addBtn = document.createElement('button');
    addBtn.type = 'button';
    addBtn.className = 'btn btn-primary';
    addBtn.textContent = '添加';

    const list = document.createElement('div');
    list.className = 'dock-manager-list';

    let dockManagerItems = state.layout.dockItems.map((d) => ({ ...d }));

    async function commit() {
      const ok = await saveDockItems(dockManagerItems, true);
      if (ok) {
        dockManagerItems = state.layout.dockItems.map((d) => ({ ...d }));
        renderList();
      }
      return ok;
    }

    addBtn.addEventListener('click', async () => {
      const selected = parseDockSelection(appSelect.value || targetSelect.value);
      if (!selected) {
        showToast('请先选择一个应用、文件夹或设置', 'info');
        return;
      }
      if (isPinned(selected.kind, selected.target)) {
        showToast('该项目已在快捷栏中', 'info');
        return;
      }
      dockManagerItems.push(selected);
      if (await commit()) {
        appSelect.value = '';
        targetSelect.value = '';
        showToast(`已添加 “${selected.label}”`, 'success');
      }
    });

    function renderList() {
      list.innerHTML = '';
      if (dockManagerItems.length === 0) {
        const empty = document.createElement('div');
        empty.className = 'dock-manager-empty';
        empty.textContent = '还没有快捷方式。从上方选择项目添加。';
        list.appendChild(empty);
        return;
      }

      dockManagerItems.forEach((item, index) => {
        const row = document.createElement('div');
        row.className = 'dock-manager-item';
        row.appendChild(createDockIconNode(item, 'dock-manager-icon'));

        const labelInput = document.createElement('input');
        labelInput.type = 'text';
        labelInput.className = 'dock-manager-label';
        labelInput.value = item.label || inferDockLabel(item.kind, item.target);
        labelInput.setAttribute('aria-label', '快捷方式名称');
        labelInput.addEventListener('change', async () => {
          dockManagerItems[index].label = labelInput.value.trim() || inferDockLabel(item.kind, item.target);
          await commit();
        });
        row.appendChild(labelInput);

        const up = document.createElement('button');
        up.type = 'button';
        up.className = 'dock-manager-btn';
        up.textContent = '↑';
        up.title = '向前移动';
        up.disabled = index === 0;
        up.addEventListener('click', async () => {
          if (index === 0) return;
          [dockManagerItems[index - 1], dockManagerItems[index]] = [dockManagerItems[index], dockManagerItems[index - 1]];
          await commit();
        });

        const down = document.createElement('button');
        down.type = 'button';
        down.className = 'dock-manager-btn';
        down.textContent = '↓';
        down.title = '向后移动';
        down.disabled = index === dockManagerItems.length - 1;
        down.addEventListener('click', async () => {
          if (index >= dockManagerItems.length - 1) return;
          [dockManagerItems[index + 1], dockManagerItems[index]] = [dockManagerItems[index], dockManagerItems[index + 1]];
          await commit();
        });

        const remove = document.createElement('button');
        remove.type = 'button';
        remove.className = 'dock-manager-btn is-danger';
        remove.textContent = '×';
        remove.title = '移除';
        remove.addEventListener('click', async () => {
          dockManagerItems.splice(index, 1);
          await commit();
        });

        row.append(up, down, remove);
        list.appendChild(row);
      });
    }

    addRow.append(appSelect, targetSelect, addBtn);
    wrap.append(addRow, list);
    renderList();
    openModal({
      title: '自定义快捷栏',
      bodyNode: wrap,
      buttons: [{ label: '完成', className: 'btn btn-primary', onClick: closeModal }]
    });
  }

  // ---------------- Live Status ----------------
  function applyStatePayload(payload) {
    if (payload.distro) state.distro = payload.distro;
    if (typeof payload.daemonAlive === 'boolean') state.daemonAlive = payload.daemonAlive;
    if (typeof payload.startupEnabled === 'boolean') state.startupEnabled = payload.startupEnabled;
    if (typeof payload.clipboardSyncEnabled === 'boolean') state.clipboardSyncEnabled = payload.clipboardSyncEnabled;
    if (typeof payload.hotkeyRegistered === 'boolean') state.hotkeyRegistered = payload.hotkeyRegistered;
    if (Array.isArray(payload.stages)) state.stages = payload.stages;
    renderStatusChrome();
    renderControlCenter();
  }

  function renderStatusChrome() {
    const count = state.stages.length;
    const hasAgent = state.stages.some((s) => s.inputOwner && s.inputOwner.type === 'agent');
    const daemonClass = hasAgent ? 'is-agent' : state.daemonAlive ? 'is-live' : 'is-idle';

    els.daemonPillDot.className = `status-dot ${daemonClass}`;
    els.daemonPillDot.title = state.daemonAlive
      ? `focusstaged 运行中 · ${count} 个活跃舞台`
      : 'focusstaged 待机中（启动应用时自动拉起）';
    els.navStageBadge.textContent = String(count);
    els.navStageBadge.classList.toggle('is-hidden', count === 0);
    els.navStageBadge.classList.toggle('has-agent', hasAgent);

    els.sidebarStageCount.textContent = String(count);
    els.sidebarStageCount.classList.toggle('has-agent', hasAgent);
    els.sidebarDaemonDot.className = `status-dot ${daemonClass}`;
    els.sidebarDaemonSub.textContent = state.daemonAlive
      ? `运行中 · ${state.distro}`
      : `待机中 · ${state.distro}`;
    els.btnSidebarDaemon.textContent = state.daemonAlive ? '停止守护' : '启动守护';

    document.querySelectorAll('.dock-live-dot').forEach((dot) => {
      dot.classList.toggle('is-hidden', count === 0 && !state.daemonAlive);
      dot.classList.toggle('is-agent', hasAgent);
    });
  }

  // ---------------- Built-in Control Center ----------------
  function renderControlCenter() {
    renderStages();
    renderDaemonTab();
    if (state.activeTab === 'appearance') renderAppearanceSettings();
  }

  function findIconForStage(stage) {
    const wm = (stage.wmClass || '').toLowerCase();
    const appCmd = (stage.app || '').toLowerCase();
    for (const app of state.apps) {
      if (wm && (app.wmClass || '').toLowerCase() === wm) return app.iconUrl;
      if (appCmd && ((app.cmd || '').toLowerCase() === appCmd || (app.stem || '').toLowerCase() === appCmd)) {
        return app.iconUrl;
      }
    }
    return '/icons/brand.svg';
  }

  function renderStages() {
    const stages = state.stages;
    const desktopCount = stages.filter((s) => s.display === 'desktop').length;
    const hiddenCount = stages.length - desktopCount;
    const agentCount = stages.filter((s) => s.inputOwner && s.inputOwner.type === 'agent').length;

    els.stagesSubtitle.textContent = state.daemonAlive
      ? `当前共 ${stages.length} 个活跃舞台，按 Ctrl+Alt+Space 可随时一键接管`
      : 'focusstaged 守护进程当前未运行，启动任意 Linux 应用时会自动拉起';
    els.btnTakeoverAll.disabled = !state.daemonAlive || stages.length === 0;
    els.btnTakeoverAll.className = agentCount > 0 ? 'btn btn-agent' : 'btn btn-primary';

    els.stagesSummary.innerHTML = '';
    if (stages.length > 0) {
      els.stagesSummary.classList.remove('is-hidden');
      els.stagesSummary.append(
        makeChip(`桌面显示 ${desktopCount}`, 'chip-emerald'),
        makeChip(`后台离屏 ${hiddenCount}`, 'chip-neutral'),
        makeChip(`AI 租约 ${agentCount}`, agentCount > 0 ? 'chip-agent' : 'chip-neutral')
      );
    } else {
      els.stagesSummary.classList.add('is-hidden');
    }

    els.stagesList.innerHTML = '';
    if (stages.length === 0) {
      const empty = document.createElement('div');
      empty.className = 'empty-state';
      const title = document.createElement('p');
      title.className = 'empty-title';
      title.textContent = '当前没有运行中的舞台';
      const desc = document.createElement('p');
      desc.className = 'empty-desc';
      desc.textContent = '从应用桌面或开始菜单启动的 Linux 应用都会显示在这里，你可以随时切换前后台或从 AI 手中接管。';
      const actions = document.createElement('div');
      actions.className = 'empty-actions';
      const btnGoApps = document.createElement('button');
      btnGoApps.type = 'button';
      btnGoApps.className = 'btn btn-primary';
      btnGoApps.textContent = '前往应用桌面';
      btnGoApps.onclick = () => setView('apps');
      const btnRun = document.createElement('button');
      btnRun.type = 'button';
      btnRun.className = 'btn btn-secondary';
      btnRun.textContent = '运行自定义命令';
      btnRun.onclick = () => openCustomCommandModal();
      actions.append(btnGoApps, btnRun);
      empty.append(title, desc, actions);
      els.stagesList.appendChild(empty);
      return;
    }

    const frag = document.createDocumentFragment();
    for (const stage of stages) {
      const ownerType = stage.inputOwner ? stage.inputOwner.type : 'human';
      const isDesktop = stage.display === 'desktop';
      const isAgent = ownerType === 'agent';
      const isFrozen = ownerType === 'frozen';
      const card = document.createElement('article');
      card.className = 'stage-card ' + (
        isAgent ? 'state-agent' : isFrozen ? 'state-frozen' : isDesktop ? 'state-desktop-human' : 'state-hidden'
      );

      const top = document.createElement('div');
      top.className = 'stage-top';
      const ident = document.createElement('div');
      ident.className = 'stage-ident';
      const icon = document.createElement('img');
      icon.className = 'stage-app-icon';
      icon.src = findIconForStage(stage);
      icon.alt = '';
      const titles = document.createElement('div');
      titles.className = 'stage-titles';
      const titleEl = document.createElement('div');
      titleEl.className = 'stage-title';
      titleEl.textContent = stage.title || stage.app;
      const specs = document.createElement('div');
      specs.className = 'stage-specs';
      const idSpan = document.createElement('span');
      idSpan.className = 'spec-mono';
      idSpan.textContent = stage.stageId;
      specs.appendChild(idSpan);
      if (stage.kind) specs.appendChild(makeTextSpan(stage.kind));
      if (Array.isArray(stage.size) && stage.size.length >= 2 && stage.size[0] > 0) {
        const scaleText = stage.scale && stage.scale !== 1
          ? ` (${stage.scale.toFixed(2).replace(/\.?0+$/, '')}x)`
          : '';
        specs.appendChild(makeTextSpan(`${stage.size[0]}×${stage.size[1]}${scaleText}`));
      }
      if (stage.pid) specs.appendChild(makeTextSpan(`PID ${stage.pid}`));
      if (isAgent && stage.inputOwner.leaseId) {
        const leaseSpan = document.createElement('span');
        leaseSpan.className = 'spec-mono';
        leaseSpan.textContent = `lease ${stage.inputOwner.leaseId}`;
        specs.appendChild(leaseSpan);
      }
      titles.append(titleEl, specs);
      ident.append(icon, titles);

      const badges = document.createElement('div');
      badges.className = 'stage-badges';
      badges.appendChild(makeChip(isDesktop ? '桌面显示' : '后台离屏', isDesktop ? 'chip-emerald' : 'chip-neutral'));
      if (isAgent) badges.appendChild(makeChip(`AI · ${stage.inputOwner.agentName || 'Agent'}`, 'chip-agent'));
      else if (isFrozen) badges.appendChild(makeChip('已冻结', 'chip-amber'));
      else badges.appendChild(makeChip('人类控制', 'chip-blue'));
      top.append(ident, badges);

      const actionBar = document.createElement('div');
      actionBar.className = 'stage-actions';
      const leftActions = document.createElement('div');
      leftActions.className = 'stage-actions-left';
      const btnTakeover = document.createElement('button');
      btnTakeover.type = 'button';
      btnTakeover.className = isAgent
        ? 'btn btn-agent btn-sm'
        : (!isDesktop || isFrozen) ? 'btn btn-primary btn-sm' : 'btn btn-secondary btn-sm';
      btnTakeover.textContent = isAgent ? '立即接管' : '接管并显示';
      btnTakeover.onclick = () => runStageAction('stage.takeover', { stageId: stage.stageId });
      const btnDisplay = document.createElement('button');
      btnDisplay.type = 'button';
      btnDisplay.className = 'btn btn-secondary btn-sm';
      btnDisplay.textContent = isDesktop ? '收入后台' : '恢复桌面';
      btnDisplay.onclick = () => runStageAction(isDesktop ? 'stage.hide' : 'stage.show', { stageId: stage.stageId });
      const btnFreeze = document.createElement('button');
      btnFreeze.type = 'button';
      btnFreeze.className = 'btn btn-secondary btn-sm';
      btnFreeze.textContent = isFrozen ? '恢复输入' : '冻结输入';
      btnFreeze.onclick = () => runStageAction('stage.freeze', { stageId: stage.stageId, freeze: !isFrozen });
      leftActions.append(btnTakeover, btnDisplay, btnFreeze);
      const btnClose = document.createElement('button');
      btnClose.type = 'button';
      btnClose.className = 'btn btn-danger btn-sm';
      btnClose.textContent = '关闭舞台';
      btnClose.onclick = () => runStageAction('stage.close', { stageId: stage.stageId });
      actionBar.append(leftActions, btnClose);
      card.append(top, actionBar);
      frag.appendChild(card);
    }
    els.stagesList.appendChild(frag);
  }

  function renderDaemonTab() {
    els.daemonDetailText.textContent = state.daemonAlive
      ? `正在运行（WSL ${state.distro}）· 当前托管 ${state.stages.length} 个舞台`
      : `当前未运行（WSL ${state.distro}），启动应用时将自动拉起`;
    els.btnDaemonToggle.textContent = state.daemonAlive ? '停止守护' : '启动守护';
    els.btnDaemonToggle.className = state.daemonAlive ? 'btn btn-danger' : 'btn btn-primary';
    els.hotkeyStatusPill.textContent = state.hotkeyRegistered ? '已注册' : '未注册';
    els.hotkeyStatusPill.className = state.hotkeyRegistered ? 'chip chip-emerald' : 'chip chip-amber';
    els.hotkeyDetailText.textContent = state.hotkeyRegistered
      ? '随时按下 Ctrl+Alt+Space 作废所有 AI 租约并将后台舞台恢复到桌面'
      : 'Ctrl+Alt+Space 被其他程序占用，你仍可通过此处或系统托盘菜单随时接管';
    els.toggleClipboard.checked = state.clipboardSyncEnabled;
    els.toggleStartup.checked = state.startupEnabled;
  }

  function makeChip(text, className) {
    const span = document.createElement('span');
    span.className = `chip ${className}`;
    span.textContent = text;
    return span;
  }

  function makeTextSpan(text) {
    const span = document.createElement('span');
    span.textContent = text;
    return span;
  }

  // ---------------- Linux Settings ----------------
  async function loadLinuxSettings(method = 'linuxSettings.get', params = {}, successToast = null) {
    if (state.linuxSettingsBusy) return;
    state.linuxSettingsBusy = true;
    els.selectBrowser.disabled = true;
    els.selectTerminal.disabled = true;
    els.btnLinuxSync.disabled = true;
    els.btnLinuxReload.disabled = true;
    try {
      const data = await bridge.invoke(method, params);
      state.linuxSettings = data;
      renderLinuxSettings(data);
      if (successToast) showToast(successToast, 'success');
    } catch (err) {
      els.healthXdg.textContent = '读取失败';
      els.healthXdg.className = 'chip chip-danger';
      els.healthPortal.textContent = '未知';
      els.healthPortal.className = 'chip chip-neutral';
      els.healthMimeapps.textContent = '未知';
      els.healthMimeapps.className = 'chip chip-neutral';
      els.protocolsList.innerHTML = `<span class='muted-text'>${escapeHtml(err.message)}</span>`;
      if (successToast) showToast(`设置未保存：${err.message}`, 'error');
    } finally {
      state.linuxSettingsBusy = false;
      els.btnLinuxSync.disabled = false;
      els.btnLinuxReload.disabled = false;
    }
  }

  function renderLinuxSettings(data) {
    populateSelect(els.selectBrowser, data.browsers || [], data.browser);
    populateSelect(els.selectTerminal, data.terminals || [], data.terminal);
    els.selectBrowser.disabled = (data.browsers || []).length === 0;
    els.selectTerminal.disabled = (data.terminals || []).length === 0;
    els.windowsHostHint.classList.toggle('is-hidden', data.browser !== 'windows-host');

    els.healthXdg.textContent = data.xdgOpenInstalled ? '已安装' : '缺少 xdg-open';
    els.healthXdg.className = data.xdgOpenInstalled ? 'chip chip-emerald' : 'chip chip-danger';
    els.healthPortal.textContent = data.portalActive ? '运行中' : '未运行';
    els.healthPortal.className = data.portalActive ? 'chip chip-emerald' : 'chip chip-amber';
    els.healthMimeapps.textContent = data.mimeappsSynced ? '已同步' : '未同步';
    els.healthMimeapps.className = data.mimeappsSynced ? 'chip chip-emerald' : 'chip chip-amber';

    const protocols = Object.keys(data.protocols || {}).sort((a, b) => {
      const rank = (s) => (s === 'http' || s === 'https') ? 0 : s === 'vscode' ? 1 : 2;
      return rank(a) - rank(b) || a.localeCompare(b);
    });
    els.protocolsList.innerHTML = '';
    if (protocols.length === 0) {
      els.protocolsList.innerHTML = `<span class='muted-text'>暂无已关联协议，可点击右上角“同步协议关联”。</span>`;
      return;
    }
    for (const scheme of protocols) {
      const chip = document.createElement('span');
      chip.className = 'protocol-chip';
      chip.textContent = `${scheme}://`;
      chip.title = data.protocols[scheme] || scheme;
      els.protocolsList.appendChild(chip);
    }
  }

  function populateSelect(selectEl, items, selectedId) {
    selectEl.innerHTML = '';
    for (const item of items) {
      const opt = document.createElement('option');
      opt.value = item.id;
      opt.textContent = item.name;
      if (item.id === selectedId) opt.selected = true;
      selectEl.appendChild(opt);
    }
  }

  els.selectBrowser.addEventListener('change', () => {
    const id = els.selectBrowser.value;
    if (id) loadLinuxSettings('linuxSettings.setBrowser', { id }, '已设置默认浏览器');
  });
  els.selectTerminal.addEventListener('change', () => {
    const id = els.selectTerminal.value;
    if (id) loadLinuxSettings('linuxSettings.setTerminal', { id }, '已设置默认终端');
  });
  els.btnLinuxSync.addEventListener('click', () => loadLinuxSettings('linuxSettings.syncHandlers', {}, '已同步协议关联'));
  els.btnLinuxReload.addEventListener('click', () => loadLinuxSettings('linuxSettings.get', {}, '已刷新 Linux 设置'));

  // ---------------- Actions ----------------
  const uninstallingIds = new Set();

  function uninstallActionFor(app) {
    const src = (app.source || '').toLowerCase();
    const distro = state.distro || 'WSL';
    if (src === 'deb') {
      return {
        label: '删除安装包',
        message: `将删除下载目录中的 Linux 安装包 “${app.displayName}”。不会卸载已经安装的软件，此操作不可撤销。`
      };
    }
    if (src === 'appimage') {
      return {
        label: '删除 AppImage',
        message: `将删除 “${app.displayName}” 的 AppImage 原文件与 WSL 缓存，之后无法从启动器打开，此操作不可撤销。`
      };
    }
    return {
      label: '卸载应用',
      message: `将从 ${distro} 卸载 “${app.displayName}”，并同步清理失效的 Windows 开始菜单快捷方式，此操作不可撤销。`
    };
  }

  function confirmUninstallApp(app) {
    if (uninstallingIds.has(app.id)) return;
    const action = uninstallActionFor(app);
    openConfirmModal({
      title: action.label,
      message: action.message,
      confirmLabel: action.label,
      danger: true,
      onConfirm: async () => {
        uninstallingIds.add(app.id);
        showToast(`正在${action.label}…`, 'info');
        try {
          const res = await bridge.invoke('app.uninstall', { appId: app.id });
          if (!res.ok) {
            showToast(res.message || `${action.label}失败`, 'error');
            return;
          }
          applyAppsPayload(res);
          showToast(res.message || `${action.label}完成`, 'success');
        } catch (err) {
          showToast(`${action.label}失败：${err.message}`, 'error');
        } finally {
          uninstallingIds.delete(app.id);
        }
      }
    });
  }

  async function handleActivateApp(app) {
    const isDeb = (app.source || '').toLowerCase() === 'deb';
    if (isDeb) {
      openConfirmModal({
        title: '安装 .deb 安装包',
        message: `检测到下载的 Linux 安装包 “${app.displayName}”。是否立即安装到 WSL (${state.distro})？安装完成后会自动出现在应用桌面中。`,
        confirmLabel: '立即安装',
        onConfirm: async () => {
          showToast(`正在安装 ${app.displayName}…`, 'info');
          try {
            const res = await bridge.invoke('app.activate', { appId: app.id });
            showToast(res.message || (res.ok ? '安装完成' : '安装失败'), res.ok ? 'success' : 'error');
          } catch (err) {
            showToast(`安装失败：${err.message}`, 'error');
          }
        }
      });
      return;
    }
    showToast(`正在启动 ${app.displayName}…`, 'info');
    try {
      const res = await bridge.invoke('app.activate', { appId: app.id });
      showToast(res.message, res.ok ? 'success' : 'error');
    } catch (err) {
      showToast(`启动失败：${err.message}`, 'error');
    }
  }

  async function handleOpenAppCenter() {
    showToast('正在查找 Ubuntu 应用中心…', 'info');
    try {
      const res = await bridge.invoke('app.openAppCenter');
      if (res.status === 'needs_install') {
        openConfirmModal({
          title: '安装 Ubuntu 应用中心',
          message: `当前 WSL (${state.distro}) 尚未安装 Ubuntu 应用中心 (snap-store / gnome-software)。是否立即自动安装并打开？安装的应用会自动出现在启动器中。`,
          confirmLabel: '安装并打开',
          onConfirm: async () => {
            showToast('正在安装 Ubuntu 应用中心（首次约需 1–2 分钟）…', 'info');
            try {
              const inst = await bridge.invoke('app.installAndOpenAppCenter');
              showToast(inst.message, inst.ok ? 'success' : 'error');
            } catch (err) {
              showToast(`安装应用中心失败：${err.message}`, 'error');
            }
          }
        });
      } else {
        showToast(res.message, res.status === 'launched' ? 'success' : 'error');
      }
    } catch (err) {
      showToast(`打开应用中心失败：${err.message}`, 'error');
    }
  }

  async function handleRefreshApps() {
    try {
      const res = await bridge.invoke('apps.refresh');
      applyAppsPayload(res);
      showToast(`已刷新，共 ${res.count} 个应用`, 'success');
    } catch (err) {
      showToast(`刷新失败：${err.message}`, 'error');
    }
  }

  async function handleAutoOrganize() {
    try {
      const res = await bridge.invoke('layout.autoOrganize');
      closeFolder();
      applyLayoutPayload(res.layout);
      showToast('已按应用类别整理桌面文件夹', 'success');
    } catch (err) {
      showToast(err.message, 'error');
    }
  }

  async function handleSyncStartMenu() {
    els.btnSyncStartMenu.disabled = true;
    showToast('正在同步 Linux 应用到 Windows 开始菜单…', 'info');
    try {
      const res = await bridge.invoke('shortcut.syncAll');
      showToast(res.message, res.ok ? 'success' : 'error');
    } catch (err) {
      showToast(`同步失败：${err.message}`, 'error');
    } finally {
      els.btnSyncStartMenu.disabled = false;
    }
  }

  async function handleDaemonToggle() {
    els.btnSidebarDaemon.disabled = true;
    els.btnDaemonToggle.disabled = true;
    try {
      if (state.daemonAlive) {
        const res = await bridge.invoke('daemon.stop');
        showToast(res.message || '已停止 focusstaged', res.ok ? 'success' : 'error');
      } else {
        showToast('正在启动 focusstaged…', 'info');
        const res = await bridge.invoke('daemon.start');
        showToast(res.daemonAlive ? 'focusstaged 已启动' : '启动守护进程超时', res.daemonAlive ? 'success' : 'error');
      }
    } catch (err) {
      showToast(err.message, 'error');
    } finally {
      els.btnSidebarDaemon.disabled = false;
      els.btnDaemonToggle.disabled = false;
    }
  }

  async function runStageAction(method, params) {
    try {
      const res = await bridge.invoke(method, params);
      if (res && res.message) showToast(res.message, res.ok === false ? 'error' : 'success');
    } catch (err) {
      showToast(err.message, 'error');
    }
  }

  async function handleMoveAppToFolder(appId, folderId) {
    const folder = findFolder(folderId);
    try {
      const res = await bridge.invoke('layout.moveAppToFolder', { appId, folderId });
      applyLayoutPayload(res.layout);
      if (folder) showToast(`已移入文件夹 “${folder.name}”`, 'success');
    } catch (err) {
      showToast(`移动失败：${err.message}`, 'error');
    }
  }

  async function handleMoveAppToRoot(appId) {
    const app = state.appById.get(appId);
    try {
      const res = await bridge.invoke('layout.moveAppToRoot', { appId });
      applyLayoutPayload(res.layout);
      if (app) showToast(`已将 “${app.displayName}” 移回主桌面`, 'info');
    } catch (err) {
      showToast(`移动失败：${err.message}`, 'error');
    }
  }

  async function handleMergeApps(sourceAppId, targetAppId) {
    try {
      const res = await bridge.invoke('layout.mergeAppsIntoFolder', {
        sourceAppId,
        targetAppId,
        name: '新建文件夹'
      });
      applyLayoutPayload(res.layout);
      if (res.folder) {
        openFolder(res.folder.id);
        showToast(`已创建文件夹 “${res.folder.name}”`, 'success');
      }
    } catch (err) {
      showToast(`创建文件夹失败：${err.message}`, 'error');
    }
  }

  async function handleReorderFolder(folderId, fromAppId, toAppId, insertBefore) {
    const folder = findFolder(folderId);
    if (!folder) return;
    const ids = folder.appIds.filter((id) => id !== fromAppId);
    let idx = ids.indexOf(toAppId);
    if (idx < 0) return;
    if (!insertBefore) idx += 1;
    ids.splice(idx, 0, fromAppId);
    try {
      const res = await bridge.invoke('layout.reorderFolder', { folderId, appIds: ids });
      applyLayoutPayload(res.layout);
    } catch (err) {
      showToast(`排序失败：${err.message}`, 'error');
    }
  }

  async function handleDissolveFolder(folder) {
    try {
      const res = await bridge.invoke('layout.dissolveFolder', { folderId: folder.id });
      if (state.openFolderId === folder.id) closeFolder();
      applyLayoutPayload(res.layout);
      showToast(`已解散文件夹 “${folder.name}”`, 'info');
    } catch (err) {
      showToast(`解散失败：${err.message}`, 'error');
    }
  }

  els.btnSidebarDaemon.addEventListener('click', handleDaemonToggle);
  els.btnDaemonToggle.addEventListener('click', handleDaemonToggle);
  els.btnRefreshStages.addEventListener('click', async () => {
    try {
      const res = await bridge.invoke('stages.refresh');
      applyStatePayload(res);
      showToast('已刷新舞台状态', 'info');
    } catch (err) {
      showToast(err.message, 'error');
    }
  });
  els.btnTakeoverAll.addEventListener('click', async () => {
    try {
      await bridge.invoke('stage.takeoverAll');
      showToast('已接管全部舞台', 'success');
    } catch (err) {
      showToast(err.message, 'error');
    }
  });
  els.toggleStartup.addEventListener('change', async () => {
    const enabled = els.toggleStartup.checked;
    try {
      await bridge.invoke('settings.setStartup', { enabled });
      state.startupEnabled = enabled;
      showToast(enabled ? '已开启开机自启' : '已关闭开机自启', 'success');
    } catch (err) {
      els.toggleStartup.checked = !enabled;
      showToast(`设置开机自启失败：${err.message}`, 'error');
    }
  });
  els.toggleClipboard.addEventListener('change', async () => {
    const enabled = els.toggleClipboard.checked;
    try {
      await bridge.invoke('settings.setClipboardSync', { enabled });
      state.clipboardSyncEnabled = enabled;
      showToast(enabled ? '已开启剪贴板同步' : '已关闭剪贴板同步', 'info');
    } catch (err) {
      els.toggleClipboard.checked = !enabled;
      showToast(err.message, 'error');
    }
  });
  els.btnRefreshApps.addEventListener('click', handleRefreshApps);
  els.btnAutoOrganize.addEventListener('click', handleAutoOrganize);
  els.btnSyncStartMenu.addEventListener('click', handleSyncStartMenu);

  // ---------------- Modals ----------------
  let activeModalCancel = null;

  function openModal({ title, bodyNode, buttons }) {
    closeContextMenu();
    els.modalTitle.textContent = title;
    els.modalBody.innerHTML = '';
    els.modalBody.appendChild(bodyNode);
    els.modalFooter.innerHTML = '';
    for (const b of buttons) {
      const btn = document.createElement('button');
      btn.type = 'button';
      btn.className = b.className || 'btn btn-secondary';
      btn.textContent = b.label;
      btn.onclick = b.onClick;
      els.modalFooter.appendChild(btn);
    }
    activeModalCancel = closeModal;
    els.modalOverlay.classList.remove('is-hidden');
    const focusTarget = els.modalBody.querySelector('input, button');
    if (focusTarget) {
      requestAnimationFrame(() => {
        focusTarget.focus();
        if (focusTarget.select) focusTarget.select();
      });
    }
  }

  function closeModal() {
    els.modalOverlay.classList.add('is-hidden');
    activeModalCancel = null;
  }

  els.modalClose.addEventListener('click', closeModal);
  els.modalOverlay.addEventListener('click', (e) => {
    if (e.target === els.modalOverlay) closeModal();
  });

  function openCreateFolderModal(initialAppId = null) {
    const wrap = document.createElement('div');
    const label = document.createElement('label');
    label.className = 'field-label';
    label.textContent = '文件夹名称';
    const input = document.createElement('input');
    input.type = 'text';
    input.className = 'text-input';
    input.value = '新建文件夹';
    wrap.append(label, input);

    const submit = async () => {
      const name = input.value.trim();
      if (!name) return;
      closeModal();
      try {
        const res = await bridge.invoke('layout.createFolder', { name, initialAppId: initialAppId || '' });
        applyLayoutPayload(res.layout);
        showToast(initialAppId ? `已创建文件夹 “${name}” 并移入应用` : `已创建文件夹 “${name}”`, 'success');
      } catch (err) {
        showToast(`创建失败：${err.message}`, 'error');
      }
    };
    input.addEventListener('keydown', (e) => {
      if (e.key === 'Enter') {
        e.preventDefault();
        submit();
      }
    });
    openModal({
      title: '新建文件夹',
      bodyNode: wrap,
      buttons: [
        { label: '取消', className: 'btn btn-secondary', onClick: closeModal },
        { label: '创建', className: 'btn btn-primary', onClick: submit }
      ]
    });
  }

  function openRenameFolderModal(folder) {
    const wrap = document.createElement('div');
    const label = document.createElement('label');
    label.className = 'field-label';
    label.textContent = '文件夹名称';
    const input = document.createElement('input');
    input.type = 'text';
    input.className = 'text-input';
    input.value = folder.name;
    wrap.append(label, input);
    const submit = async () => {
      const name = input.value.trim();
      if (!name) return;
      closeModal();
      try {
        const res = await bridge.invoke('layout.renameFolder', { folderId: folder.id, name });
        applyLayoutPayload(res.layout);
        showToast(`已重命名为 “${name}”`, 'success');
      } catch (err) {
        showToast(`重命名失败：${err.message}`, 'error');
      }
    };
    input.addEventListener('keydown', (e) => {
      if (e.key === 'Enter') {
        e.preventDefault();
        submit();
      }
    });
    openModal({
      title: '重命名文件夹',
      bodyNode: wrap,
      buttons: [
        { label: '取消', className: 'btn btn-secondary', onClick: closeModal },
        { label: '保存', className: 'btn btn-primary', onClick: submit }
      ]
    });
  }

  function openCustomCommandModal(initialValue = '') {
    const wrap = document.createElement('div');
    const label = document.createElement('label');
    label.className = 'field-label';
    label.textContent = 'Linux 启动命令（支持带引号参数）';
    const input = document.createElement('input');
    input.type = 'text';
    input.className = 'text-input mono';
    input.placeholder = '例如：chromium, code --no-sandbox, nautilus';
    input.value = initialValue;
    const errBox = document.createElement('div');
    errBox.className = 'field-error is-hidden';
    const examples = document.createElement('div');
    examples.className = 'example-chips';
    for (const ex of ['chromium', 'code --no-sandbox', 'nautilus', 'firefox']) {
      const chip = document.createElement('button');
      chip.type = 'button';
      chip.className = 'example-chip';
      chip.textContent = ex;
      chip.onclick = () => {
        input.value = ex;
        errBox.classList.add('is-hidden');
        input.focus();
      };
      examples.appendChild(chip);
    }
    wrap.append(label, input, errBox, examples);

    const submit = async () => {
      const commandLine = input.value.trim();
      if (!commandLine) {
        errBox.textContent = '请输入要运行的 Linux 命令。';
        errBox.classList.remove('is-hidden');
        input.focus();
        return;
      }
      try {
        errBox.classList.add('is-hidden');
        const res = await bridge.invoke('app.launchCustom', { commandLine });
        if (res.ok) {
          closeModal();
          showToast(res.message, 'success');
        } else {
          errBox.textContent = res.message || '启动失败';
          errBox.classList.remove('is-hidden');
        }
      } catch (err) {
        errBox.textContent = err.message;
        errBox.classList.remove('is-hidden');
        input.focus();
      }
    };
    input.addEventListener('keydown', (e) => {
      if (e.key === 'Enter') {
        e.preventDefault();
        submit();
      }
    });
    openModal({
      title: '运行自定义命令',
      bodyNode: wrap,
      buttons: [
        { label: '取消', className: 'btn btn-secondary', onClick: closeModal },
        { label: '启动', className: 'btn btn-primary', onClick: submit }
      ]
    });
  }

  function openAppPropertiesModal(app) {
    const cmdDesc = app.args && app.args.length > 0 ? `${app.cmd} ${app.args.join(' ')}` : app.cmd;
    const rows = [
      ['名称', app.displayName, false],
      ['来源', app.source || 'desktop', true],
      ['命令', cmdDesc, true],
      ['WM_CLASS', app.wmClass || '—', true],
      ['文件', app.filePath || '—', true]
    ];
    const table = document.createElement('div');
    table.className = 'prop-table';
    for (const [k, v, mono] of rows) {
      const keyEl = document.createElement('div');
      keyEl.className = 'prop-key';
      keyEl.textContent = k;
      const valEl = document.createElement('div');
      valEl.className = 'prop-val' + (mono ? ' mono' : '');
      valEl.textContent = v;
      table.append(keyEl, valEl);
    }
    openModal({
      title: '应用属性',
      bodyNode: table,
      buttons: [{ label: '关闭', className: 'btn btn-primary', onClick: closeModal }]
    });
  }

  function openConfirmModal({ title, message, confirmLabel, danger = false, onConfirm }) {
    const p = document.createElement('p');
    p.style.margin = '0';
    p.style.lineHeight = '1.6';
    p.textContent = message;
    openModal({
      title,
      bodyNode: p,
      buttons: [
        { label: '取消', className: 'btn btn-secondary', onClick: closeModal },
        { label: confirmLabel, className: danger ? 'btn btn-danger' : 'btn btn-primary', onClick: () => {
          closeModal();
          onConfirm();
        } }
      ]
    });
  }

  // ---------------- Context Menus ----------------
  function closeContextMenu() {
    els.contextMenu.classList.add('is-hidden');
    els.contextMenu.innerHTML = '';
  }

  function showContextMenu(items, x, y) {
    els.contextMenu.innerHTML = '';
    for (const item of items) {
      if (item.type === 'sep') {
        const sep = document.createElement('div');
        sep.className = 'ctx-sep';
        els.contextMenu.appendChild(sep);
        continue;
      }
      if (item.type === 'label') {
        const lbl = document.createElement('div');
        lbl.className = 'ctx-section-label';
        lbl.textContent = item.label;
        els.contextMenu.appendChild(lbl);
        continue;
      }
      const btn = document.createElement('button');
      btn.type = 'button';
      btn.className = 'ctx-item' + (item.primary ? ' is-primary' : '') + (item.danger ? ' is-danger' : '');
      btn.textContent = item.label;
      btn.onclick = () => {
        closeContextMenu();
        item.onClick();
      };
      els.contextMenu.appendChild(btn);
    }
    els.contextMenu.classList.remove('is-hidden');
    const rect = els.contextMenu.getBoundingClientRect();
    const maxX = window.innerWidth - rect.width - 10;
    const maxY = window.innerHeight - rect.height - 10;
    els.contextMenu.style.left = `${Math.max(8, Math.min(x, maxX))}px`;
    els.contextMenu.style.top = `${Math.max(8, Math.min(y, maxY))}px`;
  }

  function openAppContextMenu(app, inFolderId, x, y) {
    const isDeb = (app.source || '').toLowerCase() === 'deb';
    const pinned = isPinned('app', app.id);
    const items = [{ label: isDeb ? '安装此 .deb 安装包' : '打开应用', primary: true, onClick: () => handleActivateApp(app) }];
    if (!isDeb) {
      items.push(
        { label: '添加到 Windows 开始菜单', onClick: async () => {
          try {
            const res = await bridge.invoke('shortcut.addToStartMenu', { appId: app.id });
            showToast(res.message, 'success');
          } catch (err) { showToast(err.message, 'error'); }
        } },
        { label: '发送到 Windows 桌面', onClick: async () => {
          try {
            const res = await bridge.invoke('shortcut.addToDesktop', { appId: app.id });
            showToast(res.message, 'success');
          } catch (err) { showToast(err.message, 'error'); }
        } }
      );
    }
    items.push(
      {
        label: pinned ? '从快捷栏移除' : '固定到快捷栏',
        onClick: () => pinned ? unpinDockTarget('app', app.id) : pinDockItem('app', app.id)
      },
      { type: 'sep' },
      { type: 'label', label: '整理到文件夹' }
    );
    for (const folder of state.layout.folders) {
      if (folder.id === inFolderId) continue;
      items.push({ label: `移入 “${folder.name}”`, onClick: () => handleMoveAppToFolder(app.id, folder.id) });
    }
    items.push({ label: '+ 新建文件夹并移入…', onClick: () => openCreateFolderModal(app.id) });
    if (inFolderId !== null) {
      items.push({ label: '移出到主桌面', onClick: () => handleMoveAppToRoot(app.id) });
    }
    items.push(
      { type: 'sep' },
      { label: '查看命令与属性', onClick: () => openAppPropertiesModal(app) },
      { type: 'sep' },
      { label: uninstallActionFor(app).label, danger: true, onClick: () => confirmUninstallApp(app) }
    );
    showContextMenu(items, x, y);
  }

  function openFolderContextMenu(folder, x, y) {
    const pinned = isPinned('folder', folder.id);
    showContextMenu([
      { label: '打开文件夹', primary: true, onClick: () => openFolder(folder.id) },
      {
        label: pinned ? '从快捷栏移除' : '固定到快捷栏',
        onClick: () => pinned ? unpinDockTarget('folder', folder.id) : pinDockItem('folder', folder.id)
      },
      { label: '重命名文件夹', onClick: () => openRenameFolderModal(folder) },
      { label: '将文件夹内应用添加到开始菜单', onClick: async () => {
        try {
          const res = await bridge.invoke('shortcut.addFolderToStartMenu', { folderId: folder.id });
          showToast(res.message, 'success');
        } catch (err) { showToast(err.message, 'error'); }
      } },
      { type: 'sep' },
      { label: '解散文件夹', danger: true, onClick: () => handleDissolveFolder(folder) }
    ], x, y);
  }

  function openDesktopContextMenu(x, y) {
    showContextMenu([
      { label: '新建文件夹', onClick: () => openCreateFolderModal() },
      { label: '按类别自动整理文件夹', onClick: handleAutoOrganize },
      { label: '自定义快捷栏…', onClick: () => openDockManager() },
      { label: '外观与字体…', onClick: () => setView('control', 'appearance') },
      { type: 'sep' },
      { label: '打开 Ubuntu 应用中心', onClick: handleOpenAppCenter },
      { label: '运行自定义命令…', onClick: () => openCustomCommandModal() },
      { label: '刷新应用列表', onClick: handleRefreshApps },
      { type: 'sep' },
      { label: '打开舞台中心', onClick: () => setView('control', 'stages') }
    ], x, y);
  }

  document.addEventListener('click', (e) => {
    if (!e.target.closest('#context-menu')) closeContextMenu();
  });

  // ---------------- Appearance Settings ----------------
  function renderAppearanceSettings() {
    const a = state.layout.appearance;
    const preset = themeById(a.theme);
    const base = a.customColors
      ? { background: a.background, surface: a.surface, ink: a.ink, accent: a.accent }
      : preset.colors;

    els.themeGrid.innerHTML = '';
    const customTheme = {
      id: 'custom',
      name: '自定义',
      tag: '专属配色',
      colors: base
    };
    for (const theme of [...THEMES, customTheme]) {
      const card = document.createElement('button');
      card.type = 'button';
      card.className = 'theme-card';
      card.dataset.themeId = theme.id;
      const active = a.customColors ? theme.id === 'custom' : theme.id === a.theme;
      card.classList.toggle('is-active', active);
      card.setAttribute('aria-pressed', active ? 'true' : 'false');

      const preview = document.createElement('span');
      preview.className = 'theme-preview';
      preview.style.background = theme.colors.background;
      preview.style.setProperty('--preview-surface', theme.colors.surface);
      preview.style.setProperty('--preview-accent', theme.colors.accent);

      const meta = document.createElement('span');
      meta.className = 'theme-card-meta';
      const name = document.createElement('span');
      name.className = 'theme-name';
      name.textContent = theme.name;
      const tag = document.createElement('span');
      tag.className = 'theme-tag';
      tag.textContent = theme.tag;
      meta.append(name, tag);
      card.append(preview, meta);

      card.addEventListener('click', () => {
        if (theme.id === 'custom') {
          saveAppearance({ ...state.layout.appearance, customColors: true });
        } else {
          saveAppearance({
            ...state.layout.appearance,
            theme: theme.id,
            customColors: false,
            background: theme.colors.background,
            surface: theme.colors.surface,
            ink: theme.colors.ink,
            accent: theme.colors.accent
          });
        }
      });
      els.themeGrid.appendChild(card);
    }

    els.toggleCustomColors.checked = a.customColors;
    els.colorGrid.classList.toggle('is-disabled', !a.customColors);
    els.colorBackground.value = normalizeHex(base.background, preset.colors.background);
    els.colorSurface.value = normalizeHex(base.surface, preset.colors.surface);
    els.colorInk.value = normalizeHex(base.ink, preset.colors.ink);
    els.colorAccent.value = normalizeHex(base.accent, preset.colors.accent);

    els.selectFontPreset.value = a.fontPreset;
    els.inputCustomFont.value = a.customFont;
    els.rangeFontSize.value = String(a.fontSize);
    els.rangeColumns.value = String(a.columns);
    els.rangeTileScale.value = String(a.tileScale);
    els.toggleShowGrid.checked = a.showGrid;
    els.toggleShowDock.checked = a.showDock;
    els.toggleDockLabels.checked = a.dockLabels;
    updateAppearanceOutputs();
  }

  function updateAppearanceOutputs() {
    els.outputFontSize.textContent = `${els.rangeFontSize.value} px`;
    els.outputColumns.textContent = `${els.rangeColumns.value} 列`;
    els.outputTileScale.textContent = `${els.rangeTileScale.value}%`;
  }

  function readAppearanceControls() {
    const current = state.layout.appearance;
    const preset = themeById(current.theme);
    const custom = els.toggleCustomColors.checked;
    const colors = custom
      ? {
          background: els.colorBackground.value,
          surface: els.colorSurface.value,
          ink: els.colorInk.value,
          accent: els.colorAccent.value
        }
      : preset.colors;
    return normalizeAppearance({
      ...current,
      customColors: custom,
      ...colors,
      fontPreset: els.selectFontPreset.value,
      customFont: els.inputCustomFont.value,
      fontSize: els.rangeFontSize.value,
      columns: els.rangeColumns.value,
      tileScale: els.rangeTileScale.value,
      showGrid: els.toggleShowGrid.checked,
      showDock: els.toggleShowDock.checked,
      dockLabels: els.toggleDockLabels.checked
    });
  }

  function previewAppearanceFromControls() {
    const draft = readAppearanceControls();
    applyAppearance(draft);
    renderAppsGrid();
    renderDock();
    updateAppearanceOutputs();
  }

  async function saveAppearance(patch) {
    const appearance = normalizeAppearance({ ...state.layout.appearance, ...patch });
    state.layout.appearance = appearance;
    applyAppearance(appearance);
    renderAppsGrid();
    renderDock();
    renderAppearanceSettings();
    try {
      const res = await bridge.invoke('appearance.set', { appearance });
      setLayout(res.layout);
      renderLayoutViews();
    } catch (err) {
      showToast(`保存外观失败：${err.message}`, 'error');
    }
  }

  function wireAppearanceControls() {
    for (const input of [els.colorBackground, els.colorSurface, els.colorInk, els.colorAccent]) {
      input.addEventListener('input', () => {
        els.toggleCustomColors.checked = true;
        els.colorGrid.classList.remove('is-disabled');
        previewAppearanceFromControls();
      });
      input.addEventListener('change', () => saveAppearance(readAppearanceControls()));
    }

    for (const range of [els.rangeFontSize, els.rangeColumns, els.rangeTileScale]) {
      range.addEventListener('input', previewAppearanceFromControls);
      range.addEventListener('change', () => saveAppearance(readAppearanceControls()));
    }

    els.toggleCustomColors.addEventListener('change', () => {
      if (!els.toggleCustomColors.checked) {
        const colors = themeById(state.layout.appearance.theme).colors;
        els.colorBackground.value = colors.background;
        els.colorSurface.value = colors.surface;
        els.colorInk.value = colors.ink;
        els.colorAccent.value = colors.accent;
      }
      els.colorGrid.classList.toggle('is-disabled', !els.toggleCustomColors.checked);
      saveAppearance(readAppearanceControls());
    });

    els.selectFontPreset.addEventListener('change', () => {
      if (els.selectFontPreset.value === 'custom' && !els.inputCustomFont.value.trim()) {
        els.inputCustomFont.focus();
      }
      saveAppearance(readAppearanceControls());
    });
    els.inputCustomFont.addEventListener('change', () => {
      els.selectFontPreset.value = 'custom';
      saveAppearance(readAppearanceControls());
    });
    for (const toggle of [els.toggleShowGrid, els.toggleShowDock, els.toggleDockLabels]) {
      toggle.addEventListener('change', () => saveAppearance(readAppearanceControls()));
    }

    els.btnManageDock.addEventListener('click', () => openDockManager());
    els.btnResetAppearance.addEventListener('click', () => {
      saveAppearance({ ...DEFAULT_APPEARANCE });
      showToast('已恢复默认外观', 'success');
    });
  }

  // ---------------- Global Keyboard Shortcuts ----------------
  document.addEventListener('keydown', (e) => {
    if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'f') {
      e.preventDefault();
      closeContextMenu();
      closeModal();
      closeFolder();
      if (state.activeView !== 'apps') setView('apps');
      els.searchInput.focus();
      els.searchInput.select();
      return;
    }
    if (e.key === 'Escape') {
      if (!els.contextMenu.classList.contains('is-hidden')) {
        closeContextMenu();
        return;
      }
      if (activeModalCancel) {
        activeModalCancel();
        return;
      }
      if (state.openFolderId) {
        closeFolder();
        return;
      }
      if (state.searchQuery) {
        state.searchQuery = '';
        els.searchInput.value = '';
        updateSearchUi();
        renderAppsGrid();
        return;
      }
      if (state.activeView === 'control') setView('apps');
    }
  });

  function escapeHtml(str) {
    const entities = { '&': '&amp;', '<': '&lt;', '>': '&gt;' };
    entities[String.fromCharCode(34)] = '&quot;';
    entities[String.fromCharCode(39)] = '&#39;';
    return String(str).replace(/[&<>\x22\x27]/g, (ch) => entities[ch]);
  }

  // ---------------- Host Events & Bootstrap ----------------
  bridge.on('state', applyStatePayload);
  bridge.on('apps', (payload) => {
    applyAppsPayload(payload);
    if (payload.reason === 'auto') {
      showToast(`已自动检测到应用更新，当前共 ${state.apps.length} 个应用`, 'info');
    }
  });
  bridge.on('toast', (payload) => {
    if (payload && payload.message) showToast(payload.message, payload.tone || 'info');
  });
  bridge.on('windowState', (payload) => {
    if (payload) updateMaximizedUi(payload.maximized);
  });

  async function bootstrap() {
    if (!window.chrome || !window.chrome.webview) {
      state.appsLoaded = true;
      renderLayoutViews();
      return;
    }
    try {
      const init = await bridge.invoke('app.init');
      if (init) {
        updateMaximizedUi(init.maximized);
        applyStatePayload(init);
        applyAppsPayload(init);
      }
    } catch (err) {
      showToast(`初始化失败：${err.message}`, 'error');
    }
  }

  wireAppearanceControls();
  applyAppearance(DEFAULT_APPEARANCE);
  renderLayoutViews();
  bootstrap();
})();
