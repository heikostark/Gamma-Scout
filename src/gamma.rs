//! Gamma-Scout: serial communication (Communication Interface V1.12) and decoding
//! of the log memory. Source: https://www.gamma-scout.com/de/schnittstellenbeschreibung/

use std::{
    collections::BTreeMap,
    io::{self, Read, Write},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::sleep,
    time::{Duration, Instant},
};

use anyhow::{bail, Context, Result};
use chrono::{Duration as ChronoDuration, NaiveDate, NaiveDateTime};
use serialport::{ClearBuffer, DataBits, FlowControl, Parity, SerialPort, StopBits};

/// After every command character a pause of at least 550 ms is required.
const CMD_DELAY: Duration = Duration::from_millis(600);

/// FW < 6.00: 2400, 6.00..6.90: 9600, 6.90+: 460800 baud (7E1 in each case).
pub const BAUD_CANDIDATES: [u32; 3] = [460_800, 9_600, 2_400];

// ---------------------------------------------------------------------------
// Ports
// ---------------------------------------------------------------------------

pub fn list_ports() -> Vec<String> {
    let mut v: Vec<String> = serialport::available_ports()
        .map(|p| p.into_iter().map(|x| x.port_name).collect())
        .unwrap_or_default();

    #[cfg(unix)]
    if let Ok(rd) = std::fs::read_dir("/dev") {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if n.starts_with("ttyUSB") || n.starts_with("ttyACM") || n.starts_with("cu.usbserial") {
                let p = format!("/dev/{n}");
                if !v.contains(&p) {
                    v.push(p);
                }
            }
        }
    }
    v.sort();
    v
}

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

pub const FLAG_OVERFLOW: u8 = 1; // Dose rate > 1000 µSv/h
pub const FLAG_DOSE_ALARM: u8 = 2;
pub const FLAG_RATE_ALARM: u8 = 4;

#[derive(Debug, Clone)]
pub struct Record {
    pub start: Option<NaiveDateTime>,
    pub duration_s: Option<u32>,
    pub pulses: u64,
    pub flags: u8,
    pub conversion: Option<&'static str>,
}

impl Record {
    /// Counts per minute (raw count rate, no conversion to µSv/h).
    pub fn cpm(&self) -> Option<f64> {
        match self.duration_s {
            Some(d) if d > 0 => Some(self.pulses as f64 * 60.0 / d as f64),
            _ => None,
        }
    }
}

pub struct ReadResult {
    pub records: Vec<Record>,
    pub info: String,
    pub fw: f64,
    pub baud: u32,
    /// Entire communication in chronological order (including the local echo of the characters sent)
    pub session_log: Vec<u8>,
    pub warnings: Vec<String>,
}

// ---------------------------------------------------------------------------
// Serial communication
// ---------------------------------------------------------------------------

struct Device {
    port: Box<dyn SerialPort>,
    log: Vec<u8>,
    cancel: Arc<AtomicBool>,
}

fn open_port(path: &str, baud: u32) -> Result<Box<dyn SerialPort>> {
    serialport::new(path, baud)
        .data_bits(DataBits::Seven)
        .parity(Parity::Even)
        .stop_bits(StopBits::One)
        .flow_control(FlowControl::None)
        .timeout(Duration::from_millis(500))
        .open()
        .with_context(|| format!("Port {path} could not be opened at {baud} baud (7E1)"))
}

impl Device {
    fn new(path: &str, baud: u32, cancel: Arc<AtomicBool>) -> Result<Self> {
        Ok(Self { port: open_port(path, baud)?, log: Vec::new(), cancel })
    }

    /// Reads until no character has arrived for the duration `idle`.
    fn read_until_idle(&mut self, idle: Duration) -> Result<Vec<u8>> {
        self.port.set_timeout(idle)?;
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            if self.cancel.load(Ordering::Relaxed) {
                bail!("Cancelled");
            }
            match self.port.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::TimedOut => break,
                Err(e) => return Err(e.into()),
            }
        }
        Ok(out)
    }

    /// Send one command character, collect the response, observe the minimum pause.
    fn command(&mut self, cmd: u8, idle: Duration) -> Result<Vec<u8>> {
        self.port.clear(ClearBuffer::Input)?;
        self.log.push(cmd); // local echo (the device does not echo)
        self.log.extend_from_slice(b"\r\n"); // as in official dumps: command on its own line
        let t0 = Instant::now();
        self.port.write_all(&[cmd])?;
        self.port.flush()?;
        let data = self.read_until_idle(idle)?;
        self.log.extend_from_slice(&data);
        if let Some(rest) = CMD_DELAY.checked_sub(t0.elapsed()) {
            sleep(rest);
        }
        Ok(data)
    }
}

fn looks_like_text(d: &[u8]) -> bool {
    d.len() >= 3
        && d.iter()
            .all(|&b| b == b'\r' || b == b'\n' || (0x20..0x7f).contains(&b))
}

fn detect_baud(path: &str, log: &dyn Fn(String), cancel: &Arc<AtomicBool>) -> Result<u32> {
    for &baud in &BAUD_CANDIDATES {
        log(format!("Testing {baud} baud (7E1) ..."));
        let mut dev = Device::new(path, baud, cancel.clone())?;
        let resp = dev.command(b'v', Duration::from_millis(1500))?;
        if looks_like_text(&resp) {
            log(format!("Device responds at {baud} baud."));
            return Ok(baud);
        }
    }
    bail!(
        "No response from the device. For firmware < 6.00 PC mode must be activated \
         on the device with the PC button. Please also check cable/driver (FTDI) \
         and port."
    )
}

/// First number of the form "x.y" in the 'v' response (the format is not specified in the documentation).
pub fn parse_fw(info: &str) -> Option<f64> {
    info.split(|c: char| !(c.is_ascii_digit() || c == '.'))
        .filter(|t| t.contains('.') && t.chars().next().is_some_and(|c| c.is_ascii_digit()))
        .find_map(|t| t.trim_end_matches('.').parse::<f64>().ok())
}

/// Parsed response to 'v' in PC mode. Observed on a real device (FW 7.11):
/// `Version 7.11Lb00 080518 fca6 01.11.26 17:14:14`
/// = FW + suffix, serial number, used bytes in the log memory (hex), date, time.
#[derive(Clone, Debug, Default)]
pub struct VInfo {
    pub fw: Option<f64>,
    pub fw_suffix: String,
    pub serial: Option<String>,
    pub used_bytes: Option<usize>,
    pub device_time: Option<NaiveDateTime>,
}

