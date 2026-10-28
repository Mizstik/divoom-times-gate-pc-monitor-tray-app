// MZ TGM 2 frontend — uses the Tauri global API (withGlobalTauri), no bundler.
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const { getCurrentWindow } = window.__TAURI__.window;
const { LogicalSize } = window.__TAURI__.dpi;

const $ = (id) => document.getElementById(id);

let running = false;
let settings = {};

async function loadSettings() {
  try { settings = await invoke("load_settings"); }
  catch { settings = {}; }
  return settings;
}
function persist() {
  invoke("save_settings", { settings: readForm() }).catch(() => {});
}

function readForm() {
  const lastField = document.querySelector('input[name="lastField"]:checked');
  const interval = Math.round(Number($("intervalSec").value) * 1000);
  return {
    ip: $("ip").value.trim() || "192.168.0.100",
    hardware: Number($("hardware").value) || 400,
    intervalMs: Number.isFinite(interval) && interval >= 1000 ? interval : 2000,
    lastField: lastField ? lastField.value : "vram",
    lcdIndex: Number($("lcdIndex").value) || 0,
    presetId: Math.max(0, Number($("presetId").value) || 0),
    autoStart: $("autoStart").checked,
    autoStartMonitor: $("autoStartMonitor").checked,
    startMinimized: $("startMinimized").checked,
  };
}

function applySettings(s) {
  if (s.ip) $("ip").value = s.ip;
  if (s.hardware) $("hardware").value = String(s.hardware);
  if (s.intervalMs) $("intervalSec").value = String(Math.round(s.intervalMs / 1000));
  const radio = document.querySelector(`input[name="lastField"][value="${s.lastField || "vram"}"]`);
  if (radio) radio.checked = true;
  if (Number.isInteger(s.lcdIndex) && s.lcdIndex >= 0 && s.lcdIndex <= 4) $("lcdIndex").value = String(s.lcdIndex);
  $("presetId").value = String(s.presetId || 0);
  $("autoStartMonitor").checked = !!s.autoStartMonitor;
  $("startMinimized").checked = !!s.startMinimized;
  // autoStart checkbox reflects the registry, set after get_autostart
}

function setStatus(text, isError) {
  $("statusText").textContent = text;
  const dot = $("statusDot");
  dot.classList.remove("on", "err");
  if (running) dot.classList.add(isError ? "err" : "on");
}

function setRunning(r) {
  running = r;
  const btn = $("startStop");
  btn.textContent = r ? "Stop Monitor" : "Start Monitor";
  btn.classList.toggle("stop", r);
  if (r) setStatus("Starting…", false);
  else setStatus("Idle", false);
}

async function startMonitor(s) {
  await invoke("start_monitor", {
    config: {
      ip: s.ip,
      hardware: s.hardware,
      intervalMs: s.intervalMs,
      lastField: s.lastField,
      lcdIndex: s.lcdIndex,
      presetId: s.presetId,
    },
  });
}

$("startStop").addEventListener("click", async () => {
  if (running) {
    await invoke("stop_monitor").catch(() => {});
    return;
  }
  const s = readForm();
  persist();
  try {
    await startMonitor(s);
  } catch (e) {
    setStatus(String(e), true);
  }
});

$("exit").addEventListener("click", () => invoke("quit_app").catch(() => {}));

// ---------------------------------------------------------------- scan panel
// Opening it expands the window (760x510 → 760x740) so the panel fits between
// the settings card and the Start/Exit buttons; closing it restores the size.

const WIN_BASE = [760, 510];
const WIN_EXPANDED = [760, 740];

let scanOpen = false;
let scanDevices = []; // {src:"cloud"|"local", ip, name, hardware, deviceId?, ports?, divoom?}
let scanSelected = -1;
let scanCloudRunning = false;
let scanLocalRunning = false;

const isTimesGate = (d) => d.hardware === 400 || d.hardware === 402;

function setScanStatus(text) { $("scanStatus").textContent = text; }

function scanButtons() {
  $("scanSelect").disabled = scanSelected < 0 || scanCloudRunning || scanLocalRunning;
}

function renderScan() {
  const tb = $("scanRows");
  tb.innerHTML = "";
  scanDevices.forEach((d, i) => {
    const tr = document.createElement("tr");
    if (i === scanSelected) tr.classList.add("sel");
    if (isTimesGate(d)) tr.classList.add("tg");
    const cell = (text, cls) => {
      const td = document.createElement("td");
      td.textContent = text;
      if (cls) td.className = cls;
      tr.appendChild(td);
    };
    cell(i === scanSelected ? "◉" : "○");
    cell(d.src === "cloud" ? "Cloud" : "Local scan");
    cell(d.name);
    cell(d.ip, "ip");
    cell(d.hardware ? String(d.hardware) : "?", "hw");
    if (d.src === "cloud") {
      cell(`DeviceId ${d.deviceId}`);
    } else {
      cell(`:${d.ports.join(", :")}${d.divoom ? " · Divoom server" : ""}`);
    }
    tr.addEventListener("click", () => { scanSelected = i; renderScan(); scanButtons(); });
    tb.appendChild(tr);
  });
}

function resizeWindow(size) {
  getCurrentWindow().setSize(new LogicalSize(size[0], size[1])).catch(() => {});
}

