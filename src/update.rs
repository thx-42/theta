//! `theta update`: look for a newer GitHub release, verify its checksum and replace this binary.

use anyhow::{Context, Result, bail};
use futures::StreamExt;
use sha2::{Digest, Sha256};
use std::io::{IsTerminal, Write};

/// Release tag baked in by CI (`v<version>-<run number>`); absent in local builds.
const TAG: Option<&str> = option_env!("THETA_TAG");

pub const VERSION: &str = match TAG {
    Some(t) => t,
    None => concat!(env!("CARGO_PKG_VERSION"), "-dev"),
};

const ART: &str = r#"        _   _          _
       | |_| |__   ___| |_ __ _
       | __| '_ \ / _ \ __/ _` |
       | |_| | | |  __/ || (_| |
        \__|_| |_|\___|\__\__,_|"#;

/// Colors only when stdout is a terminal.
struct Ink(bool);

impl Ink {
    fn paint(&self, code: &str, s: &str) -> String {
        if self.0 { format!("\x1b[{code}m{s}\x1b[0m") } else { s.to_string() }
    }
    fn say(&self, face: &str, msg: &str) {
        println!("  {}  {msg}", self.paint("35;1", face));
    }
}

fn repo() -> String {
    std::env::var("THETA_REPO").unwrap_or_else(|_| "thx-42/theta".into())
}

fn target() -> Result<&'static str> {
    Ok(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        (os, arch) => bail!("no prebuilt binary for {os} {arch}: build from source"),
    })
}

/// Run number at the end of a tag (`v0.1.0-12` → 12).
fn build_of(tag: &str) -> Option<u64> {
    tag.rsplit('-').next()?.parse().ok()
}

fn is_newer(latest: &str, current: &str) -> bool {
    match (build_of(latest), build_of(current)) {
        (Some(l), Some(c)) => l > c,
        _ => latest != current,
    }
}

async fn latest_tag(http: &reqwest::Client) -> Result<String> {
    let url = format!("https://api.github.com/repos/{}/releases/latest", repo());
    let r = http.get(&url).header("accept", "application/vnd.github+json").send().await.context("cannot reach github.com")?;
    if r.status() == reqwest::StatusCode::NOT_FOUND {
        bail!("no release published on github.com/{} yet", repo());
    }
    let v: serde_json::Value = r.error_for_status()?.json().await?;
    v["tag_name"].as_str().map(String::from).context("release without a tag")
}

fn mb(n: u64) -> String {
    format!("{:.1}MB", n as f64 / 1_048_576.0)
}

async fn download(http: &reqwest::Client, url: &str, ink: &Ink, face: &str) -> Result<Vec<u8>> {
    let r = http.get(url).send().await?.error_for_status().with_context(|| format!("download failed: {url}"))?;
    let total = r.content_length();
    let (mut buf, mut stream) = (Vec::new(), r.bytes_stream());
    while let Some(chunk) = stream.next().await {
        buf.extend_from_slice(&chunk?);
        if ink.0 {
            let frac = total.map(|t| (buf.len() as f64 / t as f64).min(1.0)).unwrap_or(0.0);
            let full = (frac * 20.0) as usize;
            let bar = format!("{}{}", "█".repeat(full), "░".repeat(20 - full));
            print!("\r  {}  [{}] {:>3.0}%  {}   ", ink.paint("35;1", face), ink.paint("36", &bar), frac * 100.0, mb(buf.len() as u64));
            let _ = std::io::stdout().flush();
        }
    }
    if ink.0 {
        println!();
    }
    Ok(buf)
}

fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// Unpack `theta` from the archive and swap it over the running binary.
fn install(archive: &[u8], exe: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let tmp = std::env::temp_dir().join(format!("theta-update-{}", std::process::id()));
    std::fs::create_dir_all(&tmp)?;
    let result = (|| -> Result<()> {
        std::fs::write(tmp.join("theta.tar.gz"), archive)?;
        let st = std::process::Command::new("tar").arg("-xzf").arg(tmp.join("theta.tar.gz")).arg("-C").arg(&tmp).status().context("tar not found")?;
        if !st.success() {
            bail!("cannot unpack the archive");
        }
        // Copy next to the target first: rename is only atomic on the same filesystem.
        let staged = exe.with_file_name(".theta.new");
        std::fs::copy(tmp.join("theta"), &staged).with_context(|| format!("cannot write next to {} (permissions? reinstall with install.sh)", exe.display()))?;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
        std::fs::rename(&staged, exe).with_context(|| format!("cannot replace {}", exe.display()))
    })();
    let _ = std::fs::remove_dir_all(&tmp);
    result
}

pub async fn run(check_only: bool, force: bool) -> Result<()> {
    let ink = Ink(std::io::stdout().is_terminal());
    println!("{}", ink.paint("35;1", ART));
    println!("{}\n", ink.paint("2", "        ~ update ~"));

    // GitHub's API rejects requests without a user agent.
    let http = reqwest::Client::builder().user_agent(format!("theta/{VERSION}")).connect_timeout(std::time::Duration::from_secs(20)).build()?;
    ink.say("(•‿•)", &format!("looking for news on github.com/{} …", repo()));
    let latest = latest_tag(&http).await?;
    let dev = TAG.is_none();
    println!("        {} {}", ink.paint("2", "current"), if dev { format!("{VERSION} (local build)") } else { VERSION.to_string() });
    println!("        {} {latest}", ink.paint("2", "latest "));

    let newer = dev || is_newer(&latest, VERSION);
    if !newer && !force {
        ink.say("(｡•‿•｡)", "already up to date, nothing to do ♥");
        return Ok(());
    }
    if check_only {
        ink.say("(☆ᴗ☆)", &format!("a new version is out! run {} to get it", ink.paint("1", "theta update")));
        return Ok(());
    }
    if dev && !force {
        ink.say("(•ᴗ•)", &format!("this is a local build: {} replaces it with {latest}", ink.paint("1", "theta update --force")));
        return Ok(());
    }

    let (target, base) = (target()?, format!("https://github.com/{}/releases/download/{latest}", repo()));
    let file = format!("theta-{target}.tar.gz");
    let archive = download(&http, &format!("{base}/{file}"), &ink, "(づ｡◕‿‿◕｡)づ").await?;
    ink.say("(•_•)", "checking the checksum …");
    let sums = String::from_utf8(download(&http, &format!("{base}/checksums.txt"), &Ink(false), "").await?)?;
    let want = sums.lines().filter_map(|l| l.split_once(char::is_whitespace)).find(|(_, n)| n.trim() == file).map(|(h, _)| h.to_string());
    if want.as_deref() != Some(&sha256(&archive)) {
        ink.say("(╥﹏╥)", "checksum mismatch, nothing was installed");
        bail!("checksum mismatch for {file}");
    }

    let exe = std::env::current_exe()?.canonicalize()?;
    ink.say("(ง •̀_•́)ง", &format!("installing → {}", exe.display()));
    install(&archive, &exe)?;
    // Keep install.sh's bookkeeping in step when it manages this install.
    let state = crate::config::home().join("install");
    if state.exists() {
        let _ = std::fs::write(&state, format!("method=release\nref={latest}\n"));
    }
    ink.say("(ﾉ◕ヮ◕)ﾉ*:･ﾟ✧", &format!("updated to {}", ink.paint("1", &latest)));
    if tokio::net::UnixStream::connect(crate::proto::socket_path()).await.is_ok() {
        ink.say("(•_•)", "the background daemon still runs the old version: `theta daemon stop` restarts it (running agents are cut)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_compares_run_numbers_not_strings() {
        assert!(is_newer("v0.1.0-10", "v0.1.0-9"));
        assert!(!is_newer("v0.1.0-9", "v0.1.0-10"));
        assert!(!is_newer("v0.1.0-3", "v0.1.0-3"));
        assert!(is_newer("v0.2.0", "v0.1.0"));
    }

    #[test]
    fn sha256_matches_known_digest() {
        assert_eq!(sha256(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }
}
