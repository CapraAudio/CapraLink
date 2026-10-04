//! Push-to-talk: the talk state machine, the start/stop chirps, and a listen-only global
//! key/button listener per OS. The listener never grabs or consumes input, so games still get
//! every key: evdev reads on Linux, Raw Input (RIDEV_INPUTSINK) on Windows, a listen-only
//! CGEventTap on macOS. No low-level hooks (anti-cheat software flags them).

use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Push-to-talk setting (per connection).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PttMode {
    #[default]
    Off,
    /// Talking while the key is held, plus `TAIL`.
    Hold,
    /// Each press starts or stops talking.
    Toggle,
}

/// A key or button. `id` is opaque and per OS ("evdev:183" on Linux, "vk:124" on Windows,
/// "key:105" on macOS, "mouse:3" for a mouse button on Windows/macOS); `label` is for people.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PttKey {
    pub id: String,
    pub label: String,
}

/// Hold mode keeps talking this long after the key is released (so word endings aren't cut).
pub const TAIL: Duration = Duration::from_millis(200);

/// Talking or not, from presses and releases of the push-to-talk key.
#[derive(Debug, Default)]
pub struct Talk {
    mode: PttMode,
    held: bool,
    on: bool,
    until: Option<Instant>, // Hold: end of the release tail
}

impl Talk {
    pub fn new(mode: PttMode) -> Talk {
        Talk { mode, ..Talk::default() }
    }

    /// Key down. Auto-repeat (another down while held) changes nothing.
    pub fn press(&mut self) {
        if std::mem::replace(&mut self.held, true) {
            return;
        }
        match self.mode {
            PttMode::Hold => (self.on, self.until) = (true, None),
            PttMode::Toggle => self.on = !self.on,
            PttMode::Off => {}
        }
    }

    pub fn release(&mut self, now: Instant) {
        if std::mem::replace(&mut self.held, false) && self.mode == PttMode::Hold && self.on {
            self.until = Some(now + TAIL);
        }
    }

    /// Ends a Hold tail that has run out.
    pub fn tick(&mut self, now: Instant) {
        if self.until.is_some_and(|u| now >= u) {
            (self.on, self.until) = (false, None);
        }
    }

    pub fn on(&self) -> bool {
        self.on
    }

    /// Input may have been missed: stop talking until the next press.
    pub fn reset(&mut self) {
        *self = Talk::new(self.mode);
    }

    /// When `tick` next has something to do.
    pub fn deadline(&self) -> Option<Instant> {
        self.until
    }
}

/// How long a chirp lasts (its second tone ends at 70 + 80 ms).
pub const CHIRP_SECS: f32 = 0.15;

/// The "rising chirp" at `t` seconds: talking starts = 660 Hz for 60 ms, then 880 Hz for 80 ms
/// from 70 ms; stops = the reverse (880 then 660).
pub fn chirp(start: bool, t: f32) -> f32 {
    let (a, b) = if start { (660.0, 880.0) } else { (880.0, 660.0) };
    beep(a, t, 0.060) + beep(b, t - 0.070, 0.080)
}

/// One tone of `len` s: 5 ms linear attack, then an exponential decay reaching −60 dB at `len`;
/// 0.25 peak.
fn beep(hz: f32, t: f32, len: f32) -> f32 {
    const ATTACK: f32 = 0.005;
    if !(0.0..len).contains(&t) {
        return 0.0;
    }
    let env = if t < ATTACK { t / ATTACK } else { (-(1000f32.ln()) * (t - ATTACK) / (len - ATTACK)).exp() };
    0.25 * env * (std::f32::consts::TAU * hz * t).sin()
}

/// What the listener reports.
pub enum Ev {
    /// A key or button went down (`true`) or up.
    Key(PttKey, bool),
    /// Why listening doesn't work right now (`None` = it works again).
    Status(Option<String>),
    /// Key changes may have been missed (a device went away, events were dropped).
    Lost,
}

/// A running listener; dropping it stops it.
pub struct Listener {
    stop: Arc<AtomicBool>,
    #[cfg(windows)]
    thread: Arc<std::sync::atomic::AtomicU32>, // its message loop, 0 until it has one
    /// Tests feed events in here (no real input).
    #[cfg(test)]
    pub tx: Sender<Ev>,
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.stop.store(true, SeqCst);
        #[cfg(windows)]
        win::wake(&self.thread);
    }
}

