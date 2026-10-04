fn main() {
    // The git tree hash of drivers/macos: identifies the audio drivers this build ships, so an
    // in-app update can tell whether a release changed them. "unknown" (no git) never matches.
    let drivers = std::process::Command::new("git")
        .args(["rev-parse", "HEAD:drivers/macos"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map_or("unknown".into(), |o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    println!("cargo:rustc-env=DRIVERS_REV={drivers}");
    println!("cargo:rerun-if-changed=../.git/HEAD");
    tauri_build::build()
}