pub fn parse_v_info(text: &str) -> VInfo {
    let toks: Vec<&str> = text
        .split_whitespace()
        .skip_while(|t| t.eq_ignore_ascii_case("Version"))
        .collect();
    let mut info = VInfo::default();
    let Some(first) = toks.first() else { return info };
    let n = first.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(first.len());
    info.fw = first[..n].trim_end_matches('.').parse::<f64>().ok();
    info.fw_suffix = first[n..].to_string();

    // Read from the end so that an additional field (e.g. CPU version in FW 6.90+) does not interfere
    let l = toks.len();
    if l >= 5 {
        let dt = format!("{} {}", toks[l - 2], toks[l - 1]);
        if let Ok(t) = NaiveDateTime::parse_from_str(&dt, "%d.%m.%y %H:%M:%S") {
            info.device_time = Some(t);
            info.serial = Some(toks[l - 4].to_string());
            if toks[l - 3].len() <= 6 {
                info.used_bytes = usize::from_str_radix(toks[l - 3], 16).ok();
            }
        }
    }
    info
}

// ---------------------------------------------------------------------------
// Session: state, commands and their availability per firmware/mode
// ---------------------------------------------------------------------------

const IDLE: Duration = Duration::from_millis(1500);

#[derive(Clone, Debug, PartialEq)]
pub enum Mode {
    Unknown,
    Standard,
    /// Device response to 'v' ("Online x")
    Online(String),
    Pc,
}

impl Mode {
    pub fn text(&self) -> String {
        match self {
            Mode::Unknown => "unknown".into(),
            Mode::Standard => "Standard".into(),
            Mode::Online(s) => s.clone(),
            Mode::Pc => "PC".into(),
        }
    }
}

/// Firmware classes that can be told apart unambiguously by the baud rate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FwClass {
    /// FW < 6.00 (2400 baud): PC mode only, via the button on the device
    Old,
    /// 6.00 <= FW < 6.90 (9600 baud)
    Mid,
    /// FW >= 6.90 (460800 baud)
    New,
}

#[derive(Clone, Debug)]
pub struct DeviceState {
    pub baud: u32,
    pub fw: Option<f64>,
    pub mode: Mode,
    /// Last response to 'v' in PC mode
    pub info: String,
    pub serial: Option<String>,
    /// Bytes in use in the log memory according to 'v'
    pub used_bytes: Option<usize>,
    /// Device time at the moment of the last 'v' response
    pub device_time: Option<NaiveDateTime>,
}

impl DeviceState {
    pub fn class(&self) -> FwClass {
        match self.fw {
            Some(f) if f < 6.0 => FwClass::Old,
            Some(f) if f < 6.90 => FwClass::Mid,
            Some(_) => FwClass::New,
            None => match self.baud {
                2400 => FwClass::Old,
                9600 => FwClass::Mid,
                _ => FwClass::New,
            },
        }
    }
}

/// All commands documented in interface description V1.12.
#[derive(Clone, Debug)]
pub enum Command {
    /// Complete sequence: if needed 'P', 'v', ['h','s','c'], 'b', ['X']
    ReadAll { compat_dump: bool, exit_pc_mode: bool },
    /// 'v'
    Version,
    /// 'P' (FW 6.00+)
    EnterPc,
    /// 'X' (FW 6.00+): leave PC mode or online mode
    ExitMode,
    /// 'O' (FW 6.00+, online devices only)
    OnlineClassic,
    /// 'R' (FW 6.00+, online devices only)
    OnlineDoserate,
    /// 'D' (FW 6.00+, online devices only)
    OnlineDose,
    /// 's' in online mode
    OnlineStatus,
    /// '0'..'9' in online mode
    OnlineInterval(u8),
    /// 'b'
    DumpProtocol,
    /// 'c' (FW 6.00+)
    DumpConversion,
    /// 'h' (FW 7.04+)
    DumpH,
    /// 's' in PC mode (FW 7.04+)
    DumpS,
    /// 'z'
    ResetPointer,
    /// FW < 6.00: 'u' + "hhmm" and 'd' + "DDMMYY"; FW 6.00+: 't' + "DDMMYYhhmmss"
    SetDateTime(NaiveDateTime),
    /// 't' for FW 3.03 to 5.99: "AN" (Alert device) or "AUS"
    AlertQuery,
    /// 'i' (FW 6.00 to < 6.90)
    ColdStart,
    /// 'N' (FW 6.90+)
    WarmStart,
}

impl Command {
    pub fn label(&self) -> String {
        match self {
            Command::ReadAll { .. } => "Read log".into(),
            Command::Version => "v".into(),
            Command::EnterPc => "P".into(),
            Command::ExitMode => "X".into(),
            Command::OnlineClassic => "O".into(),
            Command::OnlineDoserate => "R".into(),
            Command::OnlineDose => "D".into(),
            Command::OnlineStatus => "s (Online)".into(),
            Command::OnlineInterval(n) => format!("{n} (online interval)"),
            Command::DumpProtocol => "b".into(),
            Command::DumpConversion => "c".into(),
            Command::DumpH => "h".into(),
            Command::DumpS => "s (PC)".into(),
            Command::ResetPointer => "z".into(),
            Command::SetDateTime(_) => "Set date/time".into(),
            Command::AlertQuery => "t (alert query)".into(),
            Command::ColdStart => "i".into(),
            Command::WarmStart => "N".into(),
        }
    }

    /// Warning text if the command has to be confirmed before it is sent.
    pub fn warning(&self) -> Option<&'static str> {
        match self {
            Command::ResetPointer => Some(
                "'z' resets the log pointer to the start of memory. The device then starts \
                 writing at the beginning again – existing log data will probably be lost \
                 as a result. Always read out and back up the data first!",
            ),
            Command::ColdStart => Some(
                "According to the documentation 'i' prepares a cold start and resets the \
                 device. The documentation explicitly warns (\"!!!\"). Settings and data \
                 may be lost. Only run it if you know exactly what you are doing.",
            ),
            Command::WarmStart => Some(
                "'N' triggers a warm start of the device. The connection may drop briefly; \
                 afterwards query the mode again with 'v' if necessary.",
            ),
            _ => None,
        }
    }

    /// Is the command available with the current firmware/mode according to the documentation?
    pub fn is_available(&self, st: &DeviceState) -> bool {
        use Command::*;
        let class = st.class();
        let old = class == FwClass::Old;
        let fw_ge = |v: f64| st.fw.is_some_and(|f| f >= v - 1e-9);
        // Devices < FW 6.00 only talk in PC mode (activated by a button); there the mode is tracked as Pc after 'v'
        let in_pc = st.mode == Mode::Pc;
        let online = matches!(st.mode, Mode::Online(_));
        let standard = st.mode == Mode::Standard;

        match self {
            Version | ReadAll { .. } => true,
            EnterPc => !old && (standard || online),
            ExitMode => !old && (in_pc || online),
            OnlineClassic | OnlineDoserate | OnlineDose => !old && standard,
            // Old devices: 's' and '0'-'9' only in online mode (online devices only), mode cannot be detected
            OnlineStatus => old || online,
            OnlineInterval(n) => {
                let base = old || online;
                // '0' only in FW 5.43 for FW < 6.00
                base && (*n != 0 || !old || st.fw.is_some_and(|f| (f - 5.43).abs() < 1e-6))
            }
            DumpProtocol | ResetPointer => in_pc,
            DumpConversion => !old && in_pc,
            DumpH | DumpS => !old && in_pc && fw_ge(7.04),
            SetDateTime(_) => in_pc,
            AlertQuery => old && in_pc && st.fw.map_or(true, |f| f >= 3.03),
            ColdStart => class == FwClass::Mid && in_pc,
            WarmStart => class == FwClass::New && in_pc,
        }
    }
}

