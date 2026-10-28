use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::json;
use tauri::image::Image;
use tauri::menu::{MenuBuilder, MenuItemBuilder};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager};

/// The built-in PC Monitor clock widget (measured on Times Gate, same as
/// divoom-times-gate-monitor's activate_pc_monitor).
const PC_MONITOR_CLOCK_ID: u32 = 625;

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Config {
    ip: String,
    hardware: u32,
    interval_ms: u64,
    last_field: String, // "vram" | "hdd"
    #[serde(default)]
    lcd_index: u32,   // screen 0..4 (the UI shows 1..5)
    #[serde(default)]
    preset_id: u32,   // LcdIndependence id; 0 = keep the device's current preset
}

#[derive(Default, Clone)]
struct Stats {
    cpu_pct: Option<f64>,
    cpu_temp: Option<f64>,
    gpu_pct: Option<f64>,
    gpu_temp: Option<f64>,
    vram_used: Option<f64>, // MiB
    vram_total: Option<f64>,
    disk_temp: Option<f64>,
}

pub struct AppState {
    running: Arc<AtomicBool>,
    last_config: Mutex<Option<Config>>,
}

#[derive(Clone, Serialize)]
struct TickPayload {
    line: String,
    ok: bool,
    error: Option<String>,
}

#[derive(Clone, Serialize)]
struct StatePayload {
    running: bool,
}

fn endpoint(cfg: &Config) -> String {
    // 402 = the second Times Gate variant, which serves the API on :9000.
    if cfg.hardware == 402 {
        format!("http://{}:9000/divoom_api", cfg.ip)
    } else {
        format!("http://{}/post", cfg.ip)
    }
}

async fn send_cmd(
    client: &reqwest::Client,
    url: &str,
    payload: serde_json::Value,
) -> Result<(), String> {
    let resp = client
        .post(url)
        .json(&payload)
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    let text = resp.text().await.map_err(|e| format!("read failed: {e}"))?;
    let v: serde_json::Value =
        serde_json::from_str(&text).unwrap_or(json!({ "raw": text }));
    // Same acceptance rules as the Node project's device.js isOk():
    // no error field, and error_code 0 / missing / "DeviceToken is err".
    if v.get("error").map(|e| !e.is_null()).unwrap_or(false) {
        return Err(format!("device error: {text}"));
    }
    let ok = match v.get("error_code") {
        None => true,
        Some(serde_json::Value::Number(n)) => n.as_i64() == Some(0),
        Some(serde_json::Value::String(s)) => s == "DeviceToken is err",
        _ => false,
    };
    if ok {
        Ok(())
    } else {
        Err(format!("device error: {text}"))
    }
}

// ---------------------------------------------------------------- stats

