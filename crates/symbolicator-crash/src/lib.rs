use std::path::PathBuf;

const CRASH_REPORTER_PROCESS_ENV_VAR: &str = "_SYMBOLICATOR_CRASH_REPORTER_PROCESS";

/// Returns `true` if the current process is the crash reporting process.
pub fn is_crash_reporter_process() -> bool {
    std::env::var_os(CRASH_REPORTER_PROCESS_ENV_VAR).is_some()
}

/// Initializes a secondary watcher process for reporting fatal crashes.
///
/// This creates a secondary process, spawned from the same binary, communicating
/// with the original process via IPC. If the primary process crashes,
/// the secondary process will capture a minidump and report it to Sentry, delaying
/// the shutdown of the crashed process until the minidump has been uploaded.
pub fn init(crash_db: PathBuf, client: sentry::Client) {
    use sentry::{self, protocol};

    let child = minidumper_child::MinidumperChild::new()
        .with_server_env_var(CRASH_REPORTER_PROCESS_ENV_VAR.to_owned())
        .with_crashes_dir(crash_db.clone())
        .on_process(|command| {
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;
                command.arg0("symbolicator-crash");
            }
            // Skip arg0, the process name.
            command.args(std::env::args().skip(1));
        })
        .on_minidump(move |buffer, path| {
            tracing::info!("Minidump captured ({})", path.display());

            sentry::with_scope(
                |scope| {
                    // Remove event.process because this event came from the
                    // main app process
                    scope.remove_extra("event.process");

                    let filename = path
                        .file_name()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "minidump.dmp".to_owned());

                    scope.add_attachment(protocol::Attachment {
                        buffer,
                        filename,
                        ty: Some(protocol::AttachmentType::Minidump),
                        ..Default::default()
                    });
                },
                || {
                    sentry::capture_event(protocol::Event {
                        level: protocol::Level::Fatal,
                        ..Default::default()
                    })
                },
            );

            // We need to flush because the server will exit after this closure returns
            client.flush(Some(std::time::Duration::from_secs(15)));
        });

    // Make sure the implementations agree on the detection.
    debug_assert_eq!(
        is_crash_reporter_process(),
        child.is_crash_reporter_process()
    );

    if child.is_crash_reporter_process() {
        // Set the event.process so that it's obvious when Rust events come from
        // the crash reporter process rather than the main app process
        sentry::configure_scope(|scope| {
            scope.set_extra("event.process", "crash-reporter".to_owned().into());
        });
    } else {
        tracing::info!("Initializing crash handler in {}", crash_db.display());
    }

    match child.spawn() {
        Ok(handle) => {
            tracing::info!("Crash handler initialized");
            // Dropping the handle would detach the crash handler not catching any crashes.
            std::mem::forget(handle);
        }
        Err(err) if is_crash_reporter_process() => {
            tracing::warn!(
                error = &err as &dyn std::error::Error,
                "Failed to initialize crash reporter"
            );
            // In this case we need to manually abort the process,
            // otherwise the forked process will continue to act as
            // another Symbolicator.
            std::process::exit(1);
        }
        Err(err) => {
            tracing::warn!(
                error = &err as &dyn std::error::Error,
                "Failed to initialize crash reporting process"
            );
        }
    }
}
