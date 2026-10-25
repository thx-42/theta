//! Bakes build metadata (commit, date, target, rustc) into the binary for `theta --info`.

use std::process::Command;

fn run(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Days since 1970-01-01 -> (year, month, day), proleptic Gregorian.
fn civil(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn build_date() -> String {
    let secs = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or_else(|| {
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
        });
    let (y, m, d) = civil(secs.div_euclid(86_400));
    let rem = secs.rem_euclid(86_400);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02} UTC", rem / 3600, rem % 3600 / 60)
}

fn main() {
    let commit = run("git", &["rev-parse", "--short=9", "HEAD"])
        .or_else(|| std::env::var("GITHUB_SHA").ok().map(|s| s.chars().take(9).collect()))
        .unwrap_or_else(|| "unknown".into());
    let dirty = run("git", &["status", "--porcelain", "--untracked-files=no"]).is_some();
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let set = |k: &str, v: &str| println!("cargo:rustc-env={k}={v}");
    set("THETA_GIT_COMMIT", &commit);
    set("THETA_GIT_DIRTY", if dirty { "1" } else { "0" });
    set("THETA_BUILD_DATE", &build_date());
    set("THETA_RUSTC", &run(&rustc, &["--version"]).unwrap_or_else(|| "unknown".into()));
    set("THETA_TARGET", &std::env::var("TARGET").unwrap_or_else(|_| "unknown".into()));
    set("THETA_PROFILE", &std::env::var("PROFILE").unwrap_or_else(|_| "unknown".into()));
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/index");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
    println!("cargo:rerun-if-env-changed=GITHUB_SHA");
}
