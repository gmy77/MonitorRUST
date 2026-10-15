#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! MonitorRUST: pannello compatto per i sensori letti da LibreHardwareMonitor.
//! I dati arrivano dal web server integrato di LHM (`/data.json`); un thread li
//! legge a intervalli regolari e l'interfaccia mostra solo i sensori scelti.

use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, Vec2};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Chiave della scheda composta "CPU" (più calda, più fredda e media dei core).
const CPU_AGG: &str = "cpu-aggregato";
/// Campioni tenuti per il mini grafico (a 1 s per campione sono 2 minuti).
const HISTORY_LEN: usize = 120;
const TILE_H: f32 = 150.0;
const TILE_MIN_W: f32 = 270.0;
const GAP: f32 = 10.0;

const GREEN: Color32 = Color32::from_rgb(46, 160, 67);
const AMBER: Color32 = Color32::from_rgb(222, 158, 36);
const RED: Color32 = Color32::from_rgb(222, 64, 64);
const ACCENT: Color32 = Color32::from_rgb(64, 140, 230);

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Monitor Sensori")
            .with_inner_size([600.0, 720.0])
            .with_min_inner_size([300.0, 260.0])
            .with_icon(make_icon()),
        ..Default::default()
    };
    eframe::run_native("MonitorRUST", options, Box::new(|cc| Ok(Box::new(App::new(cc)))))
}

// ---------------------------------------------------------------- dati

#[derive(Clone, Debug)]
struct Sensor {
    id: String,
    hardware: String,
    name: String,
    kind: String,
    value: Option<f32>,
    min: Option<f32>,
    max: Option<f32>,
    unit: String,
}

/// Stato condiviso tra il thread di lettura e l'interfaccia.
struct Shared {
    url: String,
    interval_ms: u64,
    sensors: Vec<Sensor>,
    error: Option<String>,
    seq: u64,
    /// Esito dell'avvio automatico di LibreHardwareMonitor, mostrato finché non arrivano dati.
    lhm_status: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq)]
enum Theme {
    Sistema,
    Chiaro,
    Scuro,
}

impl Theme {
    fn label(self) -> &'static str {
        match self {
            Theme::Sistema => "Tema di sistema",
            Theme::Chiaro => "Tema chiaro",
            Theme::Scuro => "Tema scuro",
        }
    }
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
struct Settings {
    url: String,
    interval_s: f32,
    selected: Vec<String>,
    initialized: bool,
    warn_c: f32,
    crit_c: f32,
    theme: Theme,
    always_on_top: bool,
    /// Nomi scelti dall'utente per le schede (chiave → nome).
    names: HashMap<String, String>,
    /// Core esclusi dalla scheda CPU composta (SensorId).
    cpu_excluded: Vec<String>,
    /// Avvia LibreHardwareMonitor all'apertura, se non è già in esecuzione.
    lhm_autostart: bool,
    lhm_path: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            url: "http://localhost:8085/data.json".into(),
            interval_s: 1.0,
            selected: Vec::new(),
            initialized: false,
            warn_c: 75.0,
            crit_c: 85.0,
            theme: Theme::Scuro,
            always_on_top: false,
            names: HashMap::new(),
            cpu_excluded: Vec::new(),
            lhm_autostart: true,
            lhm_path: default_lhm_path(),
        }
    }
}

/// Percorso predefinito di LHM: cartella `LibreHardwareMonitor` accanto a quella
/// di MonitorRUST (es. `Tools\MonitorRUST` e `Tools\LibreHardwareMonitor`),
/// oppure nella stessa cartella dell'exe.
fn default_lhm_path() -> String {
    const EXE: &str = "LibreHardwareMonitor.exe";
    let Some(dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf())) else {
        return EXE.into();
    };
    let candidates = [
        dir.parent().map(|p| p.join("LibreHardwareMonitor").join(EXE)),
        Some(dir.join("LibreHardwareMonitor").join(EXE)),
        Some(dir.join(EXE)),
    ];
    candidates
        .iter()
        .flatten()
        .find(|p| p.is_file())
        .or(candidates[0].as_ref())
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| EXE.into())
}

/// Valori pronti da disegnare in una scheda (sensore singolo o composto).
struct Reading {
    title: String,
    subtitle: String,
    kind: String,
    unit: String,
    value: Option<f32>,
    min: Option<f32>,
    max: Option<f32>,
    detail: Option<String>,
}

// ---------------------------------------------------------------- lettura HTTP