/// Starts listening on its own thread; events go to `tx` until the `Listener` is dropped.
pub fn listen(tx: Sender<Ev>) -> std::io::Result<Listener> {
    let stop = Arc::new(AtomicBool::new(false));
    #[cfg(windows)]
    let thread = Arc::new(std::sync::atomic::AtomicU32::new(0));
    #[cfg(not(test))] // tests feed `tx` themselves
    {
        let s = stop.clone();
        #[cfg(windows)]
        let t = thread.clone();
        std::thread::Builder::new().name("capralink-ptt-input".into()).spawn(move || {
            #[cfg(target_os = "linux")]
            linux::run(&s, &tx);
            #[cfg(windows)]
            win::run(&s, &tx, &t);
            #[cfg(target_os = "macos")]
            mac::run(&s, &tx);
            #[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
            let _ = (s, tx.send(Ev::Status(Some("Push-to-talk isn't supported on this system".into()))));
        })?;
    }
    Ok(Listener {
        stop,
        #[cfg(windows)]
        thread,
        #[cfg(test)]
        tx,
    })
}

/// A mouse button by number (2 = middle, 3 = back, 4 = forward), Windows and macOS.
#[cfg_attr(target_os = "linux", allow(dead_code))]
fn mouse(n: u32) -> PttKey {
    let label = match n {
        2 => "Middle click".into(),
        3 => "Mouse back".into(),
        4 => "Mouse forward".into(),
        n => format!("Mouse button {}", n + 1),
    };
    PttKey { id: format!("mouse:{n}"), label }
}

/// Linux evdev key/button code (linux/input-event-codes.h).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn evdev_key(code: u16) -> PttKey {
    const ROW1: &str = "1234567890"; // codes 2..=11
    const ROW2: &str = "QWERTYUIOP"; // 16..=25
    const ROW3: &str = "ASDFGHJKL"; // 30..=38
    const ROW4: &str = "ZXCVBNM"; // 44..=50
    let ch = |row: &str, first: u16| row.chars().nth((code - first) as usize).map(String::from);
    let label = match code {
        2..=11 => ch(ROW1, 2),
        16..=25 => ch(ROW2, 16),
        30..=38 => ch(ROW3, 30),
        44..=50 => ch(ROW4, 44),
        59..=68 => Some(format!("F{}", code - 58)),
        87 | 88 => Some(format!("F{}", code - 76)),
        183..=194 => Some(format!("F{}", code - 170)),
        _ => None,
    };
    let named = match code {
        1 => "Esc",
        14 => "Backspace",
        15 => "Tab",
        28 => "Enter",
        29 => "Left Ctrl",
        41 => "`",
        42 => "Left Shift",
        54 => "Right Shift",
        56 => "Left Alt",
        57 => "Space",
        58 => "Caps Lock",
        70 => "Scroll Lock",
        97 => "Right Ctrl",
        100 => "Right Alt",
        102 => "Home",
        103 => "Up",
        104 => "Page Up",
        105 => "Left",
        106 => "Right",
        107 => "End",
        108 => "Down",
        109 => "Page Down",
        110 => "Insert",
        111 => "Delete",
        119 => "Pause",
        125 => "Left Super",
        126 => "Right Super",
        127 | 139 => "Menu",
        0x112 => "Middle click",
        0x113 | 0x116 => "Mouse back",
        0x114 | 0x115 => "Mouse forward",
        0x130 => "A button",
        0x131 => "B button",
        0x133 => "Y button", // BTN_NORTH
        0x134 => "X button", // BTN_WEST
        0x136 => "Left bumper",
        0x137 => "Right bumper",
        0x138 => "Left trigger",
        0x139 => "Right trigger",
        0x13a => "View button",
        0x13b => "Menu button",
        0x13c => "Guide button",
        0x13d => "Left stick click",
        0x13e => "Right stick click",
        0x220..=0x223 => "D-pad",
        _ => "",
    };
    let label = label.or_else(|| (!named.is_empty()).then(|| named.to_string())).unwrap_or_else(|| match code {
        0x2c0..=0x2e7 => format!("Extra button {}", code - 0x2c0 + 1), // BTN_TRIGGER_HAPPY: back paddles
        0x100.. => format!("Button {code:#x}"),
        _ => format!("Key {code}"),
    });
    PttKey { id: format!("evdev:{code}"), label }
}

/// Left and right mouse buttons, and touch/stylus contact codes, are never a push-to-talk key.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn evdev_usable(code: u16) -> bool {
    !matches!(code, 0 | 0x110 | 0x111 | 0x140..=0x14f)
}

