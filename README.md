# Gamma-Scout Reader

A desktop application written in Rust ([egui](https://github.com/emilk/egui)) that talks to a
**Gamma-Scout** radiation counter over its (virtual) COM port, reads the log memory, and shows the
data in a zoomable graph. Readings can be saved as CSV, and recorded dump files can be loaded and
analysed without a device.

The communication follows the manufacturer's *Communication Interface V1.12*
(<https://www.gamma-scout.com/de/schnittstellenbeschreibung/>).

> **Unofficial project.** This software is not affiliated with or endorsed by
> Gamma-Scout GmbH & Co. KG. Use it at your own risk, in particular the commands marked
> as dangerous (see [Safety notes](#safety-notes)).

---

## Table of contents

1. [Features](#features)
2. [Requirements](#requirements)
3. [Build and run](#build-and-run)
4. [Quick start](#quick-start)
5. [User guide](#user-guide)
6. [Serial protocol summary](#serial-protocol-summary)
7. [File formats](#file-formats)
8. [Architecture](#architecture)
9. [Tests](#tests)
10. [Verification status and known limitations](#verification-status-and-known-limitations)
11. [Safety notes](#safety-notes)
12. [Troubleshooting](#troubleshooting)
13. [Dependencies](#dependencies)
14. [License and credits](#license-and-credits)

---

## Features

- **Connect** over a serial/COM port with automatic baud-rate detection (460800, 9600 or 2400 baud,
  7E1) and automatic recognition of the device mode (Standard, PC, Online) and firmware version.
- **Port fallback:** if connecting fails, the remaining serial ports are tried one after another
  (optional).
- **Read the log memory** with one click (switches to PC mode, reads, leaves PC mode again).
- **All commands from the interface description**, enabled only if they are valid for the detected
  firmware and the current mode (`v P X O R D s 0-9 b c h z t u d i N`).
- **Console** showing the raw responses of the device.
- **Zoomable graph** (pan, wheel zoom, box zoom, reset) with counts per minute (cpm) or counts per
  interval on a real time axis. Alarm/overflow entries are highlighted.
- **Averaging** per minute, hour, day, week or month (time-weighted, mixed log intervals are handled
  correctly).
- **Save and load:** CSV (raw entries), CSV (averaged), raw communication dump; load CSV files and
  dump files without a device.
- **Set date and time** on the device (`t`, or `u` + `d` on old firmware), with a button to take over
  the PC time.
- Handles all documented log layouts: firmware < 6.00, 6.00–6.016, 6.017–6.89 and 6.90/7.x.

## Requirements

- A recent stable **Rust toolchain** (edition 2021). Install it from <https://rustup.rs>.
- A Gamma-Scout with USB/serial interface and its virtual COM port driver (FTDI). Linux and macOS
  usually need no extra driver.
- On Linux the usual windowing/OpenGL libraries for desktop applications (X11 or Wayland,
  `libxkbcommon`, OpenGL) are required by `eframe`.
- On Linux, access to the serial device (see [Troubleshooting](#troubleshooting)).
- On Linux, native file dialogs (`rfd`) use the XDG desktop portal by default; install
  `xdg-desktop-portal` and a backend for your desktop environment if the *Save*/*Load* dialogs do
  not appear.

## Build and run

```bash
cd gamma-scout-gui
cargo run --release        # build and start the GUI
cargo test                 # run the unit tests
```

The expected layout is:

```
gamma-scout-gui/
├── Cargo.toml
├── README.md
└── src/
    ├── main.rs      # GUI, background worker thread
    └── gamma.rs     # protocol, session, decoding, averaging, CSV
```

The program takes **no command-line arguments**; everything is configured in the GUI.

## Quick start

1. Connect the Gamma-Scout with the USB cable.
2. Start the program (`cargo run --release`).
3. Select the **port** (`COM3`, `/dev/ttyUSB0`, `/dev/cu.usbserial-…`) or leave the suggested one.
   Leave the baud rate on *Automatic*.
4. **Firmware < 6.00 only:** press the **PC button** on the device first.
5. Click **🔌 Connect**.
6. Click **📡 Read log**. The graph appears when reading has finished.
7. Zoom with <kbd>Ctrl</kbd> + mouse wheel, pan by dragging; choose **Average per** for a smoother
   curve.
8. **💾 Save CSV** to keep the data.

---

## User guide

### Connection

| Control | Description |
|---|---|
| **Port** | Text field plus a drop-down (▼) listing the detected serial ports. **⟳** rescans. |
| **Baud rate** | *Automatic* tries 460800, 9600 and 2400 baud (7E1) by sending `v` and checking for a readable answer. You can force a rate. |
| **Firmware (optional)** | Overrides the firmware version. Useful if the response to `v` cannot be parsed, or when loading dumps without a version line. |
| **Try other ports on failure** | If connecting fails, the other serial ports are probed one after another. See the note below. |
| **Connect / Disconnect** | Opens or closes the connection. The connection stays open, so commands can be sent one by one. |
| **Cancel** | Aborts the current operation (connection attempt, reading, ...). |

After connecting, the panel shows the **mode** (Standard / PC / Online), the **firmware**, the baud
rate class, and — in PC mode on recent firmware — the **serial number**, **memory in use** and the
**device time** at the last `v` command.

> **Port fallback note:** while probing other ports, the character `v` is sent to each port at 460800,
> 9600 and 2400 baud (7E1). This is harmless for the Gamma-Scout but may confuse other serial
> devices. Disable the option if unrelated devices are attached. Probing takes up to about
> 6 seconds per port. The port that finally answered is shown in the port field.

### Reading the log

**📡 Read log** performs the complete documented sequence:

```
v            query the mode
P            switch to PC mode (FW 6.00+, if the device is in Standard/Online mode)
v            read firmware, serial number, memory usage, date/time
[h] [s] [c]  only if "Also query extra data for the dump" is ticked (h/s: FW 7.04+, c: FW 6.00+)
b            read the log memory
X            leave PC mode (FW 6.00+, if "Leave PC mode afterwards" is ticked)
v            confirm the new mode
```

For firmware **< 6.00** the PC mode cannot be switched remotely: press the PC button on the device
before connecting, and press the radiation button afterwards to leave PC mode.

### Command panel

Every documented command has a button. A button is enabled only if the command is valid for the
detected firmware and the current mode; hover over it for a description (also for disabled buttons).

| Command | Meaning | Available |
|---|---|---|
| `v` | Version, or the current mode (PC mode: FW, serial no., used bytes, date/time) | always |
| `P` | Switch to PC mode | Standard or Online mode, FW 6.00+ |
| `X` | Leave PC mode or Online mode | PC or Online mode, FW 6.00+ |
| `O` / `R` / `D` | Online modes: classic / dose rate / dose | Standard mode, FW 6.00+, online devices only |
| `s` (online) | Query online status and counts | Online mode (old firmware: always enabled) |
| `0` … `9` | Set online interval (`0` on FW < 6.00 only for FW 5.43) | Online mode (old firmware: always enabled) |
| `b` | Read the log memory and show it in the graph | PC mode |
| `z` | Reset the log pointer to the start of memory (**asks for confirmation**) | PC mode |
| `c` | Internal conversion data | PC mode, FW 6.00+ |
| `h` | Internal data | PC mode, FW 7.04+ |
| `s` (PC) | Internal data | PC mode, FW 7.04+ |
| `t` (query) | `"AN"` for Alert devices, otherwise `"AUS"` | PC mode, FW 3.03 – 5.99 |
| `t` / `u` + `d` | Set date and time (see below) | PC mode |
| `i` | Cold start / reset (**dangerous, asks for confirmation**) | PC mode, FW 6.00 – 6.89 |
| `N` | Warm start (**asks for confirmation**) | PC mode, FW 6.90+ |

The firmware class is derived from the baud rate (2400 → < 6.00, 9600 → 6.00–6.89, 460800 → 6.90+).
The exact version becomes known after `v` in PC mode. Without it, commands that need an exact
version (`h`, `s` in PC mode) stay disabled.

**Set date and time:** enter the date as `YYYY-MM-DD` and the time as `hh:mm:ss` (or click
**PC time**), then click **Set**. Only the years 2000–2099 are supported because the device stores
two digits.

- FW < 6.00: `u` + `hhmm`, then `d` + `DDMMYY`, with at least 550 ms pause after every character.
- FW 6.00+: `t` + `DDMMYYhhmmss`, parameter characters with a 2 ms gap. The program then asks `v`
  again and shows the device time in the console.

**Online mode:** the format of the answers to `s` is not described in the documentation, so the
console shows the raw text. In online mode (FW 6.00+) there is also an optional **auto-poll** that
sends `s` every N seconds.

### Console

The console at the bottom is the single place for all messages. It lists every command you send
(`›`) with the raw response of the device, and also all status and progress messages: connecting,
baud-rate detection, port fallback, device information, warnings, saving and loading results.
Errors are marked with `✖`. There is no separate status line in the left panel. Very long answers
(e.g. `c`) are truncated for display only. **Clear** empties the console.

### Graph

- **Axes:** time (device time, no time-zone conversion) and counts per minute (cpm) or counts per
  interval. If entries have no timestamp, the entry number is used as X axis.
- **cpm** is the plain count rate (counts ÷ interval × 60). It is **not** converted to µSv/h.
- Entries with an overflow or alarm flag are marked in red (original view only).
- With more than 5000 points the individual markers are hidden for speed.

| Action | Result |
|---|---|
| Drag | Pan |
| <kbd>Ctrl</kbd> + mouse wheel | Zoom |
| Right-click and drag | Zoom to the selected area |
| Double-click | Reset the view |
| **Reset view** button | Reset the view |

The time-axis labels adapt to the zoom level (seconds, minutes/hours, days).

### Averaging

*Average per* offers **Original (none), Minute, Hour, Day, Week, Month**. It is available for the cpm
view when every entry has a timestamp and a duration.

- The value of a window is the **time-weighted mean**: total counts ÷ covered time × 60.
- An entry that spans a window boundary is **split proportionally** by time. The total number of
  counts is preserved.
- Entries that are **at least as long as the chosen unit** are left unchanged (for example weekly
  entries with *Day*), because the resolution cannot be refined. Weekly entries are distributed over
  the months with *Month*.
- Partially covered windows (start/end of the log, gaps) are averaged over the time actually covered.
- Weeks start on **Monday**; months are calendar months.
- The averaged point is drawn in the middle of the covered period.
- **💾 CSV (averaged)** saves exactly the points shown.

### Saving and loading

| Button | Action |
|---|---|
| **💾 Save CSV** | Saves all decoded entries (see [CSV](#csv-entries)). |
| **💾 CSV (averaged)** | Saves the currently displayed averaged values. |
| **📂 Load CSV** | Loads a CSV written by this program. |
| **📂 Load dump** | Reads a recorded dump file and decodes its log section; no device needed. If the file has no version line, enter the firmware first. |
| **Save raw data** | Saves the complete communication of the session as a dump file. |

The statistics panel shows the number of entries, total counts, average and maximum cpm, the covered
time range and the number of entries with alarm/overflow.

---

## Serial protocol summary

### Port settings

| Firmware | Baud rate | Format |
|---|---|---|
| < 6.00 | 2400 | 7 data bits, even parity, 1 stop bit (7E1), no flow control |
| 6.00 – 6.89 | 9600 | 7E1 |
| 6.90 and newer (documented from 7.01) | 460800 | 7E1 |

### Timing

After every command character at least **550 ms** must pass before the next one; the program waits
600 ms. For commands with a parameter block, firmware < 6.00 needs the same pause after every
character, firmware 6.00+ accepts the parameter characters quickly (the program uses 2 ms). Commands
are single characters without a terminator; the device does not echo.

### Modes

- **Standard:** normal operation. `v` answers `Standard`.
- **PC mode:** needed for reading the log memory. Entered with `P` (FW 6.00+) or the PC button (all
  firmware).
- **Online mode:** for online devices; entered with `O`, `R` or `D` (FW 6.00+).

### Log memory format

The response to `b` depends on the firmware:

- **FW 6.00+:** (FW 7.x: preceded by the header line `GAMMA-SCOUT Protokoll`) lines of **66 hex
  characters** = 33 bytes: 32 data bytes plus a checksum (sum of the 32 bytes modulo 256).
- **FW < 6.00:** lines of an address and 16 hex bytes (`AAAA bb bb … bb`). The first `0x100` bytes
  are not log data and are ignored.

The unused part of the memory is filled with `0xFF`; the data ends at the first long `0xFF` run. From
firmware 7.03 the `v` response reports the number of used bytes, which the program cross-checks
(verified on a FW 7.11 device).

**Count entry** (2 bytes, first nibble `0x0`–`0xE`): 5-bit exponent `e` and 11-bit mantissa `m`;
counts = `m << e`. Example: `0x3E27` → `1575 << 7 = 201600`. Bytes with the high nibble `0xF` are special
codes. The interval is the one most recently set; a timestamp sets the time of the **following**
interval, which is then advanced entry by entry.

**Special codes** (per firmware layout):

| Firmware | Interval codes | Timestamp | Out-of-band interval | Overflow / alarms |
|---|---|---|---|---|
| < 6.00 | `F0`–`F4`: week, day, hour, 10 min, 1 min | `FE` + 5 bytes | `FF` + 2 bytes (minutes) | `FC` = overflow |
| 6.00 – 6.016 | `F0`–`FC`: week … 10 s | `FE` + 5 bytes | `FF` + 2 bytes (10 s units) | `FD` = overflow |
| 6.017 – 6.89 | `F5 00`–`F5 0C` | `F5 ED` + 6 bytes, `F5 EF` + 5 bytes | `F5 EE` + 2 bytes (10 s) | `FA` = overflow |
| 6.90+ (docs from 7.01) | `F5 00` = stopped, `F5 01`–`F5 0D` | `F5 ED` / `F5 EF` | `F5 EE` + 2 bytes (10 s) | `F9`–`FF` = flag bits (1 overflow, 2 dose alarm, 4 dose-rate alarm); `F8 n` = skip; `F5 EA`/`F5 EB` = conversion set Cs-137 / Co-60 |

The intervals (codes `1`–`13` on 6.90+) are: week, 3 days, day, 12 h, 2 h, 1 h, 30 min, 10 min,
5 min, 2 min, 1 min, 30 s, 10 s. Timestamps are BCD: `mmhhDDMMYY` (5 bytes) or `ssmmhhDDMMYY`
(6 bytes). Debug flags `F5 F0`–`F5 FE` are ignored.

---

## File formats

### CSV (entries)

One row per log entry:

```
start,duration_s,pulses,cpm,dose_rate_overflow,dose_alarm,dose_rate_alarm,conversion
2024-01-11 18:23:20,600,104,10.40,0,0,0,Co60
2024-01-11 18:33:20,600,114,11.40,0,0,0,Co60
```

- `start`: start of the interval in device time (empty if unknown).
- `pulses`: counts in the interval; `cpm`: counts per minute.
- Flags are `0`/`1`. `conversion` is `Cs137`, `Co60` or empty.

### CSV (averaged)

```
start,end,covered_s,pulses,cpm
2024-01-11 18:23:20,2024-01-11 19:00:00,2200,459.33,12.527
```

`start`/`end` are the first and last covered point in the window, `covered_s` the covered seconds,
`pulses` the (possibly fractional) counts, `cpm` the time-weighted mean.

### Raw dump

The complete communication in chronological order. Every command character is written on its own
line, followed by the device response. Example (serial number replaced):

```
v
Standard
P
PC-Mode gestartet
v
Version 7.11Lb00 123456 fca6 01.11.26 17:14:14
b
GAMMA-SCOUT Protokoll
f5ed202318110124f504f5ebf5080068007200960089007c0084008000840080c6
…
X
PC-Mode beendet
v
Standard
```

The `v` line in PC mode (observed on FW 7.11) contains: firmware + suffix, serial number, used log
bytes (hex), date (`DD.MM.YY`), time. Device responses are in German as sent by the device.

---

## Architecture

- **`src/gamma.rs`** — everything that does not need a GUI:
  `Device` (serial port with timing rules), `Session` (connection + `DeviceState`), `Command`
  (`is_available()` per firmware/mode, `warning()`), `decode()` (all four log layouts),
  `extract_protocol_bytes()`, `build_result()` / `parse_dump()`, `parse_v_info()`, `average()`,
  CSV reading/writing.
- **`src/main.rs`** — the egui application. A **worker thread** owns the serial connection and
  processes requests (`Req::Cmd`, `Req::Disconnect`) one after another; it reports back with
  events (`Log`, `Connected`, `State`, `Text`, `Protocol`, `Error`, `Idle`, `Disconnected`) over
  `mpsc` channels and wakes the UI with `request_repaint()`. The GUI thread never blocks on the
  serial port.

## Tests

```bash
cargo test
```

The unit tests cover the mantissa/exponent coding, the example lines from the interface
description, timestamps and intervals, old-firmware dump parsing, the command availability matrix
for old, 6.x and 7.x firmware, the `v` response of a real FW 7.11 device, the beginning of a real
dump, a weekly entry from a real dump, the averaging (hour merge, boundary split, weeks into months,
long entries, Monday-based weeks) and the CSV round trip.

## Verification status and known limitations

**Verified against real data:** a complete dump of a device with **firmware 7.11** (about 32,300
entries, 10-minute and weekly intervals). Checksums, decoding to the last byte, count scaling, time
semantics, `v` response format, the `fca6` used-bytes field and the command sequence
`v, P, v, h, s, c, b, X, v` match the implementation.

**Verified by build and emulation:** the project builds without warnings and all unit tests pass
(Rust 1.99.1). The complete serial session (baud-rate detection, `P`, `v`, `h`, `s`, `c`, `b`, `X`,
setting the time, rejection of unavailable commands, a silent port) and all GUI states were
additionally exercised against an *emulated* device on a virtual serial port and in a headless UI
test with the real dump. The recorded raw dump of such a session is byte-identical in format to the
original dump. This does not replace tests with real hardware.

**Not verified with real hardware or data:**

- Out-of-band intervals (`FF` / `F5 EE`): the two duration bytes are interpreted as **little endian**.
- Overflow and alarm flags, the `F8` skip code, the 5-byte timestamp `F5 EF`.
- The log layouts for firmware below 7.01 and the old (address-based) dump format.
- Setting date and time, the online modes, `z`, `i`, `N`.
- The 550 ms timing and the baud-rate detection on real hardware, and the application on
  Windows and macOS.

**Further limitations:**

- Counts are **not converted to µSv/h**; the conversion data returned by `c` is not interpreted.
  Values are shown as counts and counts per minute. The conversion set (`Cs137`/`Co60`) is stored
  per entry.
- The format of the answers to `s` (online mode) and the meaning of the digits `0`–`9` are not
  documented; the console shows raw text only.
- Parameter blocks (`t`, `u`, `d`) are sent without a terminator and the device gives no documented
  confirmation; check the result with `v` (FW 6.00+) or on the device.
- The data end is detected at the first run of eight `0xFF` bytes, cross-checked with the used-bytes
  field from FW 7.03 on.
- Timestamps are the device's local time; no time-zone conversion is done. Dates are limited to
  2000–2099.
- If the log memory is full, the behaviour of the device is not described in the documentation.
  Read out and back up the data regularly. (In the verified dump 64,678 of 64,704 bytes were in use.)

## Safety notes

- `z` resets the log pointer; the log data on the device will probably be lost. **Read out and save
  the data first.**
- `i` prepares a cold start and resets the device. The manufacturer's documentation warns about it
  explicitly. Do not use it on a device with data you need.
- `N` triggers a warm start; the connection may drop briefly.
- All three commands require an explicit confirmation in a dialog.
- The *Try other ports* option sends the character `v` to all other serial ports. Do not use it if
  unrelated devices are attached.
- Sending undocumented characters to the device is not supported by this program.

## Troubleshooting

| Problem | Cause / solution |
|---|---|
| `No such file or directory` when opening `COM3` | `COMx` names exist only on Windows. On Linux use `/dev/ttyUSB0` (check with `ls /dev/ttyUSB*` and `dmesg`), on macOS `/dev/cu.usbserial-*`. |
| `Permission denied` on Linux | Add your user to the `dialout` group (`sudo usermod -aG dialout $USER`, then log in again) or, for a quick test, `sudo chmod a+rw /dev/ttyUSB0`. |
| WSL2: no `/dev/ttyUSB0` | WSL2 does not see USB devices by default. Attach the device with `usbipd-win` (`usbipd list`, `usbipd bind`, `usbipd attach --wsl`) or run the program natively on Windows. WSL1 exposes `COMx` as `/dev/ttyS<x>`. |
| "No response" with firmware < 6.00 | Press the **PC button** on the device before connecting. Old firmware answers `v` only in PC mode. |
| No response with firmware 6.00+ | Check cable and driver, close other programs using the port, try a fixed baud rate (460800 / 9600). |
| Wrong port is selected | Enable *Try other ports on failure*, or choose the port from the drop-down. |
| Buttons are greyed out | The command does not match the detected firmware/mode; hover over the button to see the reason. Switch to PC mode with `P` (FW 6.00+) or the PC button. |
| "Firmware version unknown" | Enter the version in *Firmware (optional)* (needed for dumps without a version line). |
| Warnings about checksums / unreadable lines | The transfer was disturbed (cable, baud rate); read the log again. |
| File dialogs do not appear (Linux) | Install `xdg-desktop-portal` and a backend (GTK/KDE), or build `rfd` with the `gtk3` feature. |
| The program does not start (Linux) | Missing desktop libraries (X11/Wayland, `libxkbcommon`, OpenGL). |
| Graph shows a jump when switching *Counts per interval* | Intervals differ in length (10 minutes vs. one week); use cpm or averaging to compare. |

## Dependencies

| Crate | Version | Purpose |
|---|---|---|
| [`eframe`](https://crates.io/crates/eframe) / `egui` | 0.29 | Window and GUI |
| [`egui_plot`](https://crates.io/crates/egui_plot) | 0.29 | Zoomable plot |
| [`serialport`](https://crates.io/crates/serialport) | 4 (default features off) | Serial communication |
| [`chrono`](https://crates.io/crates/chrono) | 0.4 (`std`, `clock`) | Date/time handling |
| [`rfd`](https://crates.io/crates/rfd) | 0.15 | Native file dialogs |
| [`anyhow`](https://crates.io/crates/anyhow) | 1 | Error handling |

`serialport` is built without `libudev`, so no extra system package is needed on Linux; the port
list is completed from `/dev/ttyUSB*`, `/dev/ttyACM*` and `/dev/cu.usbserial*`. `eframe` and
`egui_plot` must use matching versions; newer releases have API changes.

## License and credits

See `LICENSE` file.

The serial protocol is documented by Gamma-Scout GmbH & Co. KG in *Communication Interface V1.12*.
Gamma-Scout is a trademark of its owner. The official interface description is not part of this
repository.
