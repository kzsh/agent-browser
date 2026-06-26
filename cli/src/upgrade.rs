use crate::color;
use std::process::{exit, Command};

const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const GITHUB_RELEASES_URL: &str = "https://api.github.com/repos/kzsh/agent-browser/releases/latest";

enum InstallMethod {
    Homebrew,
    Cargo,
    Unknown,
}

async fn fetch_latest_version() -> Result<String, String> {
    let resp = reqwest::Client::new()
        .get(GITHUB_RELEASES_URL)
        .header("User-Agent", "agent-browser-upgrade")
        .send()
        .await
        .map_err(|e| format!("Failed to fetch version info: {}", e))?;

    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse version info: {}", e))?;

    body.get("tag_name")
        .and_then(|v| v.as_str())
        .map(|s| s.trim_start_matches('v').to_string())
        .ok_or_else(|| "No tag_name in GitHub release response".to_string())
}

fn detect_install_method() -> InstallMethod {
    if let Ok(exe) = std::env::current_exe() {
        let real_path = exe.canonicalize().unwrap_or(exe);
        let path_str = real_path.to_string_lossy();

        if path_str.contains("/.cargo/bin/") {
            return InstallMethod::Cargo;
        }

        if path_str.contains("/Cellar/agent-browser/")
            || path_str.contains("/homebrew/")
            || path_str.contains("/linuxbrew/")
        {
            return InstallMethod::Homebrew;
        }
    }

    if command_succeeds("brew", &["list", "agent-browser"]) {
        return InstallMethod::Homebrew;
    }

    InstallMethod::Unknown
}

fn command_succeeds(cmd: &str, args: &[&str]) -> bool {
    Command::new(cmd)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn run_upgrade_command(method: &InstallMethod) -> bool {
    let (cmd, args, display): (&str, &[&str], &str) = match method {
        InstallMethod::Homebrew => (
            "brew",
            &["upgrade", "agent-browser"],
            "brew upgrade agent-browser",
        ),
        InstallMethod::Cargo => (
            "cargo",
            &["install", "agent-browser", "--force"],
            "cargo install agent-browser --force",
        ),
        InstallMethod::Unknown => return false,
    };

    println!("Running: {}", display);
    Command::new(cmd)
        .args(args)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub fn run_upgrade() {
    let current = CURRENT_VERSION;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|e| {
            eprintln!(
                "{} Failed to create runtime: {}",
                color::error_indicator(),
                e
            );
            exit(1);
        });

    let latest = match rt.block_on(fetch_latest_version()) {
        Ok(v) => v,
        Err(e) => {
            eprintln!(
                "{} Could not check latest version: {}",
                color::warning_indicator(),
                e
            );
            String::new()
        }
    };

    if !latest.is_empty() && current == latest.as_str() {
        println!(
            "{} agent-browser is already at the latest version (v{})",
            color::success_indicator(),
            current
        );
        return;
    }

    let method = detect_install_method();

    if matches!(method, InstallMethod::Unknown) {
        eprintln!(
            "{} Could not detect installation method.",
            color::error_indicator()
        );
        eprintln!("  To update manually:");
        eprintln!("    brew upgrade agent-browser              # Homebrew");
        eprintln!("    cargo install agent-browser --force     # Cargo");
        exit(1);
    }

    let method_name = match &method {
        InstallMethod::Homebrew => "Homebrew",
        InstallMethod::Cargo => "Cargo",
        InstallMethod::Unknown => unreachable!(),
    };

    println!("Detected installation via {}.", method_name);

    if !latest.is_empty() {
        println!(
            "{}",
            color::cyan(&format!(
                "Upgrading agent-browser... v{} → v{}",
                current, latest
            ))
        );
    } else {
        println!(
            "{}",
            color::cyan(&format!("Upgrading agent-browser (v{})...", current))
        );
    }

    let success = run_upgrade_command(&method);

    if success {
        if !latest.is_empty() {
            println!(
                "{} Done! v{} → v{}",
                color::success_indicator(),
                current,
                latest
            );
        } else {
            println!("{} Done!", color::success_indicator());
        }
    } else {
        eprintln!("{} Upgrade failed.", color::error_indicator());
        exit(1);
    }
}
