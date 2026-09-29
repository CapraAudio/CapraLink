//! Linux virtual devices (MASTER.md §3.4): PulseAudio / pipewire-pulse modules loaded with `pactl`.
//! Other OSes: nothing to do (macOS ships drivers; Windows is M4).

use std::process::Command;

/// (key token identifying our module in `pactl list short modules`, module, arguments).
/// Single quotes around the property list keep the space in the description intact for both
/// PulseAudio's and pipewire-pulse's argument parsers.
const MODULES: [(&str, &str, &str); 3] = [
    (
        "sink_name=capralink_output",
        "module-null-sink",
        r#"sink_name=capralink_output sink_properties='device.description="CapraLink Output"' rate=48000 channels=2"#,
    ),
    (
        "sink_name=capralink_input_feed",
        "module-null-sink",
        r#"sink_name=capralink_input_feed sink_properties='device.description="CapraLink Input (internal)"' rate=48000 channels=2"#,
    ),
    (
        "source_name=capralink_input",
        "module-remap-source",
        r#"master=capralink_input_feed.monitor source_name=capralink_input source_properties='device.description="CapraLink Input"'"#,
    ),
];

/// The virtual devices of this process. Dropping (or `unload`) removes the modules it holds.
#[derive(Default)]
pub struct Virtual {
    loaded: Vec<u32>,
    pub error: Option<String>,
}

impl Virtual {
    /// Creates the devices, reusing any left over from a crashed run. Never fails: a problem
    /// lands in `error` and everything else keeps working.
    pub fn setup() -> Virtual {
        let mut v = Virtual::default();
        if cfg!(target_os = "linux") && !cfg!(test) {
            if let Err(e) = v.load() {
                v.error = Some(format!("Virtual devices unavailable: {e}"));
            }
        }
        v
    }

    // ponytail: leftovers are adopted and unloaded on our exit, so two CapraLink processes on
    // one machine would pull the devices from under each other; track ownership if that's ever done.
    fn load(&mut self) -> Result<(), String> {
        let list = pactl(&["list", "short", "modules"])?;
        for (key, module, args) in MODULES {
            let idx = match find_module(&list, module, key) {
                Some(i) => i,
                None => {
                    let out = pactl(&["load-module", module, args])?;
                    out.trim().parse().map_err(|_| format!("pactl load-module {module} printed {:?}", out.trim()))?
                }
            };
            self.loaded.push(idx);
        }
        Ok(())
    }

    pub fn unload(&mut self) {
        while let Some(i) = self.loaded.pop() {
            let _ = pactl(&["unload-module", &i.to_string()]);
        }
    }
}

impl Drop for Virtual {
    fn drop(&mut self) {
        self.unload();
    }
}

// ponytail: pactl runs synchronously in Node::start; a hung sound server would stall startup.
// Move it to a thread with a timeout if that's ever seen.
fn pactl(args: &[&str]) -> Result<String, String> {
    let out = Command::new("pactl").args(args).output().map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => "pactl not found — install it (Debian/Ubuntu/Fedora: pulseaudio-utils) and restart CapraLink".to_string(),
        _ => format!("can't run pactl: {e}"),
    })?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!("`pactl {}` failed: {} (is PulseAudio or PipeWire with pipewire-pulse running?)", args.join(" "), err.trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Index of the module named `module` whose arguments contain the exact token `key`,
/// in `pactl list short modules` output (`index<TAB>name<TAB>arguments…`).
fn find_module(list: &str, module: &str, key: &str) -> Option<u32> {
    list.lines().find_map(|l| {
        let mut f = l.split('\t');
        let idx = f.next()?.trim().parse().ok()?;
        (f.next()? == module && f.next()?.split_whitespace().any(|t| t == key)).then_some(idx)
    })
}

#[cfg(test)]
#[test]
fn modules_list_parsing() {
    let list = "0\tmodule-device-restore\t\t\n\
        536870913\tmodule-null-sink\tsink_name=capralink_input_feed sink_properties='device.description=\"CapraLink Input (internal)\"'\t\n\
        24\tmodule-remap-source\tmaster=capralink_input_feed.monitor source_name=capralink_input source_properties=x\t1\n\
        25\tmodule-null-sink\tsink_name=capralink_output_other\t\n";
    assert_eq!(find_module(list, "module-null-sink", "sink_name=capralink_input_feed"), Some(536870913));
    assert_eq!(find_module(list, "module-remap-source", "source_name=capralink_input"), Some(24));
    assert_eq!(find_module(list, "module-null-sink", "sink_name=capralink_output"), None, "exact token only");
    assert_eq!(find_module(list, "module-null-sink", "source_name=capralink_input"), None, "module name must match");
    for (key, _, args) in MODULES {
        assert!(args.split_whitespace().any(|t| t == key), "{key} must appear in its own arguments");
    }
}