/// Long-lived PowerShell sampler: localized counter names via Perflib,
/// CPU load + ASUS ATKACPI CPU temperature. Ported from the Node project's
/// stats.js (same script, same output format).
fn sampler_script(interval_ms: u64) -> String {
    let script = r##"
$ErrorActionPreference = 'SilentlyContinue'
function Read-CounterNames($key) {
  $arr = (Get-ItemProperty -Path $key -Name Counter).Counter
  $map = @{}
  for ($i = 0; $i -lt $arr.Count - 1; $i += 2) { $map[[int]$arr[$i]] = $arr[$i + 1] }
  return $map
}
$en = Read-CounterNames 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Perflib\009'
$cur = Read-CounterNames 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Perflib\CurrentLanguage'
if ($cur.Count -eq 0) { $cur = $en }
function Loc($english) {
  foreach ($k in $en.Keys) { if ($en[$k] -eq $english) { return $cur[$k] } }
  return $english
}
$objProcessor = Loc 'Processor Information'
$pctName = Loc '% Processor Time'
$paths = @("\$objProcessor(_Total)\$pctName")
try {
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class AtkAcpi {
  const uint CONTROL_CODE = 0x0022240C;
  const uint DSTS = 0x53545344;
  const uint TEMP_CPU = 0x00120094;
  [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
  static extern IntPtr CreateFile(string name, uint access, uint share, IntPtr sec, uint disp, uint flags, IntPtr tmpl);
  [DllImport("kernel32.dll", SetLastError = true)]
  static extern bool DeviceIoControl(IntPtr h, uint code, byte[] inBuf, int inSize, byte[] outBuf, int outSize, out int returned, IntPtr overlapped);
  [DllImport("kernel32.dll", SetLastError = true)]
  static extern bool CloseHandle(IntPtr h);
  static IntPtr handle = new IntPtr(-1);
  public static bool Open() {
    handle = CreateFile(@"\\.\ATKACPI", 0xC0000000, 3, IntPtr.Zero, 3, 0x80, IntPtr.Zero);
    return handle != new IntPtr(-1);
  }
  public static int ReadCpuTemp() {
    if (handle == new IntPtr(-1) && !Open()) return -1;
    byte[] buf = new byte[16];
    BitConverter.GetBytes(DSTS).CopyTo(buf, 0);
    BitConverter.GetBytes((uint)8).CopyTo(buf, 4);
    BitConverter.GetBytes(TEMP_CPU).CopyTo(buf, 8);
    byte[] outBuf = new byte[16];
    int ret;
    if (!DeviceIoControl(handle, CONTROL_CODE, buf, buf.Length, outBuf, outBuf.Length, out ret, IntPtr.Zero)) {
      CloseHandle(handle);
      handle = new IntPtr(-1);
      return -1;
    }
    return BitConverter.ToInt32(outBuf, 0) - 65536;
  }
}
'@
} catch { }
$atkReady = $false
try { $atkReady = [AtkAcpi]::Open() } catch { }
while ($true) {
  $rows = New-Object System.Collections.Generic.List[array]
  $sample = Get-Counter -Counter $paths -ErrorAction SilentlyContinue
  if ($sample) {
    foreach ($s in $sample.CounterSamples) {
      if ($s.Status -ne 0) { continue }
      $rows.Add(@($s.Path, $s.CookedValue))
    }
  }
  if ($atkReady) {
    $t = [AtkAcpi]::ReadCpuTemp()
    if ($t -ge 0 -and $t -le 130) { $rows.Add(@('atkacpi|cpu-temp', $t)) }
  }
  if ($rows.Count -gt 0) {
    [Console]::Out.WriteLine((ConvertTo-Json -Compress -Depth 4 -InputObject $rows))
    [Console]::Out.Flush()
  }
  Start-Sleep -Milliseconds __INTERVAL__
}
"##;
    script.replace("__INTERVAL__", &interval_ms.to_string())
}

fn strip_counter_prefix(path: &str) -> String {
    // "\\HOST\Processor Information(_Total)\% Processor Time"
    //   -> "Processor Information(_Total)\% Processor Time"
    if path.starts_with("\\\\") {
        if let Some(i) = path[2..].find('\\') {
            return path[2 + i + 1..].to_string();
        }
    }
    path.trim_start_matches('\\').to_string()
}

fn apply_sample(stats: &mut Stats, line: &str) {
    let v: serde_json::Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return,
    };
    let rows: Vec<serde_json::Value> = match v.as_array() {
        Some(a) => a.clone(),
        None => vec![v],
    };
    for row in rows {
        let arr = match row.as_array() {
            Some(a) if a.len() == 2 => a,
            _ => continue,
        };
        let path = match arr[0].as_str() {
            Some(p) => p.to_string(),
            None => continue,
        };
        let n = match arr[1].as_f64() {
            Some(n) => n,
            None => continue,
        };
        if path == "atkacpi|cpu-temp" {
            stats.cpu_temp = Some(n);
            continue;
        }
        let stripped = strip_counter_prefix(&path);
        let at = match stripped.rfind('\\') {
            Some(i) => i,
            None => continue,
        };
        let head = &stripped[..at];
        let counter = &stripped[at + 1..];
        let (obj, instance) = match (head.rfind('('), head.ends_with(')')) {
            (Some(p), true) => (&head[..p], &head[p + 1..head.len() - 1]),
            _ => (head, ""),
        };
        let key = format!("{}|{}", obj.to_lowercase(), counter.to_lowercase());
        if key == "processor information|% processor time"
            && instance.eq_ignore_ascii_case("_total")
        {
            stats.cpu_pct = Some(n.clamp(0.0, 100.0));
        }
    }
}

fn hide_console(cmd: &mut tokio::process::Command) {
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW (inherent tokio method)
    #[cfg(not(windows))]
    let _ = cmd;
}

