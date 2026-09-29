use capralink_engine::{input_devices, output_devices, Config, Link};
use std::time::Duration;

const USAGE: &str = "usage: capralinkd --list
       capralinkd --peer HOST:PORT [--port N] [--in NAME] [--out NAME] [--bitrate BPS] [--channels 1|2]";

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--list") {
        println!("Input devices:");
        input_devices().iter().for_each(|d| println!("  {d}"));
        println!("Output devices:");
        output_devices().iter().for_each(|d| println!("  {d}"));
        return Ok(());
    }
    let mut cfg = Config::default();
    let mut peer = None;
    for pair in args.chunks(2) {
        let [flag, val] = pair else { anyhow::bail!("missing value for {}\n{USAGE}", pair[0]) };
        match flag.as_str() {
            "--peer" => peer = Some(std::net::ToSocketAddrs::to_socket_addrs(val)?.next().ok_or_else(|| anyhow::anyhow!("bad peer {val}"))?),
            "--port" => cfg.port = val.parse()?,
            "--in" => cfg.input = Some(val.clone()),
            "--out" => cfg.output = Some(val.clone()),
            "--bitrate" => cfg.bitrate = val.parse()?,
            "--channels" => cfg.channels = val.parse()?,
            _ => anyhow::bail!("unknown flag {flag}\n{USAGE}"),
        }
    }
    cfg.peer = peer.ok_or_else(|| anyhow::anyhow!("--peer is required\n{USAGE}"))?;
    let link = Link::start(cfg)?;
    loop {
        std::thread::sleep(Duration::from_secs(5));
        let s = link.stats();
        eprintln!(
            "sent={} received={} lost={} fec_recovered={} underruns={} buffer_ms={:.1}",
            s.sent, s.received, s.lost, s.fec_recovered, s.underruns, s.buffer_ms
        );
    }
}