pub enum Outcome {
    /// Raw text response of the device
    Text(String),
    /// Decoded log
    Protocol(ReadResult),
}

/// Existing connection to the device including state.
pub struct Session {
    dev: Device,
    pub state: DeviceState,
}

impl Session {
    pub fn connect(
        port: &str,
        baud: Option<u32>,
        fw: Option<f64>,
        log: &dyn Fn(String),
        cancel: Arc<AtomicBool>,
    ) -> Result<Self> {
        let baud = match baud {
            Some(b) => b,
            None => detect_baud(port, log, &cancel)?,
        };
        log(format!("Using {port} at {baud} baud, 7E1"));
        let dev = Device::new(port, baud, cancel)?;
        let mut s = Session {
            dev,
            state: DeviceState {
                baud,
                fw,
                mode: Mode::Unknown,
                info: String::new(),
                serial: None,
                used_bytes: None,
                device_time: None,
            },
        };
        let t = s.query(b'v', IDLE)?;
        if t.trim().is_empty() {
            bail!(
                "No response to 'v'. For FW < 6.00 please press the PC button on the device \
                 first (or check the baud rate)."
            );
        }
        s.absorb_v(&t);
        log(format!("Device response: {}", t.trim().replace(['\r', '\n'], " | ")));
        Ok(s)
    }

    fn query(&mut self, cmd: u8, idle: Duration) -> Result<String> {
        let d = self.dev.command(cmd, idle)?;
        Ok(String::from_utf8_lossy(&d).into_owned())
    }

    /// Derive mode and firmware from the response to 'v'.
    fn absorb_v(&mut self, text: &str) {
        let t = text.trim();
        if t.is_empty() {
            // For FW < 6.00 the device is silent outside PC mode
            self.state.mode = Mode::Unknown;
            return;
        }
        let remote_modes = self.state.class() != FwClass::Old;
        if remote_modes && t.contains("Standard") {
            self.state.mode = Mode::Standard;
        } else if remote_modes && t.contains("Online") {
            self.state.mode = Mode::Online(t.replace(['\r', '\n'], " "));
        } else {
            self.state.mode = Mode::Pc;
            self.state.info = t.to_string();
            let vi = parse_v_info(t);
            if let Some(f) = vi.fw.or_else(|| parse_fw(t)) {
                self.state.fw = Some(f);
            }
            self.state.serial = vi.serial;
            self.state.used_bytes = vi.used_bytes;
            self.state.device_time = vi.device_time;
        }
    }

    /// Send one character, then query 'v' to detect the new mode.
    fn cmd_then_v(&mut self, c: u8) -> Result<Outcome> {
        self.query(c, IDLE)?;
        let t = self.query(b'v', IDLE)?;
        self.absorb_v(&t);
        Ok(Outcome::Text(t))
    }

    /// Command with a parameter block. For FW < 6.00 a spacing of at least 550 ms applies to ALL characters,
    /// from FW 6.00 the parameter characters may follow quickly (recommended: 1–2 ms pause).
    fn send_param(&mut self, cmd: u8, params: &[u8], gap: Duration) -> Result<String> {
        self.dev.port.clear(ClearBuffer::Input)?;
        self.dev.log.push(cmd);
        self.dev.port.write_all(&[cmd])?;
        self.dev.port.flush()?;
        sleep(CMD_DELAY);
        for &b in params {
            self.dev.log.push(b);
            self.dev.port.write_all(&[b])?;
            self.dev.port.flush()?;
            sleep(gap);
        }
        self.dev.log.extend_from_slice(b"\r\n");
        let data = self.dev.read_until_idle(Duration::from_millis(1000))?;
        self.dev.log.extend_from_slice(&data);
        sleep(CMD_DELAY);
        Ok(String::from_utf8_lossy(&data).into_owned())
    }

    fn protocol_fw(&self) -> Result<f64> {
        match (self.state.fw, self.state.class()) {
            (Some(f), _) => Ok(f),
            (None, FwClass::Old) => Ok(5.0), // for decoding, "FW < 6.00" is sufficient
            _ => bail!(
                "Firmware version unknown – please run 'v' in PC mode or enter the firmware \
                 in the connection settings"
            ),
        }
    }

    fn read_protocol(&mut self, fw: f64) -> Result<ReadResult> {
        let raw = self.dev.command(b'b', Duration::from_secs(3))?;
        if raw.is_empty() {
            bail!("No log data received");
        }
        let mut res = build_result(
            &String::from_utf8_lossy(&raw),
            fw,
            &self.state.info,
            self.state.baud,
            self.state.used_bytes,
        );
        res.session_log = self.dev.log.clone();
        Ok(res)
    }