fn spawn_sampler_task(
    stats: Arc<Mutex<Stats>>,
    running: Arc<AtomicBool>,
    interval_ms: u64,
) {
    tauri::async_runtime::spawn(async move {
        use tokio::io::{AsyncBufReadExt, BufReader};
        let mut cmd = tokio::process::Command::new("powershell.exe");
        cmd.args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &sampler_script(interval_ms),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
        hide_console(&mut cmd);
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(_) => return,
        };
        let stdout = child.stdout.take();
        let reader = stdout.map(|s| BufReader::new(s).lines());
        let mut reader = match reader {
            Some(r) => r,
            None => return,
        };
        loop {
            if !running.load(Ordering::SeqCst) {
                break;
            }
            match tokio::time::timeout(Duration::from_secs(10), reader.next_line()).await {
                Ok(Ok(Some(line))) => {
                    let t = line.trim_start();
                    if t.starts_with('[') || t.starts_with('{') {
                        apply_sample(&mut stats.lock().unwrap(), &line);
                    }
                }
                Ok(Ok(None)) => break, // sampler exited
                Ok(Err(_)) => break,
                Err(_) => {} // no sample this window, keep reading
            }
        }
        let _ = child.start_kill();
    });
}

async fn read_nvidia() -> Option<(f64, f64, f64, f64)> {
    // utilization.gpu, memory.used, memory.total (MiB), temperature.gpu
    let mut cmd = tokio::process::Command::new("nvidia-smi");
    cmd.args([
        "--query-gpu=utilization.gpu,memory.used,memory.total,temperature.gpu",
        "--format=csv,noheader,nounits",
    ])
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::null());
    hide_console(&mut cmd);
    let out = cmd.output().await.ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    let line = s.lines().next()?;
    let mut it = line.split(',').map(|x| x.trim().parse::<f64>().ok());
    let pct = it.next()??;
    let used = it.next()??;
    let total = it.next()??;
    let temp = it.next()??;
    Some((pct, used, total, temp))
}

async fn read_disk_temp() -> Option<f64> {
    // MSFT_StorageReliabilityMetrics is admin-only; unelevated this prints
    // nothing and the field shows N/A (best-effort by design).
    let ps = "Get-PhysicalDisk | Get-StorageReliabilityCounter -ErrorAction SilentlyContinue | Where-Object { $_.Temperature } | Select-Object -First 1 -ExpandProperty Temperature";
    let mut cmd = tokio::process::Command::new("powershell.exe");
    cmd.args(["-NoProfile", "-NonInteractive", "-Command", ps])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    hide_console(&mut cmd);
    let out = cmd.output().await.ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    s.parse().ok()
}

fn read_ram() -> (u64, u64) {
    let mut sys = sysinfo::System::new();
    sys.refresh_memory();
    (sys.used_memory(), sys.total_memory())
}

fn build_disp_data(s: &Stats, cfg: &Config, ram_used: u64, ram_total: u64) -> Vec<String> {
    let pct = |n: f64| format!("{}%", n.round());
    let deg = |n: f64| format!("{} C", n.round());
    vec![
        s.cpu_pct.map(pct).unwrap_or_else(|| "0%".into()),
        s.gpu_pct.map(pct).unwrap_or_else(|| "0%".into()),
        s.cpu_temp.map(deg).unwrap_or_else(|| "N/A".into()),
        s.gpu_temp.map(deg).unwrap_or_else(|| "N/A".into()),
        if ram_total > 0 {
            pct(ram_used as f64 / ram_total as f64 * 100.0)
        } else {
            "0%".into()
        },
        if cfg.last_field == "hdd" {
            s.disk_temp.map(deg).unwrap_or_else(|| "N/A".into())
        } else {
            match (s.vram_used, s.vram_total) {
                (Some(u), Some(t)) if t > 0.0 => pct(u / t * 100.0),
                _ => "0%".into(),
            }
        },
    ]
}

// ---------------------------------------------------------------- monitor loop

