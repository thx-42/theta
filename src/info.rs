//! `theta --version` / `theta --info`: build and runtime details for bug reports.

use crate::{config, proto, update};

fn dirty() -> &'static str {
    if env!("THETA_GIT_DIRTY") == "1" { "+dirty" } else { "" }
}

pub fn print() {
    let daemon = match std::os::unix::net::UnixStream::connect(proto::socket_path()) {
        Ok(_) => "running",
        Err(_) => "not running",
    };
    let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "unknown".into());
    let channel = if update::is_dev() { "dev mode (stable + nightly updates)" } else { "stable" };
    let release = if update::is_release() { "release build" } else { "local build" };
    let rows = [
        ("version", update::VERSION.to_string()),
        ("package", env!("CARGO_PKG_VERSION").to_string()),
        ("channel", channel.to_string()),
        ("origin", release.to_string()),
        ("commit", format!("{}{}", env!("THETA_GIT_COMMIT"), if dirty().is_empty() { "" } else { " (uncommitted changes)" })),
        ("built", env!("THETA_BUILD_DATE").to_string()),
        ("target", format!("{} ({})", env!("THETA_TARGET"), env!("THETA_PROFILE"))),
        ("rustc", env!("THETA_RUSTC").to_string()),
        ("os", format!("{} {}", std::env::consts::OS, std::env::consts::ARCH)),
        ("exe", exe),
        ("home", config::home().display().to_string()),
        ("config", config::global_settings_path().display().to_string()),
        ("daemon", format!("{daemon} ({})", proto::socket_path().display())),
        ("repo", update::repo()),
    ];
    println!("θ theta");
    for (k, v) in rows {
        println!("  {k:<8}{v}");
    }
}
