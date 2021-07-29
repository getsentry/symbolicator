use std::io;
use std::process::{Command, Stdio};

fn emit_version_var() -> Result<(), io::Error> {
    let cmd = Command::new("git")
        .args(&["describe", "--always", "--dirty=-modified"])
        .stderr(Stdio::inherit())
        .output()?;

    if !cmd.status.success() {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("`git describe' failed: {}", cmd.status),
        ));
    }

    let ver = String::from_utf8_lossy(&cmd.stdout);

    println!("cargo:rustc-env=SYMBOLICATOR_GIT_VERSION={}", ver);
    println!("cargo:rerun-if-env-changed=SYMBOLICATOR_GIT_VERSION");

    Ok(())
}

fn emit_release_var() -> Result<(), io::Error> {
    let cmd = Command::new("git")
        .args(&["rev-parse", "HEAD"])
        .stderr(Stdio::inherit())
        .output()?;

    if !cmd.status.success() {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("`git rev-parse' failed: {}", cmd.status),
        ));
    }

    let ver = String::from_utf8_lossy(&cmd.stdout);

    println!("cargo:rustc-env=SYMBOLICATOR_RELEASE={}", ver);
    println!("cargo:rerun-if-env-changed=SYMBOLICATOR_RELEASE");

    Ok(())
}

fn main() {
    emit_version_var().ok();
    emit_release_var().ok();
}
