use std::env;
use std::process::{Command, Stdio};

fn main() {
    let exe = match env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("failed to locate Private Access service shim executable: {error}");
            std::process::exit(1);
        }
    };
    let Some(dir) = exe.parent() else {
        eprintln!("failed to locate Private Access service shim directory");
        std::process::exit(1);
    };
    let sing_box_tui = dir.join(format!("sing-box-tui{}", env::consts::EXE_SUFFIX));
    // The real implementation stays in the main binary so the TUI can spawn the same service
    // protocol in development and installed layouts. This shim gives manifests a short stable
    // Private Access service executable name without duplicating Hillstone protocol code.
    let status = Command::new(sing_box_tui)
        .arg("private-access-service")
        .arg("hillstone")
        .arg("--stdio")
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status();
    match status {
        Ok(status) => std::process::exit(status.code().unwrap_or(1)),
        Err(error) => {
            eprintln!("failed to start sing-box-tui Hillstone service: {error}");
            std::process::exit(1);
        }
    }
}