/// Cloud preset detection, same two unauthenticated calls as
/// divoom-times-gate-monitor: ReturnSameLANDevice → DeviceId by IP →
/// Get5LcdInfoV2 → first preset's LcdIndependence.
async fn detect_preset(client: &reqwest::Client, ip: &str) -> Option<(u64, u32)> {
    let r = client
        .post("https://app.divoom-gz.com/Device/ReturnSameLANDevice")
        .json(&json!({}))
        .send()
        .await
        .ok()?;
    let j: serde_json::Value = r.json().await.ok()?;
    let dev = j
        .get("DeviceList")?
        .as_array()?
        .iter()
        .find(|d| d.get("DevicePrivateIP").and_then(|v| v.as_str()) == Some(ip))?;
    let device_id = dev.get("DeviceId")?.as_u64()?;
    let r = client
        .get(format!(
            "https://app.divoom-gz.com/Channel/Get5LcdInfoV2?DeviceType=LCD&DeviceId={device_id}"
        ))
        .send()
        .await
        .ok()?;
    let j: serde_json::Value = r.json().await.ok()?;
    let preset = j
        .get("LcdIndependenceList")?
        .as_array()?
        .first()?
        .get("LcdIndependence")?
        .as_u64()? as u32;
    Some((device_id, preset))
}

