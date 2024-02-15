use symbolicator_proguard::ProguardService;
use symbolicator_service::config::Config;
use symbolicator_service::services::SharedServices;
use symbolicator_test as test;

pub use test::{assert_snapshot, fixture, read_fixture, source_config, symbol_server, Server};

/// Setup tests and create a test service.
///
/// This function returns a tuple containing the service to test, and a temporary cache
/// directory. The directory is cleaned up when the [`TempDir`] instance is dropped. Keep it as
/// guard until the test has finished.
///
/// The service is configured with `connect_to_reserved_ips = True`. This allows to use a local
/// symbol server to test object file downloads.
/// The `update_config` closure can modify any default configuration if needed before the server is
/// started.
pub fn setup_service(update_config: impl FnOnce(&mut Config)) -> (ProguardService, test::TempDir) {
    test::setup();

    let cache_dir = test::tempdir();

    let mut config = Config {
        cache_dir: Some(cache_dir.path().to_owned()),
        connect_to_reserved_ips: true,
        ..Default::default()
    };
    update_config(&mut config);

    let handle = tokio::runtime::Handle::current();
    let shared_services = SharedServices::new(config, handle).unwrap();
    let proguard = ProguardService::new(&shared_services);

    (proguard, cache_dir)
}
