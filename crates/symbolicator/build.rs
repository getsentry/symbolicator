use std::process::{Command, Stdio};

fn emit_version_var() {
    let cmd = Command::new("git")
        .args(["describe", "--always", "--dirty=-modified"])
        .stderr(Stdio::inherit())
        .output()
        .expect("read from stderr");

    if !cmd.status.success() {
        panic!("`git describe' failed: {}", cmd.status);
    }

    let ver = String::from_utf8_lossy(&cmd.stdout);

    println!("cargo:rustc-env=SYMBOLICATOR_GIT_VERSION={ver}");
    println!("cargo:rerun-if-env-changed=SYMBOLICATOR_GIT_VERSION");
}

fn emit_release_var() {
    let cmd = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .stderr(Stdio::inherit())
        .output()
        .expect("read from stderr");

    if !cmd.status.success() {
        panic!("`git rev-parse' failed: {}", cmd.status);
    }

    let ver = String::from_utf8_lossy(&cmd.stdout);

    println!("cargo:rustc-env=SYMBOLICATOR_RELEASE={ver}");
    println!("cargo:rerun-if-env-changed=SYMBOLICATOR_RELEASE");
}

fn main() {
    emit_version_var();
    emit_release_var();
}