async fn run_monitor(app: AppHandle, running: Arc<AtomicBool>, cfg: Config) {
    let _ = app.emit(
        "monitor-state",
        StatePayload { running: true },
    );
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let url = endpoint(&cfg);

    // Activate the PC Monitor widget, with retries.
    //
    // Measured difference vs divoom-times-gate-monitor (the reference app):
    // it sends SetClockSelectId *with LcdIndependence* (the device's preset
    // id, taken from the cloud Get5LcdInfoV2) and never sends
    // Set5LcdChannelType. With the preset id the device changes only the
    // target screen inside the current preset. A bare Set5LcdChannelType
    // ChannelType 1 (no LcdIndependence) switches to independent-dial mode
    // but re-initializes the other screens with default faces — the side
    // effect observed on this device.
    //
    // So: preset id from settings if configured, else detect it via the
    // cloud (same calls the reference app makes), else fall back to the
    // channel-type switch.
    let preset = if cfg.preset_id != 0 {
        Some((0u64, cfg.preset_id))
    } else {
        detect_preset(&client, &cfg.ip).await
    };

    let mut activated = false;
    for attempt in 1..=3u8 {
        let mut sel = json!({
            "Command": "Channel/SetClockSelectId",
            "ClockId": PC_MONITOR_CLOCK_ID,
            "LcdIndex": cfg.lcd_index,
        });
        let result = match preset {
            Some((device_id, p)) => {
                sel["LcdIndependence"] = json!(p);
                if device_id != 0 {
                    sel["DeviceId"] = json!(device_id);
                }
                send_cmd(&client, &url, sel).await
            }
            None => {
                let ch = json!({ "Command": "Channel/Set5LcdChannelType", "ChannelType": 1 });
                match send_cmd(&client, &url, ch).await {
                    Ok(()) => send_cmd(&client, &url, sel).await,
                    Err(e) => Err(e),
                }
            }
        };
        match result {
            Ok(()) => {
                activated = true;
                let how = match preset {
                    Some((_, p)) => format!("preset {p}"),
                    None => "channel-mode switch".to_string(),
                };
                eprintln!("activation: {how}");
                let _ = app.emit(
                    "monitor-tick",
                    TickPayload {
                        line: format!("widget activated ({how})"),
                        ok: true,
                        error: None,
                    },
                );
                break;
            }
            Err(e) => {
                let _ = app.emit(
                    "monitor-tick",
                    TickPayload {
                        line: format!("activation attempt {attempt}/3"),
                        ok: false,
                        error: Some(e),
                    },
                );
                if attempt < 3 {
                    tokio::time::sleep(Duration::from_secs(3)).await;
                }
            }
        }
    }
    if !activated {
        running.store(false, Ordering::SeqCst);
        let _ = app.emit("monitor-state", StatePayload { running: false });
        return;
    }

    let stats = Arc::new(Mutex::new(Stats::default()));
    spawn_sampler_task(stats.clone(), running.clone(), cfg.interval_ms);

    let interval = Duration::from_millis(cfg.interval_ms.max(1000));
    let mut last_disk = std::time::Instant::now() - Duration::from_secs(40);

    while running.load(Ordering::SeqCst) {
        let tick_start = std::time::Instant::now();

        if let Some((pct, used, total, temp)) = read_nvidia().await {
            let mut s = stats.lock().unwrap();
            s.gpu_pct = Some(pct);
            s.vram_used = Some(used);
            s.vram_total = Some(total);
            s.gpu_temp = Some(temp);
        }
        if last_disk.elapsed() >= Duration::from_secs(30) {
            let t = read_disk_temp().await;
            stats.lock().unwrap().disk_temp = t;
            last_disk = std::time::Instant::now();
        }
        let (ram_used, ram_total) = read_ram();

        let disp = build_disp_data(&stats.lock().unwrap().clone(), &cfg, ram_used, ram_total);
        let line = disp.join(" | ");
        let payload = json!({
            "Command": "Device/UpdatePCParaInfo",
            "ScreenList": [{ "LcdId": cfg.lcd_index, "DispData": disp }],
        });
        let result = send_cmd(&client, &url, payload).await;
        eprintln!("tick: {line}");
        match result {
            Ok(()) => {
                let _ = app.emit(
                    "monitor-tick",
                    TickPayload { line, ok: true, error: None },
                );
            }
            Err(e) => {
                let _ = app.emit(
                    "monitor-tick",
                    TickPayload { line, ok: false, error: Some(e) },
                );
            }
        }

        // Sleep out the interval in small chunks so Stop is responsive.
        let deadline = tokio::time::Instant::now() + interval.saturating_sub(tick_start.elapsed());
        while running.load(Ordering::SeqCst) && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    running.store(false, Ordering::SeqCst);
    let _ = app.emit("monitor-state", StatePayload { running: false });
}

fn do_start(app: &AppHandle, state: &AppState, cfg: Config) -> Result<(), String> {
    if state.running.swap(true, Ordering::SeqCst) {
        return Err("monitor already running".into());
    }
    *state.last_config.lock().unwrap() = Some(cfg.clone());
    let app2 = app.clone();
    let running = state.running.clone();
    tauri::async_runtime::spawn(async move {
        run_monitor(app2, running, cfg).await;
    });
    Ok(())
}

// ---------------------------------------------------------------- commands

#[tauri::command]
async fn start_monitor(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    config: Config,
) -> Result<(), String> {
    do_start(&app, &state, config)
}

#[tauri::command]
fn stop_monitor(state: tauri::State<'_, AppState>) {
    state.running.store(false, Ordering::SeqCst);
}

#[tauri::command]
fn get_monitor_state(state: tauri::State<'_, AppState>) -> StatePayload {
    StatePayload {
        running: state.running.load(Ordering::SeqCst),
    }
}

#[tauri::command]
fn set_autostart(app: AppHandle, enabled: bool) -> Result<(), String> {
    use tauri_plugin_autostart::ManagerExt;
    let m = app.autolaunch();
    if enabled {
        m.enable().map_err(|e| e.to_string())
    } else {
        m.disable().map_err(|e| e.to_string())
    }
}

#[tauri::command]
fn get_autostart(app: AppHandle) -> bool {
    use tauri_plugin_autostart::ManagerExt;
    app.autolaunch().is_enabled().unwrap_or(false)
}

#[tauri::command]
async fn quit_app(app: AppHandle, state: tauri::State<'_, AppState>) -> Result<(), String> {
    // Let the monitor task kill its PowerShell sampler before we exit,
    // so no orphaned sampler process is left behind.
    state.running.store(false, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(600)).await;
    app.exit(0);
    Ok(())
}

// Settings live in a JSON file in the app data dir (not localStorage) so
// the Rust startup path can honor "start minimized" and "auto-start
// monitor" before the webview loads, and so autostart needs no CLI args.

fn read_settings(app: &AppHandle) -> serde_json::Value {
    let dir = match app.path().app_data_dir() {
        Ok(d) => d,
        Err(_) => return json!({}),
    };
    std::fs::read_to_string(dir.join("settings.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}))
}

#[tauri::command]
fn load_settings(app: AppHandle) -> serde_json::Value {
    read_settings(&app)
}

#[tauri::command]
fn save_settings(app: AppHandle, settings: serde_json::Value) -> Result<(), String> {
    let dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let s = serde_json::to_string_pretty(&settings).map_err(|e| e.to_string())?;
    std::fs::write(dir.join("settings.json"), s).map_err(|e| e.to_string())?;
    Ok(())
}

// ---------------------------------------------------------------- scan

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CloudDevice {
    name: String,
    ip: String,
    hardware: u32,
    device_id: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LocalHit {
    ip: String,
    // 400 when :80/post answered, 402 when :9000/divoom_api answered.
    hardware: u32,
    ports: Vec<u16>,
    // true when the reply looked like the Divoom server itself.
    divoom: bool,
}

/// Cloud device list — same unauthenticated call as detect_preset.
async fn cloud_devices(client: &reqwest::Client) -> Result<Vec<CloudDevice>, String> {
    let r = client
        .post("https://app.divoom-gz.com/Device/ReturnSameLANDevice")
        .json(&json!({}))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let j: serde_json::Value = r.json().await.map_err(|e| e.to_string())?;
    let list = j
        .get("DeviceList")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "cloud response carried no DeviceList".to_string())?;
    Ok(list
        .iter()
        .filter_map(|d| {
            Some(CloudDevice {
                name: d
                    .get("DeviceName")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?")
                    .to_string(),
                ip: d.get("DevicePrivateIP")?.as_str()?.to_string(),
                hardware: d.get("Hardware").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                device_id: d.get("DeviceId").and_then(|v| v.as_u64()).unwrap_or(0),
            })
        })
        .collect())
}

fn is_virtual_alias(alias: &str) -> bool {
    let a = alias.to_lowercase();
    [
        "twingate",
        "tailscale",
        "wireguard",
        "openvpn",
        "vpn",
        "zscaler",
        "anyconnect",
        "citrix",
        "globalprotect",
        "wintun",
        "tap",
        "utun",
        "hyper-v",
        "veth",
        "docker",
        "wsl",
        "virtualbox",
        "vmware",
        "loopback",
        "loop",
        "bluetooth",
    ]
    .iter()
    .any(|k| a.contains(k))
}

/// Most likely physical LAN adapter: Windows' own route preference order
/// (InterfaceMetric), minus well-known/APIPA addresses and VPN/virtual adapters.
async fn primary_lan_ipv4() -> Option<String> {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let script = "Get-NetIPAddress -AddressFamily IPv4 | Where-Object { $_.PrefixOrigin -ne 'WellKnown' -and $_.IPAddress -notlike '169.254.*' } | Sort-Object InterfaceMetric | Select-Object -First 10 IPAddress,InterfaceAlias,InterfaceMetric | ConvertTo-Json -Compress";
    let out = tokio::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(text.trim()).ok()?;
    let rows: Vec<&serde_json::Value> = match v.as_array() {
        Some(a) => a.iter().collect(),
        None => vec![&v],
    };
    rows.iter().find_map(|row| {
        let alias = row
            .get("InterfaceAlias")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let ip = row.get("IPAddress").and_then(|v| v.as_str())?;
        (!is_virtual_alias(alias)).then(|| ip.to_string())
    })
}

/// Fallback: ask the OS which source address it would use for a public route.
fn primary_lan_ipv4_route_guess() -> Option<String> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("8.8.8.8:80").ok()?;
    Some(s.local_addr().ok()?.ip().to_string())
}

fn cloud_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap_or_default()
}

fn probe_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(Duration::from_millis(1500))
        .connect_timeout(Duration::from_millis(450))
        .build()
        .map_err(|e| e.to_string())
}