fn start_poller(shared: Arc<Mutex<Shared>>, ctx: egui::Context) {
    std::thread::spawn(move || loop {
        let (url, interval) = {
            let s = shared.lock().unwrap();
            (s.url.clone(), s.interval_ms)
        };
        let result = fetch(&url).and_then(|body| parse_sensors(&body));
        {
            let mut s = shared.lock().unwrap();
            match result {
                Ok(list) => {
                    s.sensors = list;
                    s.error = None;
                    s.lhm_status = None;
                }
                Err(e) => {
                    // Valori vecchi sarebbero fuorvianti: meglio mostrare "—".
                    s.sensors.clear();
                    s.error = Some(e);
                }
            }
            s.seq += 1;
        }
        ctx.request_repaint();
        std::thread::sleep(Duration::from_millis(interval.max(250)));
    });
}

/// Avvia LibreHardwareMonitor se non è già aperto (in un thread: `tasklist` è lento).
/// LHM richiede i permessi di amministratore: `cmd /C start` passa da ShellExecute,
/// che mostra la richiesta UAC; un avvio diretto fallirebbe con l'errore 740.
fn start_lhm(shared: Arc<Mutex<Shared>>, path: String, force: bool) {
    std::thread::spawn(move || {
        let status = if !force && lhm_running() {
            None
        } else if !std::path::Path::new(&path).is_file() {
            Some(format!("LibreHardwareMonitor non trovato in {path}"))
        } else {
            let mut cmd = std::process::Command::new("cmd");
            cmd.args(["/C", "start", "", &path]);
            if let Some(dir) = std::path::Path::new(&path).parent() {
                cmd.current_dir(dir);
            }
            hide_console(&mut cmd);
            match cmd.status() {
                Ok(_) => Some("Avvio di LibreHardwareMonitor… conferma la richiesta di Windows.".into()),
                Err(e) => Some(format!("Avvio di LibreHardwareMonitor non riuscito: {e}")),
            }
        };
        shared.lock().unwrap().lhm_status = status;
    });
}

fn lhm_running() -> bool {
    let mut cmd = std::process::Command::new("tasklist");
    cmd.args(["/FI", "IMAGENAME eq LibreHardwareMonitor.exe", "/NH", "/FO", "CSV"]);
    hide_console(&mut cmd);
    cmd.output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_ascii_lowercase().contains("librehardwaremonitor.exe"))
        .unwrap_or(false)
}

/// Evita il lampo della finestra del prompt quando si lanciano comandi di sistema.
fn hide_console(cmd: &mut std::process::Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
}

/// GET minimale in HTTP/1.0 (solo localhost/LAN, niente TLS): evita dipendenze pesanti.
fn fetch(url: &str) -> Result<String, String> {
    let rest = url.strip_prefix("http://").ok_or("l'indirizzo deve iniziare con http://")?;
    let (hostport, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let target = if hostport.contains(':') { hostport.to_string() } else { format!("{hostport}:80") };
    let addrs: Vec<_> = target
        .to_socket_addrs()
        .map_err(|e| format!("indirizzo non valido: {e}"))?
        .collect();
    let mut last = String::from("nessun indirizzo");
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, Duration::from_secs(2)) {
            Ok(mut stream) => {
                stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
                let req = format!(
                    "GET {path} HTTP/1.0\r\nHost: {hostport}\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
                );
                stream.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
                let mut buf = Vec::new();
                stream.read_to_end(&mut buf).map_err(|e| e.to_string())?;
                return decode_response(&buf);
            }
            Err(e) => last = e.to_string(),
        }
    }
    Err(format!("connessione non riuscita: {last}"))
}

fn decode_response(buf: &[u8]) -> Result<String, String> {
    let split = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("risposta HTTP incompleta")?;
    let head = String::from_utf8_lossy(&buf[..split]).to_ascii_lowercase();
    let status = head.lines().next().unwrap_or("").to_string();
    if !status.contains(" 200") {
        return Err(format!("risposta inattesa: {status}"));
    }
    let body = &buf[split + 4..];
    let chunked = head
        .lines()
        .any(|l| l.starts_with("transfer-encoding:") && l.contains("chunked"));
    let body = if chunked { dechunk(body)? } else { body.to_vec() };
    String::from_utf8(body).map_err(|_| "risposta non in UTF-8".to_string())
}