async function runLocalScan() {
  if (scanLocalRunning) return;
  scanLocalRunning = true;
  scanButtons();
  setScanStatus("Scanning local network…");
  try {
    const hits = await invoke("scan_local");
    for (const h of hits) {
      scanDevices.push({
        src: "local", ip: h.ip, name: "Local scan",
        hardware: h.hardware, ports: h.ports, divoom: h.divoom,
      });
    }
    if (scanSelected < 0) {
      const first = scanDevices.findIndex((d) => d.src === "local" && d.divoom);
      if (first >= 0) scanSelected = first;
    }
    setScanStatus(hits.length
      ? `Done — ${hits.length} responding device(s). Select one and press Select, or close with ✕.`
      : "Done — no responding devices found on the local network.");
  } catch (e) {
    setScanStatus(`Local scan failed: ${e}`);
  }
  scanLocalRunning = false;
  renderScan();
  scanButtons();
}

async function runCloudScan() {
  if (scanCloudRunning) return;
  scanCloudRunning = true;
  scanButtons();
  setScanStatus("Querying Divoom cloud…");
  scanDevices = [];
  scanSelected = -1;
  renderScan();
  try {
    const devs = await invoke("scan_cloud");
    for (const d of devs) {
      scanDevices.push({ src: "cloud", ip: d.ip, name: d.name, hardware: d.hardware, deviceId: d.deviceId });
    }
    const tgIndex = scanDevices.findIndex((d) => d.src === "cloud" && isTimesGate(d));
    if (tgIndex >= 0) {
      // Times Gate found — local scan is deliberately skipped.
      scanSelected = tgIndex;
      renderScan();
      setScanStatus(`Cloud listed ${devs.length} device(s); Times Gate preselected. Press Select, or pick another row.`);
      scanCloudRunning = false;
      scanButtons();
      return;
    }
    if (devs.length) {
      renderScan();
      setScanStatus(`Cloud listed ${devs.length} device(s), none is a Times Gate (400/402). Scanning local network…`);
      scanCloudRunning = false;
      scanButtons();
      await runLocalScan();
      return;
    }
    setScanStatus("Cloud returned no devices. Scanning local network…");
    scanCloudRunning = false;
    scanButtons();
    await runLocalScan();
  } catch (e) {
    setScanStatus(`Cloud query failed (${e}). Scanning local network…`);
    scanCloudRunning = false;
    scanButtons();
    await runLocalScan();
  }
}

function openScan() {
  if (scanOpen) return;
  scanOpen = true;
  $("scanPanel").hidden = false;
  resizeWindow(WIN_EXPANDED);
  runCloudScan();
}

function closeScan() {
  if (!scanOpen) return;
  scanOpen = false;
  $("scanPanel").hidden = true;
  resizeWindow(WIN_BASE);
}

$("scanBtn").addEventListener("click", openScan);
$("scanClose").addEventListener("click", closeScan);
$("scanSelect").addEventListener("click", () => {
  if (scanSelected < 0) return;
  const d = scanDevices[scanSelected];
  $("ip").value = d.ip;
  if (isTimesGate(d)) $("hardware").value = String(d.hardware);
  persist();
  refreshCloudPreset();
  closeScan();
  setStatus(`Device ${d.ip} selected via scan.`, false);
});

listen("scan-progress", (e) => {
  if (scanLocalRunning) setScanStatus(`Scanning local network… ${e.payload.done}/508 probes`);
});

async function refreshCloudPreset() {
  const el = $("presetCurrent");
  el.textContent = "";
  const ip = $("ip").value.trim();
  if (!ip) return;
  try {
    const p = await invoke("get_cloud_preset", { ip });
    if (p) el.textContent = `Current ID = ${p}`;
  } catch { /* not in the cloud — nothing to show */ }
}

$("autoStart").addEventListener("change", async () => {
  const s = readForm();
  persist();
  try {
    await invoke("set_autostart", { enabled: s.autoStart });
  } catch (e) {
    setStatus(`Autostart failed: ${e}`, true);
    $("autoStart").checked = !s.autoStart;
  }
});

$("startMinimized").addEventListener("change", persist);
$("autoStartMonitor").addEventListener("change", persist);
$("ip").addEventListener("change", () => { persist(); refreshCloudPreset(); });
for (const id of ["hardware", "intervalSec", "lcdIndex", "presetId"]) {
  $(id).addEventListener("change", persist);
}
for (const r of document.querySelectorAll('input[name="lastField"]')) {
  r.addEventListener("change", persist);
}

listen("monitor-tick", (e) => {
  const p = e.payload;
  setStatus(p.ok ? p.line : `${p.line}  —  ${p.error || "send failed"}`, !p.ok);
  // an activation line carries the preset the backend used — show it too
  const m = p.line.match(/preset (\d+)/);
  if (m && !$("presetCurrent").textContent) $("presetCurrent").textContent = `Current ID = ${m[1]}`;
});
listen("monitor-state", (e) => setRunning(e.payload.running));

(async function init() {
  const s = await loadSettings();
  applySettings(s);
  refreshCloudPreset();
  try {
    $("autoStart").checked = await invoke("get_autostart");
  } catch { $("autoStart").checked = false; }
  try {
    const st = await invoke("get_monitor_state");
    setRunning(st.running);
  } catch { setRunning(false); }
  if (s.autoStartMonitor && !running) {
    try { await startMonitor(s); } catch (e) { setStatus(String(e), true); }
  }
})();