/// Hit every host in the local /24 on both Divoom endpoints; anything that
/// answers HTTP is a candidate Times Gate.
async fn local_scan(app: &AppHandle, client: &reqwest::Client) -> Result<Vec<LocalHit>, String> {
    let my_ip = primary_lan_ipv4()
        .await
        .or_else(primary_lan_ipv4_route_guess)
        .ok_or("no usable local IPv4 address found")?;
    let octets: Vec<u8> = my_ip.split('.').filter_map(|s| s.parse().ok()).collect();
    if octets.len() != 4 {
        return Err(format!("unexpected local address {my_ip}"));
    }
    let base = format!("{}.{}.{}", octets[0], octets[1], octets[2]);

    let mut set = tokio::task::JoinSet::new();
    for last in 1u8..=254u8 {
        let ip = format!("{base}.{last}");
        if ip == my_ip {
            continue;
        }
        for (port, path) in [(80u16, "/post"), (9000u16, "/divoom_api")] {
            let client = client.clone();
            let ip2 = ip.clone();
            set.spawn(async move {
                match client
                    .post(format!("http://{ip2}:{port}{path}"))
                    .json(&json!({}))
                    .send()
                    .await
                {
                    Ok(r) => {
                        let body = r.text().await.unwrap_or_default();
                        let divoom = body.contains("Request method for this URI is not handled by server")
                            || body.contains("DeviceToken")
                            || body.contains("Request data illegal");
                        Some((ip2, port, divoom))
                    }
                    Err(_) => None,
                }
            });
        }
    }

    let mut hits: Vec<LocalHit> = Vec::new();
    let mut done = 0u32;
    while let Some(res) = set.join_next().await {
        done += 1;
        if done % 25 == 0 {
            let _ = app.emit_to("main", "scan-progress", json!({ "done": done }));
        }
        if let Ok(Some((ip, port, divoom))) = res {
            match hits.iter_mut().find(|h| h.ip == ip) {
                Some(h) => {
                    if !h.ports.contains(&port) {
                        h.ports.push(port);
                    }
                    h.divoom |= divoom;
                }
                None => hits.push(LocalHit {
                    ip,
                    hardware: if port == 9000 { 402 } else { 400 },
                    ports: vec![port],
                    divoom,
                }),
            }
        }
    }
    hits.sort_by(|a, b| {
        b.divoom.cmp(&a.divoom).then_with(|| {
            let ka: Vec<u32> = a.ip.split('.').filter_map(|s| s.parse().ok()).collect();
            let kb: Vec<u32> = b.ip.split('.').filter_map(|s| s.parse().ok()).collect();
            ka.cmp(&kb)
        })
    });
    Ok(hits)
}