fn dechunk(mut b: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    loop {
        let eol = b.windows(2).position(|w| w == b"\r\n").ok_or("chunk non valido")?;
        let size_txt = String::from_utf8_lossy(&b[..eol]);
        let size = usize::from_str_radix(size_txt.split(';').next().unwrap_or("").trim(), 16)
            .map_err(|_| "chunk non valido")?;
        b = &b[eol + 2..];
        if size == 0 {
            return Ok(out);
        }
        if b.len() < size {
            return Err("chunk troncato".into());
        }
        out.extend_from_slice(&b[..size]);
        b = b.get(size + 2..).unwrap_or(&[]);
    }
}

fn parse_sensors(body: &str) -> Result<Vec<Sensor>, String> {
    let root: serde_json::Value = serde_json::from_str(body.trim_start_matches('\u{feff}'))
        .map_err(|e| format!("JSON non valido: {e}"))?;
    let mut out = Vec::new();
    walk(&root, "", &mut out);
    if out.is_empty() {
        return Err("nessun sensore nel JSON".into());
    }
    Ok(out)
}

/// Visita l'albero di LHM: i nodi con `HardwareId` danno il nome del dispositivo,
/// quelli con `SensorId` sono i sensori veri e propri.
fn walk(node: &serde_json::Value, hardware: &str, out: &mut Vec<Sensor>) {
    let text = node.get("Text").and_then(|v| v.as_str()).unwrap_or("");
    let hardware = if node.get("HardwareId").is_some() { text } else { hardware };
    if let Some(id) = node.get("SensorId").and_then(|v| v.as_str()) {
        let field = |k: &str| node.get(k).and_then(|v| v.as_str()).unwrap_or("");
        // I valori "Raw" hanno sempre la stessa unità; quelli normali possono cambiarla (KB/s → MB/s).
        let pick = |raw: &str, plain: &str| if field(raw).is_empty() { field(plain) } else { field(raw) };
        let (value, unit) = parse_value(pick("RawValue", "Value"));
        out.push(Sensor {
            id: id.to_string(),
            hardware: hardware.to_string(),
            name: text.to_string(),
            kind: field("Type").to_string(),
            value,
            min: parse_value(pick("RawMin", "Min")).0,
            max: parse_value(pick("RawMax", "Max")).0,
            unit,
        });
    }
    if let Some(children) = node.get("Children").and_then(|v| v.as_array()) {
        for child in children {
            walk(child, hardware, out);
        }
    }
}

/// "74,0 °C" → (74.0, "°C"). LHM formatta con le impostazioni locali (virgola decimale).
fn parse_value(s: &str) -> (Option<f32>, String) {
    let s = s.trim();
    let end = s
        .find(|c: char| !(c.is_ascii_digit() || c == ',' || c == '.' || c == '-'))
        .unwrap_or(s.len());
    let number = s[..end].replace(',', ".");
    (number.parse::<f32>().ok(), s[end..].trim().to_string())
}

// ---------------------------------------------------------------- selezione sensori

fn cpu_core_temps(sensors: &[Sensor]) -> Vec<&Sensor> {
    let is_cpu_temp = |s: &&Sensor| {
        (s.id.starts_with("/intelcpu") || s.id.starts_with("/amdcpu"))
            && s.kind == "Temperature"
            && !s.name.contains("Distance")
    };
    let cores: Vec<&Sensor> = sensors
        .iter()
        .filter(is_cpu_temp)
        .filter(|s| s.name.starts_with("CPU Core #") || s.name.starts_with("Core #"))
        .collect();
    if !cores.is_empty() {
        return cores;
    }
    sensors.iter().filter(is_cpu_temp).collect()
}

/// Scelta iniziale: CPU composta, temperatura GPU, temperature scheda madre, ventole.
fn default_selection(sensors: &[Sensor]) -> Vec<String> {
    let mut sel = Vec::new();
    if !cpu_core_temps(sensors).is_empty() {
        sel.push(CPU_AGG.to_string());
    }
    let gpu: Vec<&Sensor> = sensors
        .iter()
        .filter(|s| s.id.starts_with("/gpu") && s.kind == "Temperature")
        .collect();
    if let Some(core) = gpu.iter().find(|s| s.name.contains("Core")).or(gpu.first()) {
        sel.push(core.id.clone());
    }
    sel.extend(
        sensors
            .iter()
            .filter(|s| s.id.starts_with("/lpc") && s.kind == "Temperature")
            .map(|s| s.id.clone()),
    );
    sel.extend(sensors.iter().filter(|s| s.kind == "Fan").map(|s| s.id.clone()));
    sel
}

// ---------------------------------------------------------------- formattazione

fn decimals(kind: &str) -> usize {
    match kind {
        "Fan" | "Clock" | "Flow" => 0,
        "Voltage" => 3,
        _ => 1,
    }
}