    pub fn execute(&mut self, cmd: &Command, log: &dyn Fn(String)) -> Result<Outcome> {
        if !cmd.is_available(&self.state) {
            bail!(
                "Command '{}' is not available with firmware {} in mode \"{}\" according to the documentation",
                cmd.label(),
                self.state.fw.map_or("?".to_string(), |f| f.to_string()),
                self.state.mode.text()
            );
        }
        match cmd {
            Command::Version => {
                let t = self.query(b'v', IDLE)?;
                self.absorb_v(&t);
                Ok(Outcome::Text(t))
            }
            Command::EnterPc => self.cmd_then_v(b'P'),
            Command::ExitMode => self.cmd_then_v(b'X'),
            Command::OnlineClassic => self.cmd_then_v(b'O'),
            Command::OnlineDoserate => self.cmd_then_v(b'R'),
            Command::OnlineDose => self.cmd_then_v(b'D'),
            Command::OnlineStatus => Ok(Outcome::Text(self.query(b's', IDLE)?)),
            Command::OnlineInterval(n) => {
                Ok(Outcome::Text(self.query(b'0' + (*n).min(9), IDLE)?))
            }
            Command::DumpConversion => Ok(Outcome::Text(self.query(b'c', Duration::from_millis(2000))?)),
            Command::DumpH => Ok(Outcome::Text(self.query(b'h', Duration::from_millis(2000))?)),
            Command::DumpS => Ok(Outcome::Text(self.query(b's', Duration::from_millis(2000))?)),
            Command::ResetPointer => {
                let t = self.query(b'z', IDLE)?;
                Ok(Outcome::Text(if t.trim().is_empty() { "'z' sent.".into() } else { t }))
            }
            Command::AlertQuery => Ok(Outcome::Text(self.query(b't', IDLE)?)),
            Command::ColdStart | Command::WarmStart => {
                let c = if matches!(cmd, Command::ColdStart) { b'i' } else { b'N' };
                let t = self.query(c, IDLE)?;
                self.state.mode = Mode::Unknown;
                sleep(Duration::from_secs(3));
                Ok(Outcome::Text(format!(
                    "'{}' sent. The device restarts – afterwards query the mode again with 'v'.\n{t}",
                    c as char
                )))
            }
            Command::SetDateTime(dt) => {
                let mut out = String::new();
                if self.state.class() == FwClass::Old {
                    // 550 ms spacing after EVERY character
                    let gap = Duration::from_millis(600);
                    out += &self.send_param(b'u', dt.format("%H%M").to_string().as_bytes(), gap)?;
                    out += &self.send_param(b'd', dt.format("%d%m%y").to_string().as_bytes(), gap)?;
                    out = format!(
                        "Time (u {}) and date (d {}) sent.\n{out}",
                        dt.format("%H%M"),
                        dt.format("%d%m%y")
                    );
                } else {
                    let p = dt.format("%d%m%y%H%M%S").to_string();
                    out += &self.send_param(b't', p.as_bytes(), Duration::from_millis(2))?;
                    let v = self.query(b'v', IDLE)?;
                    self.absorb_v(&v);
                    out = format!("t {p} sent.\n{out}\nDevice now reports:\n{v}");
                }
                Ok(Outcome::Text(out))
            }
            Command::DumpProtocol => {
                let fw = self.protocol_fw()?;
                Ok(Outcome::Protocol(self.read_protocol(fw)?))
            }
            Command::ReadAll { compat_dump, exit_pc_mode } => {
                self.dev.log.clear();
                let t = self.query(b'v', IDLE)?;
                self.absorb_v(&t);
                if self.state.class() != FwClass::Old
                    && matches!(self.state.mode, Mode::Standard | Mode::Online(_))
                {
                    log(format!("Device reports \"{}\" -> switching to PC mode ('P')", t.trim()));
                    self.query(b'P', IDLE)?;
                    let t = self.query(b'v', IDLE)?;
                    self.absorb_v(&t);
                }
                if self.state.mode != Mode::Pc {
                    bail!("Device is not in PC mode. For FW < 6.00 please press the PC button on the device.");
                }
                let fw = self.protocol_fw()?;
                log(format!("Firmware: {fw}"));

                if *compat_dump {
                    if fw >= 7.04 {
                        self.dev.command(b'h', Duration::from_millis(1500))?;
                        self.dev.command(b's', Duration::from_millis(1500))?;
                    }
                    if self.state.class() != FwClass::Old {
                        self.dev.command(b'c', Duration::from_millis(1500))?;
                    }
                }

                log("Reading log memory ('b') ...".into());
                let mut result = self.read_protocol(fw)?;
                log(format!("{} log entries decoded", result.records.len()));

                if self.state.class() != FwClass::Old {
                    if *exit_pc_mode {
                        self.query(b'X', Duration::from_millis(1000))?;
                        let t = self.query(b'v', IDLE)?;
                        self.absorb_v(&t);
                    }
                } else {
                    log("Note: for FW < 6.00 leave PC mode by pressing the radiation button on the device.".into());
                }
                result.session_log = self.dev.log.clone();
                Ok(Outcome::Protocol(result))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Log dump -> raw bytes
// ---------------------------------------------------------------------------

fn hex_bytes(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 || !s.is_ascii() {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// Returns: (log bytes, number of unreadable lines, number of checksum errors)
pub fn extract_protocol_bytes(text: &str, fw: f64) -> (Vec<u8>, usize, usize) {
    let mut out = Vec::new();
    let (mut bad_lines, mut bad_sums) = (0, 0);

    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        // The response to 'b' starts (from FW 7.x) with the header line "GAMMA-SCOUT Protokoll"
        if line.starts_with("GAMMA-SCOUT") {
            continue;
        }
        if fw < 6.0 {
            // "AAAA bb bb ... bb" (address + 16 bytes); the first 0x100 bytes are to be ignored
            let mut parts = line.split_whitespace();
            let addr = parts.next().and_then(|a| u16::from_str_radix(a, 16).ok());
            let bytes: Option<Vec<u8>> = parts.map(|p| u8::from_str_radix(p, 16).ok()).collect();
            match (addr, bytes) {
                (Some(a), Some(b)) if b.len() == 16 => {
                    if a >= 0x100 {
                        out.extend(b);
                    }
                }
                _ => bad_lines += 1,
            }
        } else {
            // 33 bytes as 66 hex characters; last byte = sum of the previous 32 mod 256
            match (line.len() == 66).then(|| hex_bytes(line)).flatten() {
                Some(b) => {
                    let sum = b[..32].iter().fold(0u8, |a, &x| a.wrapping_add(x));
                    if sum != b[32] {
                        bad_sums += 1;
                    }
                    out.extend_from_slice(&b[..32]);
                }
                None => bad_lines += 1,
            }
        }
    }
    (out, bad_lines, bad_sums)
}

// ---------------------------------------------------------------------------
// Decoding
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Layout {
    V1, // FW < 6.00
    V2, // 6.00 <= FW <= 6.016
    V3, // 6.017 < FW < 6.90
    V4, // FW >= 6.90 (documented from 7.01)
}

fn layout_for(fw: f64) -> Layout {
    if fw < 6.0 {
        Layout::V1
    } else if fw <= 6.016 + 1e-9 {
        Layout::V2
    } else if fw < 6.90 {
        Layout::V3
    } else {
        Layout::V4
    }
}

/// Week, 3 days, day, 12 h, 2 h, 1 h, 30 min, 10 min, 5 min, 2 min, 1 min, 30 s, 10 s
const INTERVALS: [u32; 13] = [
    604_800, 259_200, 86_400, 43_200, 7_200, 3_600, 1_800, 600, 300, 120, 60, 30, 10,
];
/// FW < 6.00: week, day, hour, 10 min, 1 min
const INTERVALS_V1: [u32; 5] = [604_800, 86_400, 3_600, 600, 60];

#[derive(Default)]
struct State {
    cursor: Option<NaiveDateTime>,
    interval_s: Option<u32>,
    oob_s: Option<u32>,
    flags: u8,
    conversion: Option<&'static str>,
}

fn bcd(b: u8) -> Option<u32> {
    let (hi, lo) = (b >> 4, b & 0x0f);
    (hi <= 9 && lo <= 9).then(|| (hi * 10 + lo) as u32)
}

/// BCD timestamp: 5 bytes `mmhhDDMMYY` or 6 bytes `ssmmhhDDMMYY`.
fn parse_ts(b: &[u8]) -> Option<NaiveDateTime> {
    let (sec, r) = match b.len() {
        6 => (bcd(b[0])?, &b[1..]),
        5 => (0, b),
        _ => return None,
    };
    let (min, hour, day, month, year) =
        (bcd(r[0])?, bcd(r[1])?, bcd(r[2])?, bcd(r[3])?, bcd(r[4])?);
    NaiveDate::from_ymd_opt(2000 + year as i32, month, day)?.and_hms_opt(hour, min, sec)
}

fn handle_ts(st: &mut State, data: &[u8], i: usize, n: usize) -> usize {
    match data.get(i + 1..i + 1 + n) {
        Some(b) => {
            if let Some(ts) = parse_ts(b) {
                st.cursor = Some(ts);
            }
            1 + n
        }
        None => data.len() - i,
    }
}

/// Out-of-band interval: 2 bytes duration (interpreted as little endian),
/// unit minutes (FW < 6.00) or 10 seconds (FW 6.00+).
fn handle_oob(st: &mut State, data: &[u8], i: usize, unit_s: u32) -> usize {
    match data.get(i + 1..i + 3) {
        Some(b) => {
            st.oob_s = Some(u16::from_le_bytes([b[0], b[1]]) as u32 * unit_s);
            3
        }
        None => data.len() - i,
    }
}

/// Generic special code 0xF5; `i` points to the event byte. Returns: bytes consumed starting at `i`.
fn handle_generic(st: &mut State, lay: Layout, data: &[u8], i: usize) -> usize {
    let ev = data[i];
    match (lay, ev) {
        (Layout::V3, 0x00..=0x0C) => {
            st.interval_s = Some(INTERVALS[ev as usize]);
            1
        }
        (Layout::V4, 0x00) => {
            st.interval_s = None; // logging stopped
            1
        }
        (Layout::V4, 0x01..=0x0D) => {
            st.interval_s = Some(INTERVALS[ev as usize - 1]);
            1
        }
        (_, 0xED) => handle_ts(st, data, i, 6),
        (_, 0xEF) => handle_ts(st, data, i, 5),
        (_, 0xEE) => handle_oob(st, data, i, 10),
        (Layout::V4, 0xEA) => {
            st.conversion = Some("Cs137");
            1
        }
        (Layout::V4, 0xEB) => {
            st.conversion = Some("Co60");
            1
        }
        _ => 1, // Ignore debug flags 0xF0-0xFE and anything unknown
    }
}

/// Unused memory is filled with 0xFF: cut off at the first longer 0xFF block.
fn find_end(data: &[u8]) -> usize {
    const RUN: usize = 8;
    data.windows(RUN)
        .position(|w| w.iter().all(|&b| b == 0xFF))
        .unwrap_or(data.len())
}

pub fn decode(data: &[u8], fw: f64) -> Vec<Record> {
    let lay = layout_for(fw);
    let data = &data[..find_end(data)];
    let mut st = State::default();
    let mut recs = Vec::new();
    let mut i = 0;

    while i < data.len() {
        let b = data[i];

        // Count entry: 2 bytes, upper 5 bits exponent, lower 11 bits mantissa
        if b >> 4 != 0xF {
            let Some(&b1) = data.get(i + 1) else { break };
            let exp = (b >> 3) as u32;
            let mant = (((b & 0x07) as u64) << 8) | b1 as u64;
            let duration = st.oob_s.take().or(st.interval_s);
            recs.push(Record {
                start: st.cursor,
                duration_s: duration,
                pulses: mant << exp,
                flags: std::mem::take(&mut st.flags),
                conversion: st.conversion,
            });
            if let (Some(c), Some(d)) = (st.cursor, duration) {
                st.cursor = Some(c + ChronoDuration::seconds(d as i64));
            }
            i += 2;
            continue;
        }

        let used = match lay {
            Layout::V1 => match b {
                0xF0..=0xF4 => {
                    st.interval_s = Some(INTERVALS_V1[(b - 0xF0) as usize]);
                    1
                }
                0xFC => {
                    st.flags |= FLAG_OVERFLOW;
                    1
                }
                0xFE => handle_ts(&mut st, data, i, 5),
                0xFF => handle_oob(&mut st, data, i, 60),
                _ => 1,
            },
            Layout::V2 => match b {
                0xF0..=0xFC => {
                    st.interval_s = Some(INTERVALS[(b - 0xF0) as usize]);
                    1
                }
                0xFD => {
                    st.flags |= FLAG_OVERFLOW;
                    1
                }
                0xFE => handle_ts(&mut st, data, i, 5),
                0xFF => handle_oob(&mut st, data, i, 10),
                _ => 1,
            },
            Layout::V3 => match b {
                0xFA => {
                    st.flags |= FLAG_OVERFLOW;
                    1
                }
                0xF5 if i + 1 < data.len() => 1 + handle_generic(&mut st, lay, data, i + 1),
                _ => 1,
            },
            Layout::V4 => match b {
                // Bits: 0 = overflow, 1 = dose alarm, 2 = dose rate alarm
                0xF9..=0xFF => {
                    st.flags |= b - 0xF8;
                    1
                }
                // Internal special code: following byte = number of bytes to skip (including the size byte)
                0xF8 => match data.get(i + 1) {
                    Some(&n) => 1 + n as usize,
                    None => 1,
                },
                0xF5 if i + 1 < data.len() => 1 + handle_generic(&mut st, lay, data, i + 1),
                _ => 1,
            },
        };
        i += used;
    }
    recs
}

/// Text of the 'b' response -> decoded result (shared by live reading and dump files).
pub fn build_result(
    text: &str,
    fw: f64,
    info: &str,
    baud: u32,
    used_bytes: Option<usize>,
) -> ReadResult {
    let mut warnings = Vec::new();
    let (bytes, bad_lines, bad_sums) = extract_protocol_bytes(text, fw);
    if bad_lines > 0 {
        warnings.push(format!("{bad_lines} line(s) could not be interpreted"));
    }
    if bad_sums > 0 {
        warnings.push(format!("{bad_sums} line(s) with wrong checksum"));
    }
    // Cross-check with the usage reported by 'v' (format verified on a device with FW 7.11 only)
    let end = find_end(&bytes);
    if let Some(u) = used_bytes {
        if fw >= 7.03 && u != end {
            warnings.push(format!(
                "According to 'v' {u} bytes are in use, the dump contained {end} bytes of \
                 data (maybe an interval was completed between 'v' and 'b')"
            ));
        }
    }
    ReadResult {
        records: decode(&bytes, fw),
        info: info.to_string(),
        fw,
        baud,
        session_log: Vec::new(),
        warnings,
    }
}

/// Reads a dump file (recording of the communication, as produced by the official toolboxes
/// or by `Save raw data`) and decodes the log part.
pub fn parse_dump(text: &str, fw_override: Option<f64>) -> Result<ReadResult> {
    let lines: Vec<&str> = text.lines().map(str::trim_end).collect();
    let info = lines
        .iter()
        .find(|l| l.trim_start().starts_with("Version"))
        .map(|l| l.trim().to_string())
        .unwrap_or_default();
    let vi = parse_v_info(&info);
    let fw = fw_override
        .or(vi.fw)
        .or_else(|| parse_fw(&info))
        .context("Firmware version not found in the dump – please enter it in the connection settings")?;

    // Section between the echo "b" and the echo "X"
    let start = lines.iter().position(|l| l.trim() == "b").map(|i| i + 1).unwrap_or(0);
    let end = lines[start..]
        .iter()
        .position(|l| l.trim() == "X")
        .map(|i| start + i)
        .unwrap_or(lines.len());
    let section = lines[start..end].join("\n");

    let baud = if fw < 6.0 {
        2400
    } else if fw < 6.90 {
        9600
    } else {
        460_800
    };
    let mut res = build_result(&section, fw, &info, baud, vi.used_bytes);
    if res.records.is_empty() {
        bail!("No log data found in the dump");
    }
    res.session_log = text.as_bytes().to_vec();
    Ok(res)
}

// ---------------------------------------------------------------------------
// Averaging (minute / hour / day / week / month)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AvgUnit {
    /// no averaging, original values
    Off,
    Minute,
    Hour,
    Day,
    Week,
    Month,
}

impl AvgUnit {
    pub const ALL: [AvgUnit; 6] = [
        AvgUnit::Off,
        AvgUnit::Minute,
        AvgUnit::Hour,
        AvgUnit::Day,
        AvgUnit::Week,
        AvgUnit::Month,
    ];

    pub fn label(self) -> &'static str {
        match self {
            AvgUnit::Off => "Original (none)",
            AvgUnit::Minute => "Minute",
            AvgUnit::Hour => "Hour",
            AvgUnit::Day => "Day",
            AvgUnit::Week => "Week",
            AvgUnit::Month => "Month",
        }
    }

    pub fn file_tag(self) -> &'static str {
        match self {
            AvgUnit::Off => "original",
            AvgUnit::Minute => "minute",
            AvgUnit::Hour => "hour",
            AvgUnit::Day => "day",
            AvgUnit::Week => "week",
            AvgUnit::Month => "month",
        }
    }

    /// Nominal length; entries that are at least this long are not split any further.
    fn nominal_secs(self) -> f64 {
        match self {
            AvgUnit::Off => 0.0,
            AvgUnit::Minute => 60.0,
            AvgUnit::Hour => 3_600.0,
            AvgUnit::Day => 86_400.0,
            AvgUnit::Week => 604_800.0,
            AvgUnit::Month => 2_592_000.0, // 30 days
        }
    }
}

/// One averaged point: counts and covered time in the window (day, week, ...).
#[derive(Clone, Debug)]
pub struct AvgPoint {
    /// first covered point in time in the window
    pub start: NaiveDateTime,
    /// last covered point in time in the window
    pub end: NaiveDateTime,
    /// seconds actually covered (less than the window for gaps or edge windows)
    pub duration_s: f64,
    pub pulses: f64,
    pub cpm: f64,
}

fn bucket_start(t: NaiveDateTime, u: AvgUnit) -> NaiveDateTime {
    use chrono::{Datelike, Timelike};
    let d = t.date();
    match u {
        AvgUnit::Off => t,
        AvgUnit::Minute => d.and_hms_opt(t.hour(), t.minute(), 0).unwrap(),
        AvgUnit::Hour => d.and_hms_opt(t.hour(), 0, 0).unwrap(),
        AvgUnit::Day => d.and_hms_opt(0, 0, 0).unwrap(),
        // Week starts on Monday (ISO)
        AvgUnit::Week => (d - ChronoDuration::days(d.weekday().num_days_from_monday() as i64))
            .and_hms_opt(0, 0, 0)
            .unwrap(),
        AvgUnit::Month => NaiveDate::from_ymd_opt(d.year(), d.month(), 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap(),
    }
}

fn next_bucket(b: NaiveDateTime, u: AvgUnit) -> NaiveDateTime {
    use chrono::Datelike;
    match u {
        AvgUnit::Off => b,
        AvgUnit::Minute => b + ChronoDuration::minutes(1),
        AvgUnit::Hour => b + ChronoDuration::hours(1),
        AvgUnit::Day => b + ChronoDuration::days(1),
        AvgUnit::Week => b + ChronoDuration::days(7),
        AvgUnit::Month => {
            let (y, m) = if b.month() == 12 { (b.year() + 1, 1) } else { (b.year(), b.month() + 1) };
            NaiveDate::from_ymd_opt(y, m, 1).unwrap().and_hms_opt(0, 0, 0).unwrap()
        }
    }
}

/// Time-weighted mean (counts ÷ covered time) per minute/hour/day/week/month.
///
/// * An entry that spans a time window is distributed proportionally (by time share) over the
///   windows.
/// * Entries that are at least as long as the chosen unit (e.g. weekly values with
///   "Day") are taken over unchanged – the resolution cannot be refined.
/// * Entries without a timestamp or duration are skipped.
pub fn average(recs: &[Record], unit: AvgUnit) -> Vec<AvgPoint> {
    // Window -> (counts, seconds, first, last covered point in time)
    let mut map: BTreeMap<NaiveDateTime, (f64, f64, NaiveDateTime, NaiveDateTime)> = BTreeMap::new();
    let mut out: Vec<AvgPoint> = Vec::new();

    for r in recs {
        let (Some(start), Some(dur)) = (r.start, r.duration_s) else { continue };
        if dur == 0 {
            continue;
        }
        let dur_f = dur as f64;
        let end = start + ChronoDuration::seconds(dur as i64);

        if unit == AvgUnit::Off || dur_f >= unit.nominal_secs() {
            out.push(AvgPoint {
                start,
                end,
                duration_s: dur_f,
                pulses: r.pulses as f64,
                cpm: r.pulses as f64 * 60.0 / dur_f,
            });
            continue;
        }

        let mut cur = start;
        while cur < end {
            let b = bucket_start(cur, unit);
            let seg_end = next_bucket(b, unit).min(end);
            let seg = (seg_end - cur).num_seconds() as f64;
            let e = map.entry(b).or_insert((0.0, 0.0, cur, seg_end));
            e.0 += r.pulses as f64 * seg / dur_f;
            e.1 += seg;
            e.2 = e.2.min(cur);
            e.3 = e.3.max(seg_end);
            cur = seg_end;
        }
    }

    for (_, (pulses, secs, first, last)) in map {
        if secs > 0.0 {
            out.push(AvgPoint {
                start: first,
                end: last,
                duration_s: secs,
                pulses,
                cpm: pulses * 60.0 / secs,
            });
        }
    }
    out.sort_by_key(|p| p.start);
    out
}

pub fn write_avg_csv(out: &mut dyn Write, pts: &[AvgPoint]) -> Result<()> {
    writeln!(out, "start,end,covered_s,pulses,cpm")?;
    for p in pts {
        writeln!(
            out,
            "{},{},{:.0},{:.2},{:.3}",
            p.start.format("%Y-%m-%d %H:%M:%S"),
            p.end.format("%Y-%m-%d %H:%M:%S"),
            p.duration_s,
            p.pulses,
            p.cpm
        )?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// CSV
// ---------------------------------------------------------------------------

pub fn write_csv(out: &mut dyn Write, recs: &[Record]) -> Result<()> {
    writeln!(
        out,
        "start,duration_s,pulses,cpm,dose_rate_overflow,dose_alarm,dose_rate_alarm,conversion"
    )?;
    for r in recs {
        let start = r.start.map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string()).unwrap_or_default();
        let dur = r.duration_s.map(|d| d.to_string()).unwrap_or_default();
        let cpm = r.cpm().map(|c| format!("{c:.2}")).unwrap_or_default();
        writeln!(
            out,
            "{start},{dur},{},{cpm},{},{},{},{}",
            r.pulses,
            (r.flags & FLAG_OVERFLOW != 0) as u8,
            (r.flags & FLAG_DOSE_ALARM != 0) as u8,
            (r.flags & FLAG_RATE_ALARM != 0) as u8,
            r.conversion.unwrap_or("")
        )?;
    }
    Ok(())
}

/// Reads a file produced by `write_csv` back in.
pub fn read_csv(text: &str) -> Result<Vec<Record>> {
    if text.lines().next().is_some_and(|h| h.trim_start().starts_with("start,end,")) {
        bail!(
            "This is an averaged CSV (start,end,covered_s,...). Only CSV files with log entries \
             (start,duration_s,pulses,...) can be loaded."
        );
    }
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate().skip(1) {
        if line.trim().is_empty() {
            continue;
        }
        let f: Vec<&str> = line.split(',').map(str::trim).collect();
        if f.len() < 8 {
            bail!("Line {}: too few columns", n + 1);
        }
        let start = if f[0].is_empty() {
            None
        } else {
            Some(
                NaiveDateTime::parse_from_str(f[0], "%Y-%m-%d %H:%M:%S")
                    .with_context(|| format!("Line {}: invalid timestamp", n + 1))?,
            )
        };
        let duration_s = if f[1].is_empty() {
            None
        } else {
            Some(f[1].parse::<u32>().with_context(|| format!("Line {}: invalid duration", n + 1))?)
        };
        let pulses = f[2]
            .parse::<u64>()
            .with_context(|| format!("Line {}: invalid count", n + 1))?;
        let mut flags = 0;
        if f[4] == "1" {
            flags |= FLAG_OVERFLOW;
        }
        if f[5] == "1" {
            flags |= FLAG_DOSE_ALARM;
        }
        if f[6] == "1" {
            flags |= FLAG_RATE_ALARM;
        }
        let conversion = match f[7] {
            "Cs137" => Some("Cs137"),
            "Co60" => Some("Co60"),
            _ => None,
        };
        out.push(Record { start, duration_s, pulses, flags, conversion });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Tests (sample data from the interface description)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mantissa_exponent_example() {
        // 0x3E27 = 2^7 * 1575 = 201600
        let recs = decode(&[0x3E, 0x27], 7.2);
        assert_eq!(recs[0].pulses, 201_600);
    }

    #[test]
    fn fw6_example_line() {
        let line = "f5ef5915120114f500f5ee05000013f5ef5915120114f505f5eec0000290f50a10";
        let (bytes, bad_lines, bad_sums) = extract_protocol_bytes(line, 6.10);
        assert_eq!((bad_lines, bad_sums), (0, 0));
        assert_eq!(bytes.len(), 32);
        assert_eq!(bytes[0], 0xF5);
    }

    #[test]
    fn fw6_timestamp_and_interval() {
        let data = [0xF5, 0xEF, 0x59, 0x15, 0x12, 0x01, 0x14, 0xF5, 0x0A, 0x00, 0x13, 0x00, 0x18];
        let recs = decode(&data, 6.10);
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0].pulses, 19);
        assert_eq!(recs[0].duration_s, Some(60));
        assert_eq!(recs[1].pulses, 24);
        assert_eq!(recs[1].start.unwrap().format("%H:%M").to_string(), "16:00");
    }

    #[test]
    fn old_fw_dump_ignores_first_256_bytes() {
        let text = " 0000 92 94 03 ff ff ff ff ff ff ff ff ff ff ff ff ff\n \
                     0100 fe 55 19 12 07 10 f0 ff 2d 03 1e c1 f4 00 1e 00";
        let (bytes, _, _) = extract_protocol_bytes(text, 5.0);
        assert_eq!(bytes.len(), 16);
        assert_eq!(bytes[0], 0xFE);
    }

    fn state(baud: u32, fw: Option<f64>, mode: Mode) -> DeviceState {
        DeviceState {
            baud,
            fw,
            mode,
            info: String::new(),
            serial: None,
            used_bytes: None,
            device_time: None,
        }
    }

    #[test]
    fn availability_old_fw() {
        let st = state(2400, Some(5.43), Mode::Pc);
        assert!(Command::DumpProtocol.is_available(&st));
        assert!(Command::ResetPointer.is_available(&st));
        assert!(Command::AlertQuery.is_available(&st));
        assert!(Command::OnlineInterval(0).is_available(&st)); // FW 5.43 only
        assert!(!Command::EnterPc.is_available(&st));
        assert!(!Command::DumpConversion.is_available(&st));
        assert!(!Command::WarmStart.is_available(&st));
        let st = state(2400, Some(5.36), Mode::Pc);
        assert!(!Command::OnlineInterval(0).is_available(&st));
        let st = state(2400, Some(3.0), Mode::Pc);
        assert!(!Command::AlertQuery.is_available(&st)); // only from 3.03
    }

    #[test]
    fn availability_fw6_modes() {
        let std = state(9600, None, Mode::Standard);
        assert!(Command::EnterPc.is_available(&std));
        assert!(Command::OnlineDose.is_available(&std));
        assert!(!Command::DumpProtocol.is_available(&std));
        let pc = state(9600, Some(6.10), Mode::Pc);
        assert!(Command::ColdStart.is_available(&pc));
        assert!(!Command::WarmStart.is_available(&pc));
        assert!(Command::DumpConversion.is_available(&pc));
        assert!(!Command::DumpH.is_available(&pc));
        assert!(Command::ExitMode.is_available(&pc));
    }

    #[test]
    fn availability_new_fw() {
        let pc = state(460_800, Some(7.03), Mode::Pc);
        assert!(Command::WarmStart.is_available(&pc));
        assert!(!Command::ColdStart.is_available(&pc));
        assert!(!Command::DumpH.is_available(&pc)); // only from 7.04
        let pc = state(460_800, Some(7.2), Mode::Pc);
        assert!(Command::DumpH.is_available(&pc));
        assert!(Command::DumpS.is_available(&pc));
        let on = state(460_800, None, Mode::Online("Online 1".into()));
        assert!(Command::OnlineStatus.is_available(&on));
        assert!(Command::EnterPc.is_available(&on));
        assert!(!Command::OnlineStatus.is_available(&state(460_800, None, Mode::Standard)));
    }

    #[test]
    fn v_response_fw711() {
        // Real response of a device with FW 7.11
        let vi = parse_v_info("Version 7.11Lb00 080518 fca6 01.11.26 17:14:14");
        assert_eq!(vi.fw, Some(7.11));
        assert_eq!(vi.fw_suffix, "Lb00");
        assert_eq!(vi.serial.as_deref(), Some("080518"));
        assert_eq!(vi.used_bytes, Some(0xfca6));
        assert_eq!(
            vi.device_time.unwrap().format("%Y-%m-%d %H:%M:%S").to_string(),
            "2026-11-01 17:14:14"
        );
    }

    #[test]
    fn real_dump_head_fw711() {
        // Start of a real dump (FW 7.11): timestamp (6 bytes), 12 h, Co60, 10 min, 9 entries
        let text = "v\r\nStandard\r\nP\r\nPC-Mode gestartet\r\nv\r\n\
            Version 7.11Lb00 080518 fca6 01.11.26 17:14:14\r\nb\r\nGAMMA-SCOUT Protokoll\r\n\
            f5ed202318110124f504f5ebf5080068007200960089007c0084008000840080c6\r\n\
            X\r\nPC-Mode beendet\r\nv\r\nStandard\r\n";
        let res = parse_dump(text, None).unwrap();
        assert_eq!(res.fw, 7.11);
        assert!(res.warnings.iter().all(|w| !w.contains("interpreted") && !w.contains("checksum")));
        assert_eq!(res.records.len(), 9);
        let r0 = &res.records[0];
        assert_eq!(r0.pulses, 104);
        assert_eq!(r0.duration_s, Some(600));
        assert_eq!(r0.conversion, Some("Co60"));
        assert_eq!(r0.start.unwrap().format("%Y-%m-%d %H:%M:%S").to_string(), "2024-01-11 18:23:20");
        assert_eq!(res.records[1].start.unwrap().format("%H:%M:%S").to_string(), "18:33:20");
    }

    #[test]
    fn weekly_entry_from_real_dump() {
        // F5 01 = 1 week (FW >= 7.01); 0x3794 = 1940 << 6 = 124160 counts ≈ 12.3 cpm
        let recs = decode(&[0xF5, 0x01, 0x37, 0x94], 7.11);
        assert_eq!(recs[0].pulses, 124_160);
        assert_eq!(recs[0].duration_s, Some(604_800));
        assert!((recs[0].cpm().unwrap() - 12.3).abs() < 0.1);
    }

    fn dt(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").unwrap()
    }

    fn rec(start: &str, dur: u32, pulses: u64) -> Record {
        Record { start: Some(dt(start)), duration_s: Some(dur), pulses, flags: 0, conversion: None }
    }

    #[test]
    fn average_hourly_merges_ten_minute_records() {
        let recs: Vec<Record> = (0..6)
            .map(|i| rec(&format!("2024-01-11 18:{:02}:00", i * 10), 600, 10))
            .collect();
        let a = average(&recs, AvgUnit::Hour);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].start, dt("2024-01-11 18:00:00"));
        assert!((a[0].pulses - 60.0).abs() < 1e-9);
        assert!((a[0].cpm - 1.0).abs() < 1e-9); // 60 counts in 3600 s = 1 cpm
    }

    #[test]
    fn average_splits_record_across_hour_boundary() {
        let a = average(&[rec("2024-01-11 18:55:00", 600, 100)], AvgUnit::Hour);
        assert_eq!(a.len(), 2);
        assert!((a[0].pulses - 50.0).abs() < 1e-9);
        assert!((a[0].duration_s - 300.0).abs() < 1e-9);
        assert_eq!(a[1].start, dt("2024-01-11 19:00:00"));
        assert!((a[1].pulses - 50.0).abs() < 1e-9);
    }

    #[test]
    fn average_weekly_record_into_months() {
        // Mon 2024-01-29 + 7 days: 3 days in January, 4 days in February
        let a = average(&[rec("2024-01-29 00:00:00", 604_800, 7000)], AvgUnit::Month);
        assert_eq!(a.len(), 2);
        assert!((a[0].pulses - 3000.0).abs() < 1e-6);
        assert!((a[1].pulses - 4000.0).abs() < 1e-6);
    }

    #[test]
    fn average_keeps_records_longer_than_unit() {
        let a = average(&[rec("2024-01-29 00:00:00", 604_800, 7000)], AvgUnit::Day);
        assert_eq!(a.len(), 1);
        assert!((a[0].duration_s - 604_800.0).abs() < 1e-9);
    }

    #[test]
    fn average_week_starts_on_monday() {
        // Thu 2024-01-11: week window starts on Mon 2024-01-08
        let a = average(&[rec("2024-01-11 18:00:00", 600, 10)], AvgUnit::Week);
        assert_eq!(a.len(), 1);
        assert_eq!(bucket_start(dt("2024-01-11 18:00:00"), AvgUnit::Week), dt("2024-01-08 00:00:00"));
    }

    #[test]
    fn csv_roundtrip() {
        let data = [0xF5, 0xEF, 0x59, 0x15, 0x12, 0x01, 0x14, 0xF5, 0x0A, 0x00, 0x13, 0x00, 0x18];
        let recs = decode(&data, 6.10);
        let mut buf = Vec::new();
        write_csv(&mut buf, &recs).unwrap();
        let back = read_csv(&String::from_utf8(buf).unwrap()).unwrap();
        assert_eq!(back.len(), recs.len());
        assert_eq!(back[1].pulses, 24);
        assert_eq!(back[1].start, recs[1].start);
    }
}