#[tauri::command]
async fn scan_cloud() -> Result<Vec<CloudDevice>, String> {
    cloud_devices(&cloud_client()).await
}

#[tauri::command]
async fn scan_local(app: AppHandle) -> Result<Vec<LocalHit>, String> {
    let client = probe_client()?;
    local_scan(&app, &client).await
}

// ---------------------------------------------------------------- app

/// One-shot cloud query so the UI can show the detected preset id
/// ("Current ID = …") without filling the input box.
#[tauri::command]
async fn get_cloud_preset(ip: String) -> Option<u32> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .ok()?;
    detect_preset(&client, &ip).await.map(|(_, p)| p)
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.unminimize();
                let _ = w.show();
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .manage(AppState {
            running: Arc::new(AtomicBool::new(false)),
            last_config: Mutex::new(None),
        })
        .invoke_handler(tauri::generate_handler![
            start_monitor,
            stop_monitor,
            get_monitor_state,
            set_autostart,
            get_autostart,
            quit_app,
            load_settings,
            save_settings,
            get_cloud_preset,
            scan_cloud,
            scan_local
        ])
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                // Close button minimizes to the tray; Exit quits.
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .setup(|app| {
            let handle = app.handle().clone();

            let show = MenuItemBuilder::with_id("show", "Show window").build(app)?;
            let start = MenuItemBuilder::with_id("start", "Start monitor").build(app)?;
            let stop = MenuItemBuilder::with_id("stop", "Stop monitor").build(app)?;
            let quit = MenuItemBuilder::with_id("quit", "Exit").build(app)?;
            let menu = MenuBuilder::new(app)
                .item(&show)
                .item(&start)
                .item(&stop)
                .separator()
                .item(&quit)
                .build()?;

            let _tray = TrayIconBuilder::with_id("mztgm2-tray")
                .icon(Image::from_bytes(include_bytes!("../icons/icon.png")).unwrap())
                .tooltip("MZ TGM 2 — Times Gate PC Monitor")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id().as_ref() {
                    "show" => {
                        if let Some(w) = app.get_webview_window("main") {
                            let _ = w.show();
                            let _ = w.set_focus();
                        }
                    }
                    "start" => {
                        let state = app.state::<AppState>();
                        let cfg = state.last_config.lock().unwrap().clone();
                        match cfg {
                            Some(c) => {
                                let _ = do_start(app, &state, c);
                            }
                            None => {
                                if let Some(w) = app.get_webview_window("main") {
                                    let _ = w.show();
                                    let _ = w.set_focus();
                                }
                            }
                        }
                    }
                    "stop" => {
                        app.state::<AppState>()
                            .running
                            .store(false, Ordering::SeqCst);
                    }
                    "quit" => {
                        let a = app.clone();
                        a.state::<AppState>().running.store(false, Ordering::SeqCst);
                        tauri::async_runtime::spawn(async move {
                            tokio::time::sleep(Duration::from_millis(600)).await;
                            a.exit(0);
                        });
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        let app = tray.app_handle();
                        if let Some(w) = app.get_webview_window("main") {
                            if w.is_visible().unwrap_or(false) {
                                let _ = w.hide();
                            } else {
                                let _ = w.show();
                                let _ = w.set_focus();
                            }
                        }
                    }
                })
                .build(app)?;

            let args: Vec<String> = std::env::args().collect();

            // Settings file decides startup behavior (autostart needs no
            // CLI args: the registry entry launches plain, the app reads
            // its own settings).
            let settings = read_settings(&handle);
            let minimized = settings
                .get("startMinimized")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
                || args.iter().any(|a| a == "--minimized");
            if minimized {
                if let Some(w) = app.get_webview_window("main") {
                    let _ = w.hide();
                }
            }
            if settings
                .get("autoStartMonitor")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                let cfg = Config {
                    ip: settings
                        .get("ip")
                        .and_then(|v| v.as_str())
                        .unwrap_or("192.168.0.100")
                        .to_string(),
                    hardware: settings
                        .get("hardware")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(400) as u32,
                    interval_ms: settings
                        .get("intervalMs")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(2000),
                    last_field: settings
                        .get("lastField")
                        .and_then(|v| v.as_str())
                        .unwrap_or("vram")
                        .to_string(),
                    lcd_index: settings
                        .get("lcdIndex")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0)
                        .min(4) as u32,
                    preset_id: settings
                        .get("presetId")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as u32,
                };
                let state = app.state::<AppState>();
                let _ = do_start(&handle, &state, cfg);
            }

            // --selftest <ip> <hardware> <intervalSec> <vram|hdd> [display 1-5] [presetId]:
            // headless run for 8 s, tick lines on stderr, then exit.
            if let Some(pos) = args.iter().position(|a| a == "--selftest") {
                if let Some(w) = app.get_webview_window("main") {
                    let _ = w.hide();
                }
                let cfg = Config {
                    ip: args.get(pos + 1).cloned().unwrap_or_else(|| "192.168.0.100".into()),
                    hardware: args
                        .get(pos + 2)
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(400),
                    interval_ms: args
                        .get(pos + 3)
                        .and_then(|s| s.parse::<u64>().ok())
                        .unwrap_or(2)
                        * 1000,
                    last_field: args
                        .get(pos + 4)
                        .cloned()
                        .unwrap_or_else(|| "vram".into()),
                    lcd_index: args
                        .get(pos + 5)
                        .and_then(|s| s.parse::<u32>().ok())
                        .unwrap_or(1)
                        .saturating_sub(1)
                        .min(4),
                    preset_id: args
                        .get(pos + 6)
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0),
                };
                let state = app.state::<AppState>();
                let _ = do_start(&handle, &state, cfg);
                let a = handle.clone();
                let running = state.running.clone();
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(8)).await;
                    running.store(false, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(600)).await;
                    a.exit(0);
                });
            }

            // --scan-cloud / --scan-local: print scan results to stderr, then exit.
            if args.iter().any(|a| a == "--scan-cloud" || a == "--scan-local") {
                if let Some(w) = app.get_webview_window("main") {
                    let _ = w.hide();
                }
                let cloud = args.iter().any(|a| a == "--scan-cloud");
                let a = handle.clone();
                tauri::async_runtime::spawn(async move {
                    let out = if cloud {
                        cloud_devices(&cloud_client()).await.map(|v| serde_json::to_string_pretty(&v).unwrap())
                    } else {
                        match probe_client() {
                            Err(e) => Err(e),
                            Ok(client) => local_scan(&a, &client)
                                .await
                                .map(|v| serde_json::to_string_pretty(&v).unwrap()),
                        }
                    };
                    match out {
                        Ok(s) => eprintln!("{s}"),
                        Err(e) => eprintln!("scan failed: {e}"),
                    }
                    a.exit(0);
                });
            }

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running MZ TGM 2");
}