/// Numero all'italiana: virgola decimale.
fn fmt_num(v: f32, kind: &str) -> String {
    format!("{:.*}", decimals(kind), v).replace('.', ",")
}

fn fmt_value(v: Option<f32>, unit: &str, kind: &str) -> String {
    match v {
        Some(v) if unit.is_empty() => fmt_num(v, kind),
        Some(v) => format!("{} {unit}", fmt_num(v, kind)),
        None => "—".into(),
    }
}

/// Intervallo della barra: fisso per temperature e percentuali, adattivo per il resto.
fn scale(r: &Reading) -> (f32, f32) {
    let v = r.value.unwrap_or(0.0);
    let lo = r.min.unwrap_or(v).min(v);
    let hi = r.max.unwrap_or(v).max(v);
    match r.kind.as_str() {
        "Temperature" => (lo.min(20.0).floor(), hi.max(100.0).ceil()),
        "Load" | "Level" | "Control" => (0.0, 100.0),
        "Fan" => (0.0, ((hi * 1.15) / 500.0).ceil().max(1.0) * 500.0),
        _ => {
            let span = (hi - lo).abs().max(hi.abs() * 0.1).max(0.1);
            (lo - span * 0.25, hi + span * 0.25)
        }
    }
}

// ---------------------------------------------------------------- applicazione

