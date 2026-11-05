mod gamma;

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime};
use eframe::egui;
use egui_plot::{Legend, Line, Plot, PlotPoint, PlotPoints, Points};

use gamma::{
    AvgPoint, AvgUnit, Command, DeviceState, FwClass, Mode, Outcome, ReadResult, Record, Session,
    FLAG_DOSE_ALARM, FLAG_OVERFLOW, FLAG_RATE_ALARM,
};

/// Requests from the GUI to the background thread
enum Req {
    Cmd(Command),
    Disconnect,
}

/// Events from the background thread to the GUI
enum Event {
    Log(String),
    /// Device state and the port actually used
    Connected(DeviceState, String),
    State(DeviceState),
    Text { cmd: String, text: String },
    Protocol(ReadResult),
    Error(String),
    Idle,
    Disconnected,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum YMode {
    Cpm,
    Pulses,
}

const MAX_CONSOLE: usize = 300;
const MAX_CONSOLE_CHARS: usize = 6000;

struct App {
    // Connection settings
    port: String,
    ports: Vec<String>,
    baud: Option<u32>,
    fw_text: String,
    compat_dump: bool,
    exit_pc_mode: bool,
    try_other_ports: bool,

    // Connection
    connecting: bool,
    connected: bool,
    busy: bool,
    state: Option<DeviceState>,
    tx: Option<Sender<Req>>,
    rx: Option<Receiver<Event>>,
    cancel: Arc<AtomicBool>,

    // Confirmation dialog for dangerous commands
    confirm: Option<Command>,
    confirm_ack: bool,

    // Set date/time
    date_text: String,
    time_text: String,

    // Online mode
    online_interval: u8,
    poll_online: bool,
    poll_secs: u32,
    last_poll: Instant,

    // Data
    records: Vec<Record>,
    source: String,
    session_log: Vec<u8>,

    // Display
    y_mode: YMode,
    show_points: bool,
    reset_view: bool,
    avg: AvgUnit,
    avg_cache: Vec<AvgPoint>,
    avg_cache_key: (AvgUnit, u64),
    data_version: u64,

    // Messages
    console: Vec<String>,
    /// the last connection attempt failed (its error is already in the console)
    last_was_error: bool,
}

impl App {
    fn new() -> Self {
        let ports = gamma::list_ports();
        let port = ports
            .first()
            .cloned()
            .unwrap_or_else(|| if cfg!(windows) { "COM3".into() } else { "/dev/ttyUSB0".into() });
        let now = chrono::Local::now().naive_local();
        Self {
            port,
            ports,
            baud: None,
            fw_text: String::new(),
            compat_dump: false,
            exit_pc_mode: true,
            try_other_ports: true,
            connecting: false,
            connected: false,
            busy: false,
            state: None,
            tx: None,
            rx: None,
            cancel: Arc::new(AtomicBool::new(false)),
            confirm: None,
            confirm_ack: false,
            date_text: now.format("%Y-%m-%d").to_string(),
            time_text: now.format("%H:%M:%S").to_string(),
            online_interval: 1,
            poll_online: false,
            poll_secs: 5,
            last_poll: Instant::now(),
            records: Vec::new(),
            source: String::new(),
            session_log: Vec::new(),
            y_mode: YMode::Cpm,
            show_points: true,
            reset_view: false,
            avg: AvgUnit::Off,
            avg_cache: Vec::new(),
            avg_cache_key: (AvgUnit::Off, 0),
            data_version: 0,
            console: Vec::new(),
            last_was_error: false,
        }
    }

    // ------------------------------------------------------------ Connection

    fn connect(&mut self, ctx: &egui::Context) {
        let selected = self.port.trim().to_string();
        // Selected port first; on failure (optional) the other available ports
        let mut ports = vec![selected];
        if self.try_other_ports {
            self.ports = gamma::list_ports();
            for p in &self.ports {
                if !ports.contains(p) {
                    ports.push(p.clone());
                }
            }
        }
        let baud = self.baud;
        let fw = self.fw_text.trim().replace(',', ".").parse::<f64>().ok();
        let (req_tx, req_rx) = mpsc::channel::<Req>();
        let (evt_tx, evt_rx) = mpsc::channel::<Event>();
        self.tx = Some(req_tx);
        self.rx = Some(evt_rx);
        self.connecting = true;
        self.busy = true;
        self.cancel.store(false, Ordering::Relaxed);
        self.last_was_error = false;
        self.set_status("Connecting ...".to_string());

        let cancel = self.cancel.clone();
        let ctx = ctx.clone();
        thread::spawn(move || worker(ports, baud, fw, cancel, req_rx, evt_tx, ctx));
    }

