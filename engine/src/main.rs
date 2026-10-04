use capralink_engine::{input_devices, output_devices, Node, VIRTUAL_INPUT, VIRTUAL_OUTPUT};
use std::time::Duration;

const USAGE: &str = "usage: capralinkd [--port N] [--open-pairing] [--pair NAME_OR_ID PIN] [--connect NAME_OR_ID]
       capralinkd [--port N] --daemon
       capralinkd --list
Runs a headless CapraLink node. --pair alone pairs and exits; --connect keeps running.
--open-pairing lets another computer pair with this one using the PIN, for 2 minutes.
--daemon runs the engine like `capralink --daemon` (local RPC on port+1, no console output).
Config dir: $CAPRALINK_CONFIG_DIR, else the OS config dir.";

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (mut port, mut pair, mut connect, mut daemon, mut open) = (47800, None, None, false, false);
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--list" => {
                println!("Input devices:");
                input_devices().iter().for_each(|d| println!("  {}  [{}]", d.name, d.id));
                println!("Output devices:");
                output_devices().iter().for_each(|d| println!("  {}  [{}]", d.name, d.id));
                return Ok(());
            }
            "--port" => port = next(&mut it, flag)?.parse()?,
            "--pair" => pair = Some((next(&mut it, flag)?, next(&mut it, flag)?)),
            "--connect" => connect = Some(next(&mut it, flag)?),
            "--daemon" => daemon = true,
            "--open-pairing" => open = true,
            _ => anyhow::bail!("unknown flag {flag}\n{USAGE}"),
        }
    }

    let dir = std::env::var_os("CAPRALINK_CONFIG_DIR").map(Into::into);
    if daemon {
        return capralink_engine::daemon(dir, port);
    }
    let node = Node::start(dir, port, true)?;
    // Ctrl-C / service stop: remove the virtual devices and say goodbye on mDNS before exiting
    let n = node.clone();
    ctrlc::set_handler(move || {
        n.shutdown();
        std::process::exit(0);
    })?;
    let (mut shown_pin, mut shown_err) = (String::new(), None);
    // state() resets the peak/gap meters, so each tick reads it once
    let mut header = |st: &capralink_engine::NodeState| {
        if st.pin != shown_pin {
            println!("This device: {}   PIN: {}   (port {})", st.name, st.pin, node.port());
            shown_pin.clone_from(&st.pin);
        }
        if st.error != shown_err {
            if let Some(e) = &st.error {
                println!("error: {e}");
            }
            shown_err.clone_from(&st.error);
        }
    };
    if open {
        node.open_pairing()?;
        println!("Pairing open for 2 minutes");
    }
    header(&node.state());
    match node.state().virtual_error {
        Some(e) => println!("{e}"),
        None if input_devices().iter().any(|d| d.id == VIRTUAL_OUTPUT) && output_devices().iter().any(|d| d.id == VIRTUAL_INPUT) => {
            println!("Virtual devices ready: \"{VIRTUAL_OUTPUT}\" (send from it) and \"{VIRTUAL_INPUT}\" (play to it)")
        }
        None => println!("Virtual devices not installed"),
    }
    if let Some((who, pin)) = pair {
        let r = find(&node, &who).and_then(|id| node.pair(&id, &pin));
        if r.is_err() || connect.is_none() {
            node.shutdown();
        }
        r?;
        println!("Paired with {who}");
        if connect.is_none() {
            return Ok(());
        }
    }
    if let Some(who) = connect {
        node.connect(&find(&node, &who)?)?;
        println!("Connected to {who}");
    }
    loop {
        std::thread::sleep(Duration::from_secs(5));
        let st = node.state();
        header(&st);
        for d in &st.devices {
            let status = match (d.paired, d.online, d.connected) {
                (_, _, true) => "Paired · Connected",
                (true, true, _) => "Paired · Online",
                (true, false, _) => "Paired · Offline",
                _ => "Not paired",
            };
            println!("  {} [{}] {status}", d.name, &d.id[..8]);
        }
        if let Some(s) = st.stats {
            println!(
                "sent={} received={} lost={} fec_recovered={} underruns={} buffer_ms={:.1} target_ms={:.0} in={:.0}dB out={:.0}dB tx_gap={:.0}ms rx_gap={:.0}ms kbps={} cx={}{}{}",
                s.sent, s.received, s.lost, s.fec_recovered, s.underruns, s.buffer_ms, s.target_ms, db(s.in_peak), db(s.out_peak), s.tx_gap_ms, s.rx_gap_ms, s.bitrate / 1000, s.complexity,
                if s.music { " music" } else { "" },
                if s.hifi { " hifi" } else { "" }
            );
        }
    }
}

fn next<'a>(it: &mut impl Iterator<Item = &'a String>, flag: &str) -> anyhow::Result<String> {
    it.next().cloned().ok_or_else(|| anyhow::anyhow!("missing value for {flag}\n{USAGE}"))
}

/// Waits up to 10 s for a device with this name or id (prefix) to show up on the network.
fn find(node: &Node, key: &str) -> anyhow::Result<String> {
    for _ in 0..50 {
        let hit = node.state().devices.into_iter().find(|d| d.online && (d.id.starts_with(key) || d.name.eq_ignore_ascii_case(key)));
        if let Some(d) = hit {
            return Ok(d.id);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    anyhow::bail!("\"{key}\" not found on the network")
}

/// Peak level in dBFS; -99 means silence (e.g. mic permission denied).
fn db(peak: f32) -> f32 {
    (20.0 * peak.log10()).max(-99.0)
}