struct App {
    shared: Arc<Mutex<Shared>>,
    settings: Settings,
    sensors: Vec<Sensor>,
    error: Option<String>,
    last_seq: u64,
    history: HashMap<String, VecDeque<f32>>,
    show_picker: bool,
    url_edit: String,
    applied_on_top: Option<bool>,
    applied_theme: Option<Theme>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let settings: Settings = cc
            .storage
            .and_then(|s| eframe::get_value(s, eframe::APP_KEY))
            .unwrap_or_default();
        let shared = Arc::new(Mutex::new(Shared {
            url: settings.url.clone(),
            interval_ms: (settings.interval_s * 1000.0) as u64,
            sensors: Vec::new(),
            error: None,
            seq: 0,
            lhm_status: None,
        }));
        if settings.lhm_autostart {
            start_lhm(shared.clone(), settings.lhm_path.clone(), false);
        }
        start_poller(shared.clone(), cc.egui_ctx.clone());
        Self {
            url_edit: settings.url.clone(),
            show_picker: !settings.initialized,
            shared,
            settings,
            sensors: Vec::new(),
            error: None,
            last_seq: 0,
            history: HashMap::new(),
            applied_on_top: None,
            applied_theme: None,
        }
    }

    /// Prende l'ultima lettura dal thread e aggiorna lo storico dei sensori mostrati.
    fn sync(&mut self) {
        let (seq, sensors, error) = {
            let s = self.shared.lock().unwrap();
            (s.seq, s.sensors.clone(), s.error.clone())
        };
        if seq == self.last_seq {
            return;
        }
        self.last_seq = seq;
        self.sensors = sensors;
        self.error = error;
        if !self.settings.initialized && !self.sensors.is_empty() {
            self.settings.selected = default_selection(&self.sensors);
            self.settings.initialized = true;
        }
        let values: Vec<(String, Option<f32>)> = self
            .settings
            .selected
            .iter()
            .map(|k| (k.clone(), self.reading(k).and_then(|r| r.value)))
            .collect();
        for (key, value) in values {
            if let Some(v) = value {
                let h = self.history.entry(key).or_default();
                h.push_back(v);
                while h.len() > HISTORY_LEN {
                    h.pop_front();
                }
            }
        }
    }

    fn reading(&self, key: &str) -> Option<Reading> {
        let custom = self.settings.names.get(key).filter(|n| !n.trim().is_empty());
        if key == CPU_AGG {
            let all = cpu_core_temps(&self.sensors);
            let total = all.len();
            let cores: Vec<&Sensor> = all
                .into_iter()
                .filter(|s| !self.settings.cpu_excluded.contains(&s.id))
                .collect();
            let values: Vec<f32> = cores.iter().filter_map(|s| s.value).collect();
            if values.is_empty() {
                return None;
            }
            let hottest = values.iter().cloned().fold(f32::MIN, f32::max);
            let coldest = values.iter().cloned().fold(f32::MAX, f32::min);
            let avg = values.iter().sum::<f32>() / values.len() as f32;
            let min = cores.iter().filter_map(|s| s.min).reduce(f32::min);
            let max = cores.iter().filter_map(|s| s.max).reduce(f32::max);
            // Con tutti i core basta il numero; con una parte si elencano quelli usati.
            let which = if cores.len() == total {
                format!("{} core", cores.len())
            } else {
                let nums: Vec<&str> = cores
                    .iter()
                    .map(|s| s.name.rsplit('#').next().unwrap_or(&s.name))
                    .collect();
                format!("core {}", nums.join(", "))
            };
            return Some(Reading {
                title: custom.cloned().unwrap_or_else(|| "CPU · core più caldo".into()),
                subtitle: format!("{} · {which}", cores[0].hardware),
                kind: "Temperature".into(),
                unit: cores[0].unit.clone(),
                value: Some(hottest),
                min,
                max,
                detail: Some(format!(
                    "media {} · più fredda {}",
                    fmt_num(avg, "Temperature"),
                    fmt_num(coldest, "Temperature")
                )),
            });
        }
        let s = self.sensors.iter().find(|s| s.id == key)?;
        Some(Reading {
            title: custom.cloned().unwrap_or_else(|| s.name.clone()),
            // Con un nome personalizzato, il nome originale resta visibile nel sottotitolo.
            subtitle: if custom.is_some() { format!("{} · {}", s.hardware, s.name) } else { s.hardware.clone() },
            kind: s.kind.clone(),
            unit: s.unit.clone(),
            value: s.value,
            min: s.min,
            max: s.max,
            detail: None,
        })
    }

    fn status_color(&self, r: &Reading, weak: Color32) -> Color32 {
        match (r.kind.as_str(), r.value) {
            (_, None) => weak,
            ("Temperature", Some(v)) if v >= self.settings.crit_c => RED,
            ("Temperature", Some(v)) if v >= self.settings.warn_c => AMBER,
            ("Temperature", Some(_)) => GREEN,
            ("Fan", Some(v)) if v < 1.0 => RED,
            _ => ACCENT,
        }
    }

    fn apply_window_settings(&mut self, ctx: &egui::Context) {
        if self.applied_on_top != Some(self.settings.always_on_top) {
            let level = if self.settings.always_on_top {
                egui::WindowLevel::AlwaysOnTop
            } else {
                egui::WindowLevel::Normal
            };
            ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(level));
            self.applied_on_top = Some(self.settings.always_on_top);
        }
        if self.applied_theme != Some(self.settings.theme) {
            ctx.set_theme(match self.settings.theme {
                Theme::Sistema => egui::ThemePreference::System,
                Theme::Chiaro => egui::ThemePreference::Light,
                Theme::Scuro => egui::ThemePreference::Dark,
            });
            self.applied_theme = Some(self.settings.theme);
        }
    }

    fn push_config(&self) {
        let mut s = self.shared.lock().unwrap();
        s.url = self.settings.url.clone();
        s.interval_ms = (self.settings.interval_s * 1000.0) as u64;
    }

    // ------------------------------------------------------------ pannelli

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Monitor Sensori");
            let (color, text) = if self.error.is_none() && !self.sensors.is_empty() {
                (GREEN, "collegato a LibreHardwareMonitor")
            } else {
                (RED, "non collegato")
            };
            let (dot, _) = ui.allocate_exact_size(Vec2::splat(12.0), Sense::hover());
            ui.painter().circle_filled(dot.center(), 5.0, color);
            ui.weak(text);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let label = if self.show_picker { "Chiudi scelta" } else { "Scegli sensori" };
                if ui.button(label).clicked() {
                    self.show_picker = !self.show_picker;
                }
                ui.checkbox(&mut self.settings.always_on_top, "In primo piano");
                egui::ComboBox::from_id_salt("tema")
                    .selected_text(self.settings.theme.label())
                    .show_ui(ui, |ui| {
                        for t in [Theme::Sistema, Theme::Chiaro, Theme::Scuro] {
                            ui.selectable_value(&mut self.settings.theme, t, t.label());
                        }
                    });
            });
        });
    }

    fn picker(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.heading("Sensori mostrati");
            ui.weak("L'ordine qui è l'ordine delle schede. Scrivi un nome per rinominarle.");
            let mut action: Option<(usize, i32)> = None; // (indice, -1 su / +1 giù / 0 togli)
            let keys = self.settings.selected.clone();
            let len = keys.len();
            for (i, key) in keys.iter().enumerate() {
                let (original, device) = if key == CPU_AGG {
                    ("CPU · core più caldo".to_string(), "core scelti sotto".to_string())
                } else {
                    self.sensors
                        .iter()
                        .find(|s| &s.id == key)
                        .map(|s| (s.name.clone(), s.hardware.clone()))
                        .unwrap_or_else(|| (key.clone(), "non disponibile".into()))
                };
                ui.horizontal(|ui| {
                    if ui.add_enabled(i > 0, egui::Button::new("su").small()).clicked() {
                        action = Some((i, -1));
                    }
                    if ui.add_enabled(i + 1 < len, egui::Button::new("giù").small()).clicked() {
                        action = Some((i, 1));
                    }
                    if ui.small_button("togli").clicked() {
                        action = Some((i, 0));
                    }
                    let mut name = self.settings.names.get(key).cloned().unwrap_or_default();
                    let edit = egui::TextEdit::singleline(&mut name).hint_text(original).desired_width(150.0);
                    if ui.add(edit).changed() {
                        if name.trim().is_empty() {
                            self.settings.names.remove(key);
                        } else {
                            self.settings.names.insert(key.clone(), name);
                        }
                    }
                    ui.weak(device);
                });
            }
            if let Some((i, dir)) = action {
                match dir {
                    0 => {
                        self.settings.selected.remove(i);
                    }
                    -1 => self.settings.selected.swap(i, i - 1),
                    _ => self.settings.selected.swap(i, i + 1),
                }
            }

            ui.separator();
            ui.heading("Aggiungi");
            let mut toggles: Vec<(String, bool)> = Vec::new();
            let mut on = self.settings.selected.iter().any(|k| k == CPU_AGG);
            if ui.checkbox(&mut on, "CPU · core più caldo, media e più fredda").changed() {
                toggles.push((CPU_AGG.to_string(), on));
            }
            // Core usati nella scheda CPU: almeno uno deve restare acceso.
            let core_ids: Vec<(String, String)> = cpu_core_temps(&self.sensors)
                .iter()
                .map(|s| (s.id.clone(), s.name.clone()))
                .collect();
            ui.indent("core", |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.weak("usa:");
                    let used_count = core_ids.iter().filter(|(id, _)| !self.settings.cpu_excluded.contains(id)).count();
                    for (id, name) in &core_ids {
                        let mut used = !self.settings.cpu_excluded.contains(id);
                        let last = used && used_count <= 1;
                        let label = name.trim_start_matches("CPU ").to_string();
                        if ui.add_enabled(!last, egui::Checkbox::new(&mut used, label)).changed() {
                            if used {
                                self.settings.cpu_excluded.retain(|x| x != id);
                            } else {
                                self.settings.cpu_excluded.push(id.clone());
                            }
                        }
                    }
                });
            });
            let mut hardware: Vec<&str> = Vec::new();
            for s in &self.sensors {
                if !hardware.contains(&s.hardware.as_str()) {
                    hardware.push(&s.hardware);
                }
            }
            for hw in hardware {
                egui::CollapsingHeader::new(hw).default_open(false).show(ui, |ui| {
                    for s in self.sensors.iter().filter(|s| s.hardware == hw) {
                        let mut on = self.settings.selected.contains(&s.id);
                        let label = format!("{} · {}  ({})", kind_it(&s.kind), s.name, fmt_value(s.value, &s.unit, &s.kind));
                        if ui.checkbox(&mut on, label).changed() {
                            toggles.push((s.id.clone(), on));
                        }
                    }
                });
            }
            for (id, on) in toggles {
                if on {
                    if !self.settings.selected.contains(&id) {
                        self.settings.selected.push(id);
                    }
                } else {
                    self.settings.selected.retain(|k| *k != id);
                }
            }
            if ui.button("Ripristina la scelta predefinita").clicked() && !self.sensors.is_empty() {
                self.settings.selected = default_selection(&self.sensors);
            }

            ui.separator();
            ui.heading("Impostazioni");
            egui::Grid::new("impostazioni").num_columns(2).show(ui, |ui| {
                ui.label("Temperatura di attenzione");
                ui.add(egui::DragValue::new(&mut self.settings.warn_c).range(30.0..=110.0).suffix(" °C"));
                ui.end_row();
                ui.label("Temperatura critica");
                ui.add(egui::DragValue::new(&mut self.settings.crit_c).range(30.0..=110.0).suffix(" °C"));
                ui.end_row();
                ui.label("Aggiornamento ogni");
                if ui
                    .add(egui::DragValue::new(&mut self.settings.interval_s).range(0.5..=10.0).speed(0.1).suffix(" s"))
                    .changed()
                {
                    self.push_config();
                }
                ui.end_row();
                ui.label("Indirizzo dati LHM");
                ui.horizontal(|ui| {
                    ui.text_edit_singleline(&mut self.url_edit);
                    if ui.button("Applica").clicked() {
                        self.settings.url = self.url_edit.trim().to_string();
                        self.push_config();
                    }
                });
                ui.end_row();
                ui.label("Avvio automatico");
                ui.checkbox(&mut self.settings.lhm_autostart, "apri LibreHardwareMonitor se non è aperto");
                ui.end_row();
                ui.label("Percorso LHM");
                ui.text_edit_singleline(&mut self.settings.lhm_path);
                ui.end_row();
            });
        });
    }

    fn dashboard(&mut self, ui: &mut egui::Ui) {
        if self.sensors.is_empty() {
            ui.add_space(30.0);
            let lhm_status = self.shared.lock().unwrap().lhm_status.clone();
            ui.vertical_centered(|ui| {
                ui.heading("LibreHardwareMonitor non risponde");
                ui.add_space(8.0);
                if let Some(msg) = lhm_status {
                    ui.colored_label(AMBER, msg);
                    ui.add_space(8.0);
                }
                if ui.button("Avvia LibreHardwareMonitor").clicked() {
                    start_lhm(self.shared.clone(), self.settings.lhm_path.clone(), true);
                }
                ui.add_space(8.0);
                ui.label("1. Avvia LibreHardwareMonitor (chiede i permessi di amministratore).");
                ui.label("2. Nel suo menu: Options → Remote Web Server → Run.");
                ui.label(format!("3. Questa finestra legge da {}", self.settings.url));
                if let Some(e) = &self.error {
                    ui.add_space(8.0);
                    ui.weak(format!("Ultimo errore: {e}"));
                }
            });
            return;
        }
        if self.settings.selected.is_empty() {
            ui.add_space(30.0);
            ui.vertical_centered(|ui| {
                ui.heading("Nessun sensore scelto");
                ui.label("Premi «Scegli sensori» in alto a destra.");
            });
            return;
        }
        let keys = self.settings.selected.clone();
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            let width = ui.available_width();
            let cols = (((width + GAP) / (TILE_MIN_W + GAP)).floor() as usize).max(1);
            let tile_w = (width - GAP * (cols as f32 - 1.0)) / cols as f32;
            ui.spacing_mut().item_spacing = Vec2::splat(GAP);
            for row in keys.chunks(cols) {
                ui.horizontal(|ui| {
                    for key in row {
                        self.draw_tile(ui, key, Vec2::new(tile_w, TILE_H));
                    }
                });
            }
        });
    }

    /// Scheda disegnata a mano: titolo, valore grande, mini grafico,
    /// barra con fascia verde min–max (da quando LHM è avviato) e tacca attuale.
    fn draw_tile(&self, ui: &mut egui::Ui, key: &str, size: Vec2) {
        let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
        let painter = ui.painter_at(rect);
        let vis = ui.visuals().clone();
        let strong = vis.strong_text_color();
        let weak = vis.weak_text_color();
        painter.rect(rect, 10.0, vis.faint_bg_color, vis.widgets.noninteractive.bg_stroke, egui::StrokeKind::Inside);

        let left = rect.left() + 14.0;
        let right = rect.right() - 14.0;
        let top = rect.top();
        let Some(r) = self.reading(key) else {
            painter.text(Pos2::new(left, top + 22.0), Align2::LEFT_CENTER, key, FontId::proportional(15.0), strong);
            painter.text(Pos2::new(left, top + 60.0), Align2::LEFT_CENTER, "non disponibile", FontId::proportional(14.0), weak);
            return;
        };
        let color = self.status_color(&r, weak);

        painter.text(Pos2::new(left, top + 20.0), Align2::LEFT_CENTER, &r.title, FontId::proportional(15.0), strong);
        painter.text(Pos2::new(left, top + 38.0), Align2::LEFT_CENTER, &r.subtitle, FontId::proportional(11.5), weak);
        painter.text(
            Pos2::new(left, top + 70.0),
            Align2::LEFT_CENTER,
            fmt_value(r.value, &r.unit, &r.kind),
            FontId::proportional(30.0),
            color,
        );
        if let Some(d) = &r.detail {
            painter.text(Pos2::new(left, top + 97.0), Align2::LEFT_CENTER, d, FontId::proportional(12.0), weak);
        }

        let (lo, hi) = scale(&r);
        let to_x = |bar: Rect, v: f32| bar.left() + ((v - lo) / (hi - lo)).clamp(0.0, 1.0) * bar.width();

        // Mini grafico degli ultimi campioni, nella metà destra.
        if let Some(h) = self.history.get(key).filter(|h| h.len() > 1) {
            let area = Rect::from_min_max(Pos2::new(rect.center().x + 6.0, top + 50.0), Pos2::new(right, top + 92.0));
            let n = (HISTORY_LEN - 1) as f32;
            let offset = (HISTORY_LEN - h.len()) as f32;
            let pts: Vec<Pos2> = h
                .iter()
                .enumerate()
                .map(|(i, v)| {
                    let t = ((v - lo) / (hi - lo)).clamp(0.0, 1.0);
                    Pos2::new(area.left() + (offset + i as f32) / n * area.width(), area.bottom() - t * area.height())
                })
                .collect();
            painter.add(egui::Shape::line(pts, Stroke::new(1.5_f32, color.gamma_multiply(0.8))));
        }

        // Barra: binario, fascia verde min–max, soglie, tacca del valore attuale.
        let bar = Rect::from_min_max(Pos2::new(left, top + 112.0), Pos2::new(right, top + 122.0));
        painter.rect_filled(bar, 5.0, vis.extreme_bg_color);
        if let (Some(mn), Some(mx)) = (r.min, r.max) {
            let band = Rect::from_min_max(Pos2::new(to_x(bar, mn), bar.top()), Pos2::new(to_x(bar, mx).max(to_x(bar, mn) + 2.0), bar.bottom()));
            painter.rect_filled(band, 5.0, GREEN.gamma_multiply(0.55));
        }
        if r.kind == "Temperature" {
            for (t, c) in [(self.settings.warn_c, AMBER), (self.settings.crit_c, RED)] {
                if t > lo && t < hi {
                    let x = to_x(bar, t);
                    painter.line_segment([Pos2::new(x, bar.top() - 3.0), Pos2::new(x, bar.bottom() + 3.0)], Stroke::new(1.5_f32, c));
                }
            }
        }
        if let Some(v) = r.value {
            let x = to_x(bar, v);
            painter.circle(Pos2::new(x, bar.center().y), 7.0, color, Stroke::new(2.0_f32, vis.faint_bg_color));
        }
        painter.text(
            Pos2::new(left, top + 137.0),
            Align2::LEFT_CENTER,
            format!("min {}", fmt_value(r.min, &r.unit, &r.kind)),
            FontId::proportional(11.5),
            weak,
        );
        painter.text(
            Pos2::new(right, top + 137.0),
            Align2::RIGHT_CENTER,
            format!("max {}", fmt_value(r.max, &r.unit, &r.kind)),
            FontId::proportional(11.5),
            weak,
        );
    }
}