/// Key and button events in a read of `struct input_event`s: (code, down). `size` is the
/// struct's size (24 bytes on 64-bit: timeval, type u16, code u16, value i32); repeats skipped.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn evdev_events(buf: &[u8], size: usize) -> impl Iterator<Item = (u16, bool)> + '_ {
    let at = size - 8;
    buf.chunks_exact(size).filter_map(move |e| {
        let ty = u16::from_ne_bytes([e[at], e[at + 1]]);
        let code = u16::from_ne_bytes([e[at + 2], e[at + 3]]);
        let value = i32::from_ne_bytes([e[at + 4], e[at + 5], e[at + 6], e[at + 7]]);
        (ty == 1 && matches!(value, 0 | 1) && evdev_usable(code)).then_some((code, value == 1)) // EV_KEY
    })
}

/// Whether the kernel dropped events (SYN_DROPPED): key releases may be lost.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn evdev_dropped(buf: &[u8], size: usize) -> bool {
    let at = size - 8;
    buf.chunks_exact(size).any(|e| u16::from_ne_bytes([e[at], e[at + 1]]) == 0 && u16::from_ne_bytes([e[at + 2], e[at + 3]]) == 3)
}

/// Steam Deck back grips in the controller's raw state report (kernel hid-steam.c: type 9 at
/// byte 2; L4 = byte 13 bit 1, R4 = 13.2, L5 = 9.7, R5 = 10.0). Read straight from the controller,
/// so they work while Steam's menus are open and need no Steam Input mapping.
const DECK_GRIPS: [(usize, u8, &str); 4] = [(13, 1, "L4"), (9, 7, "L5"), (13, 2, "R4"), (10, 0, "R5")];

/// Which back grips a Deck report holds down (bit i = `DECK_GRIPS[i]`), None for other reports.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn deck_grips(r: &[u8]) -> Option<u8> {
    (r.len() > 13 && r[0] == 1 && r[2] == 9).then(|| DECK_GRIPS.iter().enumerate().fold(0, |m, (i, &(b, bit, _))| m | ((r[b] >> bit) & 1) << i))
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn deck_key(i: usize) -> PttKey {
    let g = DECK_GRIPS[i].2;
    PttKey { id: format!("deck:{g}"), label: format!("{g} (back grip)") }
}

/// Windows virtual-key code (left/right modifiers already told apart).
#[cfg_attr(not(windows), allow(dead_code))]
fn vk_key(vk: u16) -> PttKey {
    let label = match vk {
        0x30..=0x39 | 0x41..=0x5a => char::from(vk as u8).to_string(),
        0x70..=0x87 => format!("F{}", vk - 0x6f),
        0x60..=0x69 => format!("Numpad {}", vk - 0x60),
        _ => match vk {
            0x08 => "Backspace",
            0x09 => "Tab",
            0x0d => "Enter",
            0x13 => "Pause",
            0x14 => "Caps Lock",
            0x1b => "Esc",
            0x20 => "Space",
            0x21 => "Page Up",
            0x22 => "Page Down",
            0x23 => "End",
            0x24 => "Home",
            0x25 => "Left",
            0x26 => "Up",
            0x27 => "Right",
            0x28 => "Down",
            0x2d => "Insert",
            0x2e => "Delete",
            0x5b => "Left Windows",
            0x5c => "Right Windows",
            0x5d => "Menu",
            0x91 => "Scroll Lock",
            0xa0 => "Left Shift",
            0xa1 => "Right Shift",
            0xa2 => "Left Ctrl",
            0xa3 => "Right Ctrl",
            0xa4 => "Left Alt",
            0xa5 => "Right Alt",
            0xc0 => "`",
            _ => "",
        }
        .to_string(),
    };
    let label = if label.is_empty() { format!("Key {vk:#04x}") } else { label };
    PttKey { id: format!("vk:{vk}"), label }
}

/// macOS virtual key code (kVK_*; letters by their US-layout position).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn mac_key(code: u16) -> PttKey {
    const LETTERS: [(u16, &str); 36] = [
        (0, "A"), (1, "S"), (2, "D"), (3, "F"), (4, "H"), (5, "G"), (6, "Z"), (7, "X"), (8, "C"), (9, "V"), (11, "B"), (12, "Q"),
        (13, "W"), (14, "E"), (15, "R"), (16, "Y"), (17, "T"), (18, "1"), (19, "2"), (20, "3"), (21, "4"), (22, "6"), (23, "5"), (25, "9"),
        (26, "7"), (28, "8"), (29, "0"), (31, "O"), (32, "U"), (34, "I"), (35, "P"), (37, "L"), (38, "J"), (40, "K"), (45, "N"), (46, "M"),
    ];
    const NAMED: [(u16, &str); 49] = [
        (36, "Return"), (48, "Tab"), (49, "Space"), (50, "`"), (51, "Delete"), (53, "Esc"), (54, "Right Command"), (55, "Left Command"),
        (56, "Left Shift"), (57, "Caps Lock"), (58, "Left Option"), (59, "Left Control"), (60, "Right Shift"), (61, "Right Option"),
        (62, "Right Control"), (63, "fn"), (64, "F17"), (79, "F18"), (80, "F19"), (90, "F20"), (96, "F5"), (97, "F6"), (98, "F7"),
        (99, "F3"), (100, "F8"), (101, "F9"), (103, "F11"), (105, "F13"), (106, "F16"), (107, "F14"), (109, "F10"), (111, "F12"),
        (113, "F15"), (114, "Help"), (115, "Home"), (116, "Page Up"), (117, "Forward Delete"), (118, "F4"), (119, "End"), (120, "F2"),
        (121, "Page Down"), (122, "F1"), (123, "Left"), (124, "Right"), (125, "Down"), (126, "Up"), (10, "§"), (24, "="), (27, "-"),
    ];
    let label = LETTERS.iter().chain(&NAMED).find(|(c, _)| *c == code).map_or_else(|| format!("Key {code}"), |(_, l)| l.to_string());
    PttKey { id: format!("key:{code}"), label }
}

