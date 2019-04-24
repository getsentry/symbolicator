use std::io;
use std::process::{Command, Stdio};

fn main() -> Result<(), io::Error> {
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
    println!("cargo:rerun-if-changed=.git/config");

    Ok(())
}