    fn disconnect(&mut self) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(Req::Disconnect);
        }
        self.cancel.store(true, Ordering::Relaxed);
    }

    fn send_cmd(&mut self, cmd: Command) {
        if self.tx.is_none() {
            return;
        }
        self.busy = true;
        self.cancel.store(false, Ordering::Relaxed);
        self.push_console(format!("› {}", cmd.label()));
        if let Some(tx) = &self.tx {
            let _ = tx.send(Req::Cmd(cmd));
        }
    }

    fn push_console(&mut self, s: String) {
        self.console.push(s);
        if self.console.len() > MAX_CONSOLE {
            let n = self.console.len() - MAX_CONSOLE;
            self.console.drain(..n);
        }
    }

    /// Status messages are shown in the console (there is no separate status line).
    fn set_status(&mut self, s: String) {
        self.push_console(s);
    }

    fn set_records(&mut self, recs: Vec<Record>) {
        self.records = recs;
        self.data_version += 1;
    }

    fn poll_events(&mut self) {
        let events: Vec<Event> = self
            .rx
            .as_ref()
            .map(|rx| rx.try_iter().collect())
            .unwrap_or_default();
        for e in events {
            match e {
                Event::Log(s) => self.push_console(s),
                Event::Connected(st, port) => {
                    self.port = port;
                    self.connected = true;
                    self.connecting = false;
                    let msg = format!(
                        "Connected: {}, {} baud, mode {}{}",
                        self.port,
                        st.baud,
                        st.mode.text(),
                        st.fw.map(|f| format!(", FW {f}")).unwrap_or_default()
                    );
                    self.set_status(msg);
                    self.state = Some(st);
                }
                Event::State(st) => self.state = Some(st),
                Event::Text { cmd, text } => {
                    let mut t = text.replace("\r\n", "\n").replace('\r', "\n");
                    if t.chars().count() > MAX_CONSOLE_CHARS {
                        let total = t.chars().count();
                        t = t.chars().take(MAX_CONSOLE_CHARS).collect::<String>();
                        t.push_str(&format!("\n… ({} characters truncated)", total - MAX_CONSOLE_CHARS));
                    }
                    if t.trim().is_empty() {
                        t = "(no response)".into();
                    }
                    self.push_console(format!("[{cmd}]\n{}", t.trim_end()));
                }
                Event::Protocol(res) => {
                    if !res.info.trim().is_empty() {
                        self.push_console(format!("Device info: {}", res.info.trim().replace(['\r', '\n'], " | ")));
                    }
                    for w in &res.warnings {
                        self.push_console(format!("Warning: {w}"));
                    }
                    self.source = format!("{} (FW {}, {} baud)", self.port, res.fw, res.baud);
                    self.push_console(format!("[Log] {} entries decoded", res.records.len()));
                    self.set_records(res.records);
                    self.session_log = res.session_log;
                    self.reset_view = true;
                }
                Event::Error(e) => {
                    self.push_console(format!("✖ {e}"));
                    if self.connecting {
                        // connection attempt failed
                        self.last_was_error = true;
                        self.connecting = false;
                        self.busy = false;
                    }
                }
                Event::Idle => self.busy = false,
                Event::Disconnected => {
                    self.connected = false;
                    self.connecting = false;
                    self.busy = false;
                    self.state = None;
                    self.tx = None;
                    self.rx = None;
                    self.poll_online = false;
                    if !self.last_was_error {
                        self.set_status("Disconnected".to_string());
                    }
                    self.last_was_error = false;
                }
            }
        }
    }

    // --------------------------------------------------------- File actions

    fn save_csv(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("CSV", &["csv"])
            .set_file_name("gamma-scout.csv")
            .save_file()
        else {
            return;
        };
        let res = std::fs::File::create(&path)
            .map_err(anyhow::Error::from)
            .and_then(|mut f| gamma::write_csv(&mut f, &self.records));
        let msg = match res {
            Ok(()) => format!("Saved: {}", path.display()),
            Err(e) => format!("Error while saving: {e:#}"),
        };
        self.set_status(msg);
    }

    fn save_avg_csv(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("CSV", &["csv"])
            .set_file_name(format!("gamma-scout-{}.csv", self.avg.file_tag()))
            .save_file()
        else {
            return;
        };
        let pts = gamma::average(&self.records, self.avg);
        let res = std::fs::File::create(&path)
            .map_err(anyhow::Error::from)
            .and_then(|mut f| gamma::write_avg_csv(&mut f, &pts));
        let msg = match res {
            Ok(()) => format!("{} points saved: {}", pts.len(), path.display()),
            Err(e) => format!("Error while saving: {e:#}"),
        };
        self.set_status(msg);
    }

    fn save_raw(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Dump", &["txt", "dump"])
            .set_file_name("gamma-scout.dump")
            .save_file()
        else {
            return;
        };
        let msg = match std::fs::write(&path, &self.session_log) {
            Ok(()) => format!("Raw data saved: {}", path.display()),
            Err(e) => format!("Error while saving: {e}"),
        };
        self.set_status(msg);
    }

    fn load_dump(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Dump", &["dump", "txt"])
            .pick_file()
        else {
            return;
        };
        let fw = self.fw_text.trim().replace(',', ".").parse::<f64>().ok();
        let res = std::fs::read(&path)
            .map_err(anyhow::Error::from)
            .and_then(|b| gamma::parse_dump(&String::from_utf8_lossy(&b), fw));
        match res {
            Ok(r) => {
                if !r.info.is_empty() {
                    self.push_console(format!("Device info: {}", r.info));
                }
                for w in &r.warnings {
                    self.push_console(format!("Warning: {w}"));
                }
                self.set_status(format!("Dump loaded: {} entries (FW {})", r.records.len(), r.fw));
                self.source = format!("{} (FW {})", path.display(), r.fw);
                self.set_records(r.records);
                self.session_log = r.session_log;
                self.reset_view = true;
            }
            Err(e) => self.set_status(format!("Error while loading: {e:#}")),
        }
    }

    fn load_csv(&mut self) {
        let Some(path) = rfd::FileDialog::new().add_filter("CSV", &["csv"]).pick_file() else {
            return;
        };
        let res = std::fs::read_to_string(&path)
            .map_err(anyhow::Error::from)
            .and_then(|t| gamma::read_csv(&t));
        match res {
            Ok(recs) => {
                self.set_status(format!("{} entries loaded", recs.len()));
                self.source = path.display().to_string();
                self.set_records(recs);
                self.session_log.clear();
                self.reset_view = true;
            }
            Err(e) => self.set_status(format!("Error while loading: {e:#}")),
        }
    }

    // ------------------------------------------------------------------- UI

    /// Command button: only enabled if the command matches firmware and mode.
    fn cmd_btn(&mut self, ui: &mut egui::Ui, cmd: Command, text: &str, tip: &str) {
        let available = self.connected
            && !self.busy
            && self.state.as_ref().is_some_and(|s| cmd.is_available(s));
        let resp = ui
            .add_enabled(available, egui::Button::new(text))
            .on_hover_text(tip)
            .on_disabled_hover_text(format!(
                "{tip}\n(currently unavailable: not connected, busy, or not valid for this \
                 firmware/mode)"
            ));
        if resp.clicked() {
            if cmd.warning().is_some() {
                self.confirm = Some(cmd);
                self.confirm_ack = false;
            } else {
                self.send_cmd(cmd);
            }
        }
    }

    fn connection_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.heading("Gamma-Scout");
        ui.separator();

        let locked = self.connected || self.connecting;
        ui.add_enabled_ui(!locked, |ui| {
            ui.label("Port");
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.port).desired_width(150.0));
                egui::ComboBox::new("ports", "")
                    .selected_text("▼")
                    .show_ui(ui, |ui| {
                        for p in self.ports.clone() {
                            ui.selectable_value(&mut self.port, p.clone(), p);
                        }
                    });
                if ui.button("⟳").on_hover_text("Rescan ports").clicked() {
                    self.ports = gamma::list_ports();
                }
            });
            ui.label("Baud rate");
            egui::ComboBox::new("baud", "")
                .selected_text(match self.baud {
                    None => "Automatic".to_string(),
                    Some(b) => b.to_string(),
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.baud, None, "Automatic");
                    for b in gamma::BAUD_CANDIDATES {
                        ui.selectable_value(&mut self.baud, Some(b), b.to_string());
                    }
                });
            ui.horizontal(|ui| {
                ui.label("Firmware (optional)");
                ui.add(egui::TextEdit::singleline(&mut self.fw_text).desired_width(60.0));
            });
            ui.checkbox(&mut self.try_other_ports, "Try other ports on failure")
                .on_hover_text(
                    "If connecting fails, the remaining serial ports are tried one after \
                     another. The character 'v' is sent to each port (at 460800, 9600 and 2400 \
                     baud, 7E1). Do not enable this if other devices are attached that might \
                     react unexpectedly.",
                );
        });

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if self.connected {
                if ui.button("Disconnect").clicked() {
                    self.disconnect();
                }
            } else if ui
                .add_enabled(!self.connecting, egui::Button::new("🔌 Connect"))
                .clicked()
            {
                self.connect(ctx);
            }
            if self.busy {
                ui.spinner();
                if ui.button("Cancel").clicked() {
                    self.cancel.store(true, Ordering::Relaxed);
                }
            }
        });

        if let Some(st) = self.state.clone() {
            ui.add_space(4.0);
            egui::Grid::new("devstate").num_columns(2).show(ui, |ui| {
                ui.label("Mode");
                ui.strong(st.mode.text());
                ui.end_row();
                ui.label("Firmware");
                ui.label(st.fw.map_or("unknown (only known in PC mode)".into(), |f| f.to_string()));
                ui.end_row();
                ui.label("Baud rate");
                ui.label(format!(
                    "{} ({})",
                    st.baud,
                    match st.class() {
                        FwClass::Old => "FW < 6.00",
                        FwClass::Mid => "FW 6.00 – 6.89",
                        FwClass::New => "FW ≥ 6.90",
                    }
                ));
                ui.end_row();
                if let Some(sn) = &st.serial {
                    ui.label("Serial number");
                    ui.label(sn);
                    ui.end_row();
                }
                if let Some(u) = st.used_bytes {
                    ui.label("Memory in use");
                    ui.label(format!("{u} bytes"));
                    ui.end_row();
                }
                if let Some(t) = st.device_time {
                    ui.label("Device time (at 'v')");
                    ui.label(t.format("%Y-%m-%d %H:%M:%S").to_string());
                    ui.end_row();
                }
            });
        }
    }

    fn commands_ui(&mut self, ui: &mut egui::Ui) {
        // Quick access
        ui.horizontal(|ui| {
            let enabled = self.connected && !self.busy;
            if ui.add_enabled(enabled, egui::Button::new("📡 Read log")).clicked() {
                self.send_cmd(Command::ReadAll {
                    compat_dump: self.compat_dump,
                    exit_pc_mode: self.exit_pc_mode,
                });
            }
        });
        ui.checkbox(&mut self.exit_pc_mode, "Leave PC mode afterwards ('X', FW 6.00+)");
        ui.checkbox(&mut self.compat_dump, "Also query extra data for the dump ('h','s','c')");

        egui::CollapsingHeader::new("All commands").default_open(true).show(ui, |ui| {
            ui.small(
                "Only commands that match the firmware and the current mode according to \
                 interface description V1.12 are enabled (hover over a button for details).",
            );

            ui.label(egui::RichText::new("Info").strong());
            ui.horizontal_wrapped(|ui| {
                self.cmd_btn(ui, Command::Version, "v", "Version or current mode (in PC mode: FW, serial no., memory usage, date/time)");
                self.cmd_btn(ui, Command::DumpConversion, "c", "Output internal conversion data (FW 6.00+, PC mode)");
                self.cmd_btn(ui, Command::DumpH, "h", "Internal h data (FW 7.04+, PC mode)");
                self.cmd_btn(ui, Command::DumpS, "s (PC)", "Internal s data (FW 7.04+, PC mode)");
                self.cmd_btn(ui, Command::AlertQuery, "t (Alert?)", "FW 3.03 to 5.99: the device answers \"AN\" if it is an Alert device, otherwise \"AUS\"");
            });

            ui.label(egui::RichText::new("Mode (FW 6.00+)").strong());
            ui.horizontal_wrapped(|ui| {
                self.cmd_btn(ui, Command::EnterPc, "P", "Switch to PC mode (like the PC button)");
                self.cmd_btn(ui, Command::ExitMode, "X", "Leave PC mode or online mode");
                self.cmd_btn(ui, Command::OnlineClassic, "O", "\"Classic\" online mode (online devices only)");
                self.cmd_btn(ui, Command::OnlineDoserate, "R", "Dose-rate online mode (online devices only)");
                self.cmd_btn(ui, Command::OnlineDose, "D", "Dose online mode (online devices only)");
            });

            ui.label(egui::RichText::new("Log memory").strong());
            ui.horizontal_wrapped(|ui| {
                self.cmd_btn(ui, Command::DumpProtocol, "b", "Read the log memory and show it in the graph (PC mode)");
                self.cmd_btn(ui, Command::ResetPointer, "z", "Reset the log pointer to the start of memory (PC mode) – careful!");
            });

            ui.label(egui::RichText::new("Online mode").strong());
            ui.horizontal_wrapped(|ui| {
                self.cmd_btn(ui, Command::OnlineStatus, "s (Online)", "Query online status and counts (classic online mode)");
                ui.add(egui::DragValue::new(&mut self.online_interval).range(0..=9));
                let n = self.online_interval;
                self.cmd_btn(
                    ui,
                    Command::OnlineInterval(n),
                    "Set interval",
                    "Set online interval '0'–'9' ('0' only in FW 5.43 for FW < 6.00). The \
                     meaning of the digits is not described in the documentation.",
                );
            });
            let online = matches!(self.state.as_ref().map(|s| &s.mode), Some(Mode::Online(_)));
            ui.add_enabled_ui(online, |ui| {
                ui.horizontal(|ui| {
                    ui.checkbox(&mut self.poll_online, "Auto-poll 's' every");
                    ui.add(egui::DragValue::new(&mut self.poll_secs).range(1..=3600).suffix(" s"));
                });
            });

            ui.label(egui::RichText::new("Date / time").strong());
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.date_text).desired_width(90.0).hint_text("YYYY-MM-DD"));
                ui.add(egui::TextEdit::singleline(&mut self.time_text).desired_width(70.0).hint_text("hh:mm:ss"));
                if ui.button("PC time").on_hover_text("Fill in the current PC time").clicked() {
                    let now = chrono::Local::now().naive_local();
                    self.date_text = now.format("%Y-%m-%d").to_string();
                    self.time_text = now.format("%H:%M:%S").to_string();
                }
            });
            let parsed = parse_datetime(&self.date_text, &self.time_text);
            ui.horizontal_wrapped(|ui| {
                let dt = parsed.unwrap_or_else(|| chrono::Local::now().naive_local());
                if parsed.is_none() {
                    ui.colored_label(egui::Color32::LIGHT_RED, "Invalid date/time (year 2000–2099)");
                }
                ui.add_enabled_ui(parsed.is_some(), |ui| {
                    self.cmd_btn(
                        ui,
                        Command::SetDateTime(dt),
                        "Set (t / u+d)",
                        "FW < 6.00: 'u' hhmm and 'd' DDMMYY (550 ms pause after each character); FW \
                         6.00+: 't' DDMMYYhhmmss. PC mode required.",
                    );
                });
            });

            ui.label(egui::RichText::new("Dangerous").strong());
            ui.horizontal_wrapped(|ui| {
                self.cmd_btn(ui, Command::ColdStart, "i", "Prepare a cold start and reset the device (FW 6.00 – 6.89 only) – !!!");
                self.cmd_btn(ui, Command::WarmStart, "N", "Warm start (FW 6.90+, PC mode)");
            });
        });
    }

    fn data_ui(&mut self, ui: &mut egui::Ui) {
        ui.label("Data");
        let has_data = !self.records.is_empty();
        let time_avail =
            has_data && self.records.iter().all(|r| r.start.is_some() && r.duration_s.is_some());
        ui.horizontal_wrapped(|ui| {
            if ui.add_enabled(has_data, egui::Button::new("💾 Save CSV")).clicked() {
                self.save_csv();
            }
            if ui.button("📂 Load CSV").clicked() {
                self.load_csv();
            }
            if ui
                .button("📂 Load dump")
                .on_hover_text("Read and decode a recorded dump file – no device needed")
                .clicked()
            {
                self.load_dump();
            }
            if ui
                .add_enabled(!self.session_log.is_empty(), egui::Button::new("Save raw data"))
                .clicked()
            {
                self.save_raw();
            }
            let avg_active = self.avg != AvgUnit::Off && time_avail && self.y_mode == YMode::Cpm;
            if ui
                .add_enabled(avg_active, egui::Button::new("💾 CSV (averaged)"))
                .on_hover_text("Save the averaged values currently shown")
                .clicked()
            {
                self.save_avg_csv();
            }
        });

        ui.separator();
        ui.label("Display");
        ui.radio_value(&mut self.y_mode, YMode::Cpm, "Counts per minute (cpm)");
        ui.radio_value(&mut self.y_mode, YMode::Pulses, "Counts per interval");
        ui.add_enabled_ui(time_avail && self.y_mode == YMode::Cpm, |ui| {
            ui.horizontal(|ui| {
                ui.label("Average per");
                egui::ComboBox::new("avg", "")
                    .selected_text(self.avg.label())
                    .show_ui(ui, |ui| {
                        for u in AvgUnit::ALL {
                            ui.selectable_value(&mut self.avg, u, u.label());
                        }
                    });
            });
        });
        ui.small(
            "Averaging: counts ÷ covered time per time window (cpm only, requires \
             timestamps). Entries longer than the chosen unit (e.g. weekly values with \
             \"Day\") are left unchanged; entries that span window boundaries are split \
             proportionally. Weeks start on Monday.",
        );
        ui.checkbox(&mut self.show_points, "Show data points");
        if ui.button("Reset view").clicked() {
            self.reset_view = true;
        }
        ui.small(
            "Controls: drag = pan · Ctrl + mouse wheel = zoom · right-click drag = zoom \
             to area · double-click = reset",
        );

        if has_data {
            ui.separator();
            self.stats_ui(ui);
        }
    }

    fn stats_ui(&self, ui: &mut egui::Ui) {
        let n = self.records.len();
        let total: u64 = self.records.iter().map(|r| r.pulses).sum();
        let (mut dur_total, mut pulses_timed) = (0u64, 0u64);
        for r in &self.records {
            if let Some(d) = r.duration_s {
                dur_total += d as u64;
                pulses_timed += r.pulses;
            }
        }
        let max_cpm = self.records.iter().filter_map(|r| r.cpm()).fold(0.0f64, f64::max);
        let first = self.records.iter().find_map(|r| r.start);
        let last = self.records.iter().rev().find_map(|r| r.start);
        let alarms = self.records.iter().filter(|r| r.flags != 0).count();

        ui.label("Statistics");
        egui::Grid::new("stats").num_columns(2).show(ui, |ui| {
            ui.label("Entries");
            ui.label(n.to_string());
            ui.end_row();
            ui.label("Total counts");
            ui.label(total.to_string());
            ui.end_row();
            if dur_total > 0 {
                ui.label("Avg cpm");
                ui.label(format!("{:.1}", pulses_timed as f64 * 60.0 / dur_total as f64));
                ui.end_row();
            }
            ui.label("Max cpm");
            ui.label(format!("{max_cpm:.1}"));
            ui.end_row();
            if let (Some(a), Some(b)) = (first, last) {
                ui.label("From");
                ui.label(a.format("%Y-%m-%d %H:%M").to_string());
                ui.end_row();
                ui.label("To");
                ui.label(b.format("%Y-%m-%d %H:%M").to_string());
                ui.end_row();
            }
            if alarms > 0 {
                ui.label("Entries with alarm/overflow");
                ui.label(alarms.to_string());
                ui.end_row();
            }
        });
        if !self.source.is_empty() {
            ui.small(format!("Source: {}", self.source));
        }
    }

    fn side_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        self.connection_ui(ui, ctx);
        ui.separator();
        self.commands_ui(ui);
        ui.separator();
        self.data_ui(ui);
    }

    fn console_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.strong("Console (device responses)");
            if ui.small_button("Clear").clicked() {
                self.console.clear();
            }
        });
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .stick_to_bottom(true)
            .show(ui, |ui| {
                for l in &self.console {
                    ui.monospace(l);
                }
            });
    }

    fn confirm_dialog(&mut self, ctx: &egui::Context) {
        let Some(cmd) = self.confirm.clone() else { return };
        let mut open = true;
        let mut action: Option<bool> = None;
        let ack = &mut self.confirm_ack;
        egui::Window::new(format!("Run command '{}'?", cmd.label()))
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.set_max_width(420.0);
                ui.label(cmd.warning().unwrap_or(""));
                ui.add_space(6.0);
                ui.checkbox(ack, "I understand and want to continue");
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.add_enabled(*ack, egui::Button::new("Run")).clicked() {
                        action = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        action = Some(false);
                    }
                });
            });
        if !open || action == Some(false) {
            self.confirm = None;
            self.confirm_ack = false;
        } else if action == Some(true) {
            self.confirm = None;
            self.confirm_ack = false;
            self.send_cmd(cmd);
        }
    }

    fn plot_ui(&mut self, ui: &mut egui::Ui) {
        if self.records.is_empty() {
            ui.centered_and_justified(|ui| {
                ui.label("No data. Connect and read the log, or load a CSV/dump file.");
            });
            return;
        }

        // Time axis only if all entries have a timestamp
        let time_axis = self.records.iter().all(|r| r.start.is_some());
        let mode = self.y_mode;

        let time_avail = self.records.iter().all(|r| r.start.is_some() && r.duration_s.is_some());
        let use_avg = self.avg != AvgUnit::Off && mode == YMode::Cpm && time_avail;

        let mut pts: Vec<[f64; 2]> = Vec::with_capacity(self.records.len());
        let mut flagged: Vec<[f64; 2]> = Vec::new();
        if use_avg {
            if self.avg_cache_key != (self.avg, self.data_version) {
                self.avg_cache = gamma::average(&self.records, self.avg);
                self.avg_cache_key = (self.avg, self.data_version);
            }
            for p in &self.avg_cache {
                // Middle of the covered period
                let x = p.start.and_utc().timestamp() as f64
                    + (p.end - p.start).num_seconds() as f64 / 2.0;
                pts.push([x, p.cpm]);
            }
        } else {
            for (i, r) in self.records.iter().enumerate() {
                let x = if time_axis {
                    // Middle of the interval
                    r.start.map(|t| t.and_utc().timestamp() as f64).unwrap_or(0.0)
                        + r.duration_s.unwrap_or(0) as f64 / 2.0
                } else {
                    i as f64
                };
                let y = match mode {
                    YMode::Cpm => match r.cpm() {
                        Some(c) => c,
                        None => continue,
                    },
                    YMode::Pulses => r.pulses as f64,
                };
                pts.push([x, y]);
                if r.flags & (FLAG_OVERFLOW | FLAG_DOSE_ALARM | FLAG_RATE_ALARM) != 0 {
                    flagged.push([x, y]);
                }
            }
        }

        let y_label = match mode {
            YMode::Cpm => "Counts per minute",
            YMode::Pulses => "Counts per interval",
        };
        let name: String = if use_avg {
            format!("cpm (avg per {})", self.avg.label())
        } else {
            match mode {
                YMode::Cpm => "cpm".to_string(),
                YMode::Pulses => "Counts".to_string(),
            }
        };

        let mut plot = Plot::new("gamma_scout_plot")
            .legend(Legend::default())
            .x_axis_label(if time_axis { "Time" } else { "Entry" })
            .y_axis_label(y_label)
            .include_y(0.0)
            .allow_zoom(true)
            .allow_drag(true)
            .allow_boxed_zoom(true)
            .x_axis_formatter(move |mark, _range| {
                if time_axis {
                    format_time(mark.value, mark.step_size)
                } else {
                    format!("{}", mark.value.round() as i64)
                }
            })
            .label_formatter(move |series, p: &PlotPoint| {
                let x = if time_axis {
                    format_time(p.x, 1.0)
                } else {
                    format!("Entry {}", p.x.round() as i64)
                };
                if series.is_empty() {
                    format!("{x}\n{:.1}", p.y)
                } else {
                    format!("{series}\n{x}\n{:.1}", p.y)
                }
            });

        if self.reset_view {
            plot = plot.reset();
            self.reset_view = false;
        }

        // With very many entries (e.g. 32,000) markers would be slow and cluttered
        let show_points = self.show_points && pts.len() <= 5000;
        plot.show(ui, |plot_ui| {
            plot_ui.line(Line::new(PlotPoints::from(pts.clone())).name(name));
            if show_points {
                plot_ui.points(Points::new(PlotPoints::from(pts)).radius(2.5_f32).name("Data points"));
            }
            if !flagged.is_empty() {
                plot_ui.points(
                    Points::new(PlotPoints::from(flagged))
                        .radius(5.0_f32)
                        .color(egui::Color32::RED)
                        .name("Alarm / overflow"),
                );
            }
        });
    }
}