#[cfg(all(target_os = "linux", not(test)))]
mod linux {
    use super::{deck_grips, deck_key, evdev_dropped, evdev_events, evdev_key, Ev};
    use std::fs::File;
    use std::io::{ErrorKind, Read};
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
    use std::sync::mpsc::Sender;
    use std::time::{Duration, Instant};

    const O_NONBLOCK: i32 = 0o4000;

    /// Polls every readable /dev/input/event* (no grab: other programs still get every event),
    /// looking for new devices every few seconds.
    // ponytail: a 10 ms poll over a handful of files; switch to poll(2) if it ever shows in a profile.
    pub fn run(stop: &AtomicBool, tx: &Sender<Ev>) {
        let size = 2 * size_of::<usize>() + 8; // struct input_event
        let (mut open, mut status, mut scanned) = (Vec::<(PathBuf, File)>::new(), None, None::<Instant>);
        let mut pads = Vec::<(PathBuf, File, u8)>::new(); // Steam Deck controller (raw reports), grips held
        let mut buf = vec![0u8; size * 64];
        while !stop.load(SeqCst) {
            if scanned.is_none_or(|t| t.elapsed() > Duration::from_secs(3)) {
                scanned = Some(Instant::now());
                let mut seen = 0;
                for p in std::fs::read_dir("/dev/input").into_iter().flatten().flatten().map(|e| e.path()) {
                    if !p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("event")) {
                        continue;
                    }
                    seen += 1;
                    if !open.iter().any(|(q, _)| *q == p) {
                        if let Ok(f) = std::fs::OpenOptions::new().read(true).custom_flags(O_NONBLOCK).open(&p) {
                            open.push((p, f));
                        }
                    }
                }
                // the Deck's own controller (Valve 28DE:1205): its back grips, see `deck_grips`
                for p in std::fs::read_dir("/dev").into_iter().flatten().flatten().map(|e| e.path()) {
                    let Some(n) = p.file_name().map(|n| n.to_string_lossy().into_owned()).filter(|n| n.starts_with("hidraw")) else { continue };
                    let deck = std::fs::read_to_string(format!("/sys/class/hidraw/{n}/device/uevent")).is_ok_and(|u| u.contains("HID_ID=0003:000028DE:00001205"));
                    if deck && !pads.iter().any(|(q, ..)| *q == p) {
                        if let Ok(f) = std::fs::OpenOptions::new().read(true).custom_flags(O_NONBLOCK).open(&p) {
                            pads.push((p, f, 0));
                        }
                    }
                }
                let now = open.is_empty().then(|| match seen {
                    0 => "No keyboards, mice or controllers found".to_string(),
                    _ => "CapraLink can't read keyboards, mice or controllers: add your user to the \"input\" group (sudo usermod -aG input $USER), then log in again".to_string(),
                });
                if now != status {
                    status = now.clone();
                    let _ = tx.send(Ev::Status(now));
                }
            }
            open.retain_mut(|(_, f)| loop {
                match f.read(&mut buf) {
                    Ok(n) if n > 0 => {
                        if evdev_dropped(&buf[..n], size) {
                            let _ = tx.send(Ev::Lost);
                        }
                        for (code, down) in evdev_events(&buf[..n], size) {
                            let _ = tx.send(Ev::Key(evdev_key(code), down));
                        }
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break true,
                    Err(e) if e.kind() == ErrorKind::Interrupted => {}
                    _ => {
                        // unplugged: found again by the next scan if it comes back
                        let _ = tx.send(Ev::Lost);
                        break false;
                    }
                }
            });
            pads.retain_mut(|(_, f, held)| loop {
                match f.read(&mut buf[..64]) {
                    Ok(n) if n > 0 => {
                        if let Some(now) = deck_grips(&buf[..n]) {
                            for i in (0..4).filter(|i| (now ^ *held) >> i & 1 == 1) {
                                let _ = tx.send(Ev::Key(deck_key(i), now >> i & 1 == 1));
                            }
                            *held = now;
                        }
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break true,
                    Err(e) if e.kind() == ErrorKind::Interrupted => {}
                    _ => {
                        let _ = tx.send(Ev::Lost);
                        break false;
                    }
                }
            });
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

#[cfg(windows)]
#[cfg_attr(test, allow(dead_code))]
mod win {
    use super::{mouse, vk_key, Ev};
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::SeqCst};
    use std::sync::mpsc::Sender;
    use windows_sys::Win32::System::Threading::GetCurrentThreadId;
    use windows_sys::Win32::UI::Input::{GetRawInputData, RegisterRawInputDevices, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER, RIDEV_DEVNOTIFY, RIDEV_INPUTSINK, RID_INPUT, RIM_TYPEKEYBOARD, RIM_TYPEMOUSE};
    use windows_sys::Win32::UI::WindowsAndMessaging::*;

    /// Ends `run`'s message loop.
    pub fn wake(thread: &AtomicU32) {
        let t = thread.load(SeqCst);
        if t != 0 {
            unsafe { PostThreadMessageW(t, WM_QUIT, 0, 0) };
        }
    }

    /// Raw Input for keyboards and mice on a hidden message-only window, also while other apps
    /// have the focus (RIDEV_INPUTSINK). Listen-only: nothing is consumed.
    pub fn run(stop: &AtomicBool, tx: &Sender<Ev>, thread: &AtomicU32) {
        let fail = |what: &str| {
            let e = std::io::Error::last_os_error();
            let _ = tx.send(Ev::Status(Some(format!("Push-to-talk can't listen for keys ({what}: {e})"))));
        };
        let class: Vec<u16> = "Message\0".encode_utf16().collect();
        unsafe {
            let hwnd = CreateWindowExW(0, class.as_ptr(), std::ptr::null(), 0, 0, 0, 0, 0, HWND_MESSAGE, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null());
            if hwnd.is_null() {
                return fail("window");
            }
            // the thread has a message queue now: `wake` can reach it
            thread.store(GetCurrentThreadId(), SeqCst);
            let dev = |usage| RAWINPUTDEVICE { usUsagePage: 1, usUsage: usage, dwFlags: RIDEV_INPUTSINK | RIDEV_DEVNOTIFY, hwndTarget: hwnd };
            let devs = [dev(6), dev(2)]; // keyboard, mouse
            if RegisterRawInputDevices(devs.as_ptr(), 2, size_of::<RAWINPUTDEVICE>() as u32) == 0 {
                fail("register");
            } else {
                let mut msg: MSG = std::mem::zeroed();
                while !stop.load(SeqCst) && GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                    if msg.message == WM_INPUT {
                        for (key, down) in read(msg.lParam as HRAWINPUT) {
                            let _ = tx.send(Ev::Key(key, down));
                        }
                    } else if msg.message == WM_INPUT_DEVICE_CHANGE && msg.wParam == GIDC_REMOVAL as usize {
                        let _ = tx.send(Ev::Lost); // unplugged: its held keys never come up
                    }
                    DispatchMessageW(&msg);
                }
            }
            DestroyWindow(hwnd);
        }
    }

    /// The key/button changes in one WM_INPUT.
    unsafe fn read(h: HRAWINPUT) -> Vec<(super::PttKey, bool)> {
        let mut raw: RAWINPUT = unsafe { std::mem::zeroed() };
        let mut size = size_of::<RAWINPUT>() as u32;
        let header = size_of::<RAWINPUTHEADER>() as u32;
        if unsafe { GetRawInputData(h, RID_INPUT, (&raw mut raw).cast(), &mut size, header) } == u32::MAX {
            return vec![];
        }
        match raw.header.dwType {
            RIM_TYPEKEYBOARD => {
                let k = unsafe { raw.data.keyboard };
                let (down, e0) = (k.Flags as u32 & RI_KEY_BREAK == 0, k.Flags as u32 & RI_KEY_E0 != 0);
                let vk = match k.VKey {
                    255 => return vec![], // a fake key from an escape sequence
                    0x10 => if k.MakeCode == 0x36 { 0xa1 } else { 0xa0 }, // shift: right has its own scan code
                    0x11 => if e0 { 0xa3 } else { 0xa2 },
                    0x12 => if e0 { 0xa5 } else { 0xa4 },
                    v => v,
                };
                vec![(vk_key(vk), down)]
            }
            RIM_TYPEMOUSE => {
                let b = unsafe { raw.data.mouse.Anonymous.Anonymous.usButtonFlags } as u32;
                let mut v = vec![];
                for (dn, up, n) in [(RI_MOUSE_MIDDLE_BUTTON_DOWN, RI_MOUSE_MIDDLE_BUTTON_UP, 2), (RI_MOUSE_BUTTON_4_DOWN, RI_MOUSE_BUTTON_4_UP, 3), (RI_MOUSE_BUTTON_5_DOWN, RI_MOUSE_BUTTON_5_UP, 4)] {
                    if b & dn != 0 {
                        v.push((mouse(n), true));
                    }
                    if b & up != 0 {
                        v.push((mouse(n), false));
                    }
                }
                v
            }
            _ => vec![],
        }
    }
}

#[cfg(all(target_os = "macos", not(test)))]
mod mac {
    use super::{mac_key, mouse, Ev};
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
    use std::sync::mpsc::Sender;
    use std::time::Duration;

    type Ref = *mut c_void;
    type Callback = extern "C" fn(proxy: Ref, ty: u32, event: Ref, user: *mut c_void) -> Ref;

    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGEventTapCreate(tap: u32, place: u32, options: u32, mask: u64, callback: Callback, user: *mut c_void) -> Ref;
        fn CGEventTapEnable(tap: Ref, enable: bool);
        fn CGEventGetIntegerValueField(event: Ref, field: u32) -> i64;
        fn CGEventGetFlags(event: Ref) -> u64;
        fn CGPreflightListenEventAccess() -> bool;
        fn CGRequestListenEventAccess() -> bool;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        static kCFRunLoopDefaultMode: Ref;
        fn CFMachPortCreateRunLoopSource(alloc: Ref, port: Ref, order: isize) -> Ref;
        fn CFMachPortInvalidate(port: Ref);
        fn CFRunLoopGetCurrent() -> Ref;
        fn CFRunLoopAddSource(rl: Ref, source: Ref, mode: Ref);
        fn CFRunLoopRemoveSource(rl: Ref, source: Ref, mode: Ref);
        fn CFRunLoopRunInMode(mode: Ref, seconds: f64, return_after_source_handled: u8) -> i32;
        fn CFRelease(r: Ref);
    }

    const KEY_DOWN: u32 = 10;
    const KEY_UP: u32 = 11;
    const FLAGS_CHANGED: u32 = 12;
    const OTHER_MOUSE_DOWN: u32 = 25;
    const OTHER_MOUSE_UP: u32 = 26;
    const DISABLED_BY_TIMEOUT: u32 = 0xffff_fffe;
    const DISABLED_BY_USER_INPUT: u32 = 0xffff_ffff;
    const KEYCODE: u32 = 9; // kCGKeyboardEventKeycode
    const BUTTON: u32 = 3; // kCGMouseEventButtonNumber

    struct Tap {
        tx: Sender<Ev>,
        port: Ref,
    }

    /// The modifier flag that says whether modifier key `code` is down (device-dependent
    /// left/right bits, so either side can be the key).
    fn modifier(code: i64) -> u64 {
        match code {
            59 => 0x1,       // left control
            56 => 0x2,       // left shift
            60 => 0x4,       // right shift
            55 => 0x8,       // left command
            54 => 0x10,      // right command
            58 => 0x20,      // left option
            61 => 0x40,      // right option
            62 => 0x2000,    // right control
            57 => 0x1_0000,  // caps lock
            63 => 0x80_0000, // fn
            _ => 0,
        }
    }

    extern "C" fn callback(_: Ref, ty: u32, event: Ref, user: *mut c_void) -> Ref {
        let tap = unsafe { &*(user as *const Tap) };
        let send = |k, down| drop(tap.tx.send(Ev::Key(k, down)));
        unsafe {
            match ty {
                DISABLED_BY_TIMEOUT | DISABLED_BY_USER_INPUT => {
                    // events went by unseen while the tap was off
                    let _ = tap.tx.send(Ev::Lost);
                    CGEventTapEnable(tap.port, true);
                }
                KEY_DOWN | KEY_UP => send(mac_key(CGEventGetIntegerValueField(event, KEYCODE) as u16), ty == KEY_DOWN),
                FLAGS_CHANGED => {
                    let code = CGEventGetIntegerValueField(event, KEYCODE);
                    if modifier(code) != 0 {
                        send(mac_key(code as u16), CGEventGetFlags(event) & modifier(code) != 0);
                    }
                }
                OTHER_MOUSE_DOWN | OTHER_MOUSE_UP => send(mouse(CGEventGetIntegerValueField(event, BUTTON) as u32), ty == OTHER_MOUSE_DOWN),
                _ => {}
            }
        }
        event // listen-only: the event goes on unchanged either way
    }

    /// A listen-only event tap (needs Input Monitoring). Asks macOS for the permission once,
    /// then retries every 2 s until it is granted.
    pub fn run(stop: &AtomicBool, tx: &Sender<Ev>) {
        let tap = Box::into_raw(Box::new(Tap { tx: tx.clone(), port: std::ptr::null_mut() }));
        let mask = [KEY_DOWN, KEY_UP, FLAGS_CHANGED, OTHER_MOUSE_DOWN, OTHER_MOUSE_UP].iter().fold(0u64, |m, t| m | 1 << t);
        unsafe {
            if !CGPreflightListenEventAccess() {
                CGRequestListenEventAccess();
            }
            let mut denied = false;
            while !stop.load(SeqCst) {
                // session tap, at the head, listen-only
                let port = CGEventTapCreate(1, 0, 1, mask, callback, tap.cast());
                // without the permission macOS may still create the tap but never deliver key
                // events to it: ask the permission itself, so the window can say what to do
                if !port.is_null() && !CGPreflightListenEventAccess() {
                    CFMachPortInvalidate(port);
                    CFRelease(port);
                    if !denied {
                        denied = true;
                        let _ = tx.send(Ev::Status(Some("Allow CapraLink in System Settings → Privacy & Security → Input Monitoring (if it's already on, remove it with −, then quit and reopen CapraLink)".into())));
                    }
                    std::thread::sleep(Duration::from_secs(2));
                    continue;
                }
                if port.is_null() {
                    if !denied {
                        denied = true;
                        let _ = tx.send(Ev::Status(Some("Allow CapraLink in System Settings → Privacy & Security → Input Monitoring, then quit and reopen CapraLink".into())));
                    }
                    std::thread::sleep(Duration::from_secs(2));
                    continue;
                }
                if denied {
                    let _ = tx.send(Ev::Status(None));
                }
                (*tap).port = port;
                let source = CFMachPortCreateRunLoopSource(std::ptr::null_mut(), port, 0);
                let rl = CFRunLoopGetCurrent();
                CFRunLoopAddSource(rl, source, kCFRunLoopDefaultMode);
                CGEventTapEnable(port, true);
                while !stop.load(SeqCst) {
                    CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.2, 0);
                }
                CFRunLoopRemoveSource(rl, source, kCFRunLoopDefaultMode);
                CFMachPortInvalidate(port);
                CFRelease(source);
                CFRelease(port);
            }
            drop(Box::from_raw(tap));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hold_talks_while_held_plus_the_tail() {
        let t0 = Instant::now();
        let mut t = Talk::new(PttMode::Hold);
        assert!(!t.on());
        t.press();
        assert!(t.on() && t.deadline().is_none());
        t.press(); // auto-repeat
        t.release(t0);
        assert_eq!(t.deadline(), Some(t0 + TAIL));
        t.tick(t0 + TAIL - Duration::from_millis(1));
        assert!(t.on(), "still in the tail");
        t.press(); // pressed again inside the tail: keeps talking, no tail running
        t.tick(t0 + TAIL);
        assert!(t.on() && t.deadline().is_none());
        t.release(t0 + TAIL);
        t.tick(t0 + TAIL * 2);
        assert!(!t.on() && t.deadline().is_none());
        t.release(t0 + TAIL * 3); // a stray release
        assert!(t.deadline().is_none());
    }

    #[test]
    fn toggle_flips_on_each_press() {
        let mut t = Talk::new(PttMode::Toggle);
        t.press();
        t.press(); // auto-repeat while held doesn't flip back
        assert!(t.on());
        t.release(Instant::now());
        assert!(t.on() && t.deadline().is_none(), "no tail in toggle mode");
        t.press();
        assert!(!t.on());
        let mut off = Talk::new(PttMode::Off);
        off.press();
        assert!(!off.on());
    }

    #[test]
    fn chirps() {
        let rate = 48_000.0;
        let wave = |start| (0..(CHIRP_SECS * rate) as usize + 100).map(|i| chirp(start, i as f32 / rate)).collect::<Vec<_>>();
        let (up, down) = (wave(true), wave(false));
        let peak = |w: &[f32]| w.iter().fold(0f32, |m, s| m.max(s.abs()));
        let ms = |a: f32, b: f32| &up[(a * 48.0) as usize..(b * 48.0) as usize];
        assert!((0.2..=0.25).contains(&peak(&up)), "{}", peak(&up));
        assert!(peak(ms(0.0, 0.5)) < 0.03, "5 ms attack");
        assert!(peak(ms(4.5, 6.0)) > 0.2, "full level after the attack");
        assert!(peak(ms(57.0, 60.0)) < 0.25 * 0.003, "first tone decays towards −60 dB");
        assert_eq!(peak(ms(60.0, 70.0)), 0.0, "gap between the tones");
        assert!(peak(ms(75.0, 76.0)) > 0.15, "second tone");
        assert_eq!(peak(&up[(CHIRP_SECS * rate) as usize..]), 0.0, "silent after {CHIRP_SECS} s");
        // rising: more zero crossings in the second tone; falling: the reverse
        let crossings = |w: &[f32]| w.windows(2).filter(|p| p[0] <= 0.0 && p[1] > 0.0).count();
        let (a, b) = (crossings(&up[..2880]), crossings(&up[3360..7200]));
        assert!((39..=40).contains(&a) && (69..=71).contains(&b), "660 Hz for 60 ms, then 880 Hz for 80 ms: {a} {b}");
        let (c, d) = (crossings(&down[..2880]), crossings(&down[3360..7200]));
        assert!((52..=53).contains(&c) && (52..=53).contains(&d), "880 Hz for 60 ms, then 660 Hz for 80 ms: {c} {d}");
    }

    #[test]
    fn labels() {
        let l = |k: PttKey| (k.id, k.label);
        assert_eq!(l(evdev_key(183)), ("evdev:183".into(), "F13".into()));
        assert_eq!(l(evdev_key(47)).1, "V");
        assert_eq!(l(evdev_key(97)).1, "Right Ctrl");
        assert_eq!(l(evdev_key(0x113)).1, "Mouse back");
        assert_eq!(l(evdev_key(0x114)).1, "Mouse forward");
        assert_eq!(l(evdev_key(0x130)).1, "A button");
        assert_eq!(l(evdev_key(0x2c1)).1, "Extra button 2");
        assert_eq!(l(evdev_key(240)).1, "Key 240");
        assert!(!evdev_usable(0x110) && !evdev_usable(0x14a) && evdev_usable(0x113) && evdev_usable(30));
        assert_eq!(l(vk_key(0x7c)), ("vk:124".into(), "F13".into()));
        assert_eq!(l(vk_key(0x56)).1, "V");
        assert_eq!(l(vk_key(0xa3)).1, "Right Ctrl");
        assert_eq!(l(vk_key(0xff)).1, "Key 0xff");
        assert_eq!(l(mac_key(105)), ("key:105".into(), "F13".into()));
        assert_eq!(l(mac_key(9)).1, "V");
        assert_eq!(l(mac_key(62)).1, "Right Control");
        assert_eq!(l(mouse(3)), ("mouse:3".into(), "Mouse back".into()));
        assert_eq!(l(mouse(4)).1, "Mouse forward");
        assert_eq!(l(mouse(7)).1, "Mouse button 8");
    }

    #[test]
    fn evdev_parsing() {
        let ev = |ty: u16, code: u16, value: i32| [&[0u8; 16][..], &ty.to_ne_bytes(), &code.to_ne_bytes(), &value.to_ne_bytes()].concat();
        let buf = [ev(1, 183, 1), ev(0, 0, 0), ev(1, 183, 2), ev(4, 4, 7), ev(1, 0x110, 1), ev(1, 183, 0)].concat();
        assert_eq!(evdev_events(&buf, 24).collect::<Vec<_>>(), [(183, true), (183, false)], "keys only; repeats, sync, scan and left click skipped");
        assert!(!evdev_dropped(&buf, 24) && evdev_dropped(&[buf, ev(0, 3, 0)].concat(), 24), "SYN_DROPPED");
    }

    #[test]
    fn deck_back_grips() {
        let mut r = [0u8; 64];
        (r[0], r[2]) = (1, 9);
        assert_eq!(deck_grips(&r), Some(0));
        (r[13], r[9], r[10]) = (0b110, 0x80, 1);
        assert_eq!(deck_grips(&r), Some(0b1111), "L4, L5, R4, R5");
        r[13] = 0b10;
        (r[9], r[10]) = (0x7f, 0xfe); // every other button held: no grip
        assert_eq!(deck_grips(&r), Some(0b0001));
        r[2] = 1; // not a Deck state report
        assert_eq!(deck_grips(&r), None);
        assert_eq!((deck_key(3).id, deck_key(3).label), ("deck:R5".to_string(), "R5 (back grip)".to_string()));
    }
}