fn kind_it(kind: &str) -> &str {
    match kind {
        "Temperature" => "Temperatura",
        "Fan" => "Ventola",
        "Voltage" => "Tensione",
        "Clock" => "Frequenza",
        "Load" => "Carico",
        "Power" => "Potenza",
        "Control" => "Controllo",
        "Data" | "SmallData" => "Dati",
        "Throughput" => "Traffico",
        other => other,
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.sync();
        self.apply_window_settings(ctx);
        egui::TopBottomPanel::top("barra").show(ctx, |ui| {
            ui.add_space(4.0);
            self.top_bar(ui);
            ui.add_space(4.0);
        });
        if self.show_picker {
            egui::SidePanel::right("scelta")
                .default_width(360.0)
                .resizable(true)
                .show(ctx, |ui| self.picker(ui));
        }
        egui::CentralPanel::default().show(ctx, |ui| self.dashboard(ui));
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, eframe::APP_KEY, &self.settings);
    }
}

/// Icona generata al volo: tre barre verde/ambra/rosso su fondo scuro.
fn make_icon() -> egui::IconData {
    let n = 64usize;
    let mut rgba = vec![0u8; n * n * 4];
    for y in 0..n {
        for x in 0..n {
            let i = (y * n + x) * 4;
            let inside = x >= 4 && x < n - 4 && y >= 4 && y < n - 4;
            let mut c = if inside { [32, 36, 44, 255] } else { [0, 0, 0, 0] };
            for (k, (h, col)) in [(20, [46, 160, 67]), (32, [222, 158, 36]), (44, [222, 64, 64])].iter().enumerate() {
                let x0 = 12 + k * 15;
                if x >= x0 && x < x0 + 10 && y >= n - 10 - h && y < n - 10 {
                    c = [col[0], col[1], col[2], 255];
                }
            }
            rgba[i..i + 4].copy_from_slice(&c);
        }
    }
    egui::IconData { rgba, width: n as u32, height: n as u32 }
}