/// "YYYY-MM-DD" + "hh:mm:ss" -> date/time (year 2000–2099, because the device only stores two digits)
fn parse_datetime(date: &str, time: &str) -> Option<NaiveDateTime> {
    use chrono::Datelike;
    let d = NaiveDate::parse_from_str(date.trim(), "%Y-%m-%d").ok()?;
    if !(2000..=2099).contains(&d.year()) {
        return None;
    }
    let t = NaiveTime::parse_from_str(time.trim(), "%H:%M:%S")
        .or_else(|_| NaiveTime::parse_from_str(time.trim(), "%H:%M"))
        .ok()?;
    Some(NaiveDateTime::new(d, t))
}

/// Unix seconds (device time, no time zone conversion) -> text; level of detail depends on the grid step.
fn format_time(secs: f64, step: f64) -> String {
    let fmt = if step < 60.0 {
        "%H:%M:%S"
    } else if step < 86_400.0 {
        "%m-%d %H:%M"
    } else {
        "%Y-%m-%d"
    };
    DateTime::from_timestamp(secs as i64, 0)
        .map(|d| d.format(fmt).to_string())
        .unwrap_or_default()
}

/// Background thread: owns the serial connection and processes requests one after another.
fn worker(
    ports: Vec<String>,
    baud: Option<u32>,
    fw: Option<f64>,
    cancel: Arc<AtomicBool>,
    rx: Receiver<Req>,
    tx: Sender<Event>,
    ctx: egui::Context,
) {
    let send = |e: Event| {
        let _ = tx.send(e);
        ctx.request_repaint();
    };
    let log = |s: String| send(Event::Log(s));

    // Try ports in order (the one chosen by the user comes first)
    let mut found: Option<(Session, String)> = None;
    let mut errors: Vec<String> = Vec::new();
    for (n, p) in ports.iter().enumerate() {
        if n > 0 {
            log(format!("Trying next port: {p}"));
        }
        match Session::connect(p, baud, fw, &log, cancel.clone()) {
            Ok(s) => {
                found = Some((s, p.clone()));
                break;
            }
            Err(e) => {
                log(format!("✖ {p}: {e:#}"));
                errors.push(format!("{p}: {e:#}"));
                if cancel.load(Ordering::Relaxed) {
                    break; // cancelled by the user
                }
            }
        }
    }
    let Some((mut session, used_port)) = found else {
        let msg = if errors.len() > 1 {
            format!("No Gamma-Scout found on {} ports – {}", errors.len(), errors.join(" | "))
        } else {
            errors.join(" | ")
        };
        send(Event::Error(msg));
        send(Event::Idle);
        send(Event::Disconnected);
        return;
    };
    if ports.first().is_some_and(|f| *f != used_port) {
        log(format!("Device found on {used_port}"));
    }
    send(Event::Connected(session.state.clone(), used_port));
    send(Event::Idle);

    while let Ok(req) = rx.recv() {
        match req {
            Req::Disconnect => break,
            Req::Cmd(cmd) => {
                let label = cmd.label();
                match session.execute(&cmd, &log) {
                    Ok(Outcome::Text(t)) => send(Event::Text { cmd: label, text: t }),
                    Ok(Outcome::Protocol(r)) => send(Event::Protocol(r)),
                    Err(e) => send(Event::Error(format!("{label}: {e:#}"))),
                }
                send(Event::State(session.state.clone()));
                send(Event::Idle);
            }
        }
    }
    send(Event::Disconnected);
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_events();

        // Automatic 's' polling in online mode
        if self.poll_online && self.connected {
            let online = matches!(self.state.as_ref().map(|s| &s.mode), Some(Mode::Online(_)));
            if !online {
                self.poll_online = false;
            } else {
                if !self.busy && self.last_poll.elapsed() >= Duration::from_secs(self.poll_secs as u64) {
                    self.last_poll = Instant::now();
                    self.send_cmd(Command::OnlineStatus);
                }
                ctx.request_repaint_after(Duration::from_millis(250));
            }
        }

        egui::SidePanel::left("side").min_width(340.0).show(ctx, |ui| {
            egui::ScrollArea::vertical()
                .show(ui, |ui| self.side_ui(ui, ctx));
        });
        egui::TopBottomPanel::bottom("console")
            .resizable(true)
            .default_height(160.0)
            .show(ctx, |ui| self.console_ui(ui));
        egui::CentralPanel::default().show(ctx, |ui| self.plot_ui(ui));

        self.confirm_dialog(ctx);
    }
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1300.0, 800.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Gamma-Scout",
        options,
        Box::new(|_cc| Ok(Box::new(App::new()))),
    )
}
