use std::fs::{self, File};
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::sleep;
use std::time::{Duration, SystemTime};

use anyhow::Result;
use filetime::FileTime;
use futures::future::BoxFuture;
use symbolic::common::ByteView;
use tempfile::NamedTempFile;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};

use crate::config::{
    CacheConfig, CacheConfigs, DerivedCacheConfig, DiagnosticsCacheConfig, DownloadedCacheConfig,
};
use crate::test;

use super::cache_error::cache_entry_from_bytes;
use super::shared_cache::config::SharedCacheBackendConfig;
use super::*;

fn tempdir() -> io::Result<tempfile::TempDir> {
    tempfile::tempdir_in(".")
}

#[test]
fn test_cache_dir_created() {
    let basedir = tempdir().unwrap();
    let cachedir = basedir.path().join("cache");
    let config = Config {
        cache_dir: Some(cachedir.clone()),
        ..Default::default()
    };
    let _cache = Cache::from_config(
        CacheName::Objects,
        &config,
        CacheConfig::Downloaded(Default::default()),
        Default::default(),
        1024,
    );
    let fsinfo = fs::metadata(cachedir).unwrap();
    assert!(fsinfo.is_dir());
}

#[test]
fn test_caches_tmp_created() {
    let basedir = tempdir().unwrap();
    let cachedir = basedir.path().join("cache");
    let tmpdir = cachedir.join("tmp");

    let cfg = Config {
        cache_dir: Some(cachedir),
        ..Default::default()
    };
    let caches = Caches::from_config(&cfg).unwrap();
    caches.clear_tmp(&cfg).unwrap();

    let fsinfo = fs::metadata(tmpdir).unwrap();
    assert!(fsinfo.is_dir());
}

#[test]
fn test_caches_tmp_cleared() {
    let basedir = tempdir().unwrap();
    let cachedir = basedir.path().join("cache");
    let tmpdir = cachedir.join("tmp");

    fs::create_dir_all(&tmpdir).unwrap();
    let spam = tmpdir.join("spam");
    File::create(&spam).unwrap();
    let fsinfo = fs::metadata(&spam).unwrap();
    assert!(fsinfo.is_file());

    let cfg = Config {
        cache_dir: Some(cachedir),
        ..Default::default()
    };
    let caches = Caches::from_config(&cfg).unwrap();
    caches.clear_tmp(&cfg).unwrap();

    let fsinfo = fs::metadata(spam);
    assert!(fsinfo.is_err());
}

#[test]
fn test_max_unused_for() -> Result<()> {
    let tempdir = tempdir()?;
    let config = Config {
        cache_dir: Some(tempdir.path().to_path_buf()),
        ..Default::default()
    };
    fs::create_dir_all(tempdir.path().join("objects"))?;

    let cache = Cache::from_config(
        CacheName::Objects,
        &config,
        CacheConfig::Derived(DerivedCacheConfig {
            max_unused_for: Some(Duration::from_millis(50)),
            ..Default::default()
        }),
        Default::default(),
        1024,
    )?;

    File::create(tempdir.path().join("objects/killthis"))?.write_all(b"hi")?;
    File::create(tempdir.path().join("objects/keepthis"))?.write_all(b"")?;
    sleep(Duration::from_millis(100));

    File::create(tempdir.path().join("objects/keepthis2"))?.write_all(b"hi")?;
    cache.cleanup(false)?;

    let mut basenames: Vec<_> = fs::read_dir(tempdir.path().join("objects"))?
        .map(|x| x.unwrap().file_name().into_string().unwrap())
        .collect();

    basenames.sort();

    assert_eq!(basenames, vec!["keepthis", "keepthis2"]);

    Ok(())
}

#[test]
fn test_retry_misses_after() -> Result<()> {
    let tempdir = tempdir()?;
    let config = Config {
        cache_dir: Some(tempdir.path().to_path_buf()),
        ..Default::default()
    };
    fs::create_dir_all(tempdir.path().join("objects"))?;

    let cache = Cache::from_config(
        CacheName::Objects,
        &config,
        CacheConfig::Derived(DerivedCacheConfig {
            retry_misses_after: Some(Duration::from_millis(50)),
            ..Default::default()
        }),
        Default::default(),
        1024,
    )?;

    File::create(tempdir.path().join("objects/keepthis"))?.write_all(b"hi")?;
    File::create(tempdir.path().join("objects/killthis"))?.write_all(b"")?;
    sleep(Duration::from_millis(100));

    File::create(tempdir.path().join("objects/keepthis2"))?.write_all(b"")?;
    cache.cleanup(false)?;

    let mut basenames: Vec<_> = fs::read_dir(tempdir.path().join("objects"))?
        .map(|x| x.unwrap().file_name().into_string().unwrap())
        .collect();

    basenames.sort();

    assert_eq!(basenames, vec!["keepthis", "keepthis2"]);

    Ok(())
}

#[test]
fn test_cleanup_malformed() -> Result<()> {
    let tempdir = tempdir()?;
    let config = Config {
        cache_dir: Some(tempdir.path().to_path_buf()),
        ..Default::default()
    };
    fs::create_dir_all(tempdir.path().join("objects"))?;

    // File has same amount of chars as "malformed", check that optimization works
    File::create(tempdir.path().join("objects/keepthis"))?.write_all(b"addictive")?;
    File::create(tempdir.path().join("objects/keepthis2"))?.write_all(b"hi")?;
    File::create(tempdir.path().join("objects/keepthis3"))?.write_all(b"honkhonkbeepbeep")?;

    File::create(tempdir.path().join("objects/killthis"))?.write_all(b"malformed")?;
    File::create(tempdir.path().join("objects/killthis2"))?.write_all(b"malformedhonk")?;

    sleep(Duration::from_millis(10));

    // Creation of this struct == "process startup", this tests that all malformed files created
    // before startup are cleaned
    let cache = Cache::from_config(
        CacheName::Objects,
        &config,
        CacheConfig::Derived(DerivedCacheConfig {
            retry_misses_after: Some(Duration::from_millis(20)),
            ..Default::default()
        }),
        Default::default(),
        1024,
    )?;

    cache.cleanup(false)?;

    let mut basenames: Vec<_> = fs::read_dir(tempdir.path().join("objects"))?
        .map(|x| x.unwrap().file_name().into_string().unwrap())
        .collect();

    basenames.sort();

    assert_eq!(basenames, vec!["keepthis", "keepthis2", "keepthis3"]);

    Ok(())
}

#[test]
fn test_cleanup_cache_download() -> Result<()> {
    let tempdir = tempdir()?;
    let config = Config {
        cache_dir: Some(tempdir.path().to_path_buf()),
        ..Default::default()
    };
    fs::create_dir_all(tempdir.path().join("objects"))?;

    File::create(tempdir.path().join("objects/keepthis"))?.write_all(b"beeep")?;
    File::create(tempdir.path().join("objects/keepthis2"))?.write_all(b"hi")?;
    File::create(tempdir.path().join("objects/keepthis3"))?.write_all(b"honkhonkbeepbeep")?;

    File::create(tempdir.path().join("objects/killthis"))?.write_all(b"downloaderror")?;
    File::create(tempdir.path().join("objects/killthis2"))?.write_all(b"downloaderrorhonk")?;
    File::create(tempdir.path().join("objects/killthis3"))?.write_all(b"downloaderrormalformed")?;
    File::create(tempdir.path().join("objects/killthis4"))?.write_all(b"malformeddownloaderror")?;

    let cache = Cache::from_config(
        CacheName::Objects,
        &config,
        CacheConfig::Downloaded(DownloadedCacheConfig {
            retry_misses_after: Some(Duration::from_millis(20)),
            ..Default::default()
        }),
        Default::default(),
        1024,
    )?;

    sleep(Duration::from_millis(30));

    cache.cleanup(false)?;

    let mut basenames: Vec<_> = fs::read_dir(tempdir.path().join("objects"))?
        .map(|x| x.unwrap().file_name().into_string().unwrap())
        .collect();

    basenames.sort();

    assert_eq!(basenames, vec!["keepthis", "keepthis2", "keepthis3"]);

    Ok(())
}

fn expiration_strategy(path: &Path) -> io::Result<ExpirationStrategy> {
    let bv = ByteView::open(path)?;
    let cache_entry = cache_entry_from_bytes(bv);
    Ok(super::fs::expiration_strategy(&cache_entry))
}

#[test]
fn test_expiration_strategy_positive() -> Result<()> {
    let tempdir = tempdir()?;
    fs::create_dir_all(tempdir.path().join("honk"))?;

    File::create(tempdir.path().join("honk/keepbeep"))?.write_all(b"toot")?;
    File::create(tempdir.path().join("honk/keepbeep2"))?.write_all(b"honk")?;
    File::create(tempdir.path().join("honk/keepbeep3"))?.write_all(b"honkhonkbeepbeep")?;
    File::create(tempdir.path().join("honk/keepbeep4"))?.write_all(b"malform")?;
    File::create(tempdir.path().join("honk/keepbeep5"))?.write_all(b"dler")?;

    assert_eq!(
        expiration_strategy(tempdir.path().join("honk/keepbeep").as_path())?,
        ExpirationStrategy::None,
    );
    assert_eq!(
        expiration_strategy(tempdir.path().join("honk/keepbeep2").as_path())?,
        ExpirationStrategy::None,
    );
    assert_eq!(
        expiration_strategy(tempdir.path().join("honk/keepbeep3").as_path())?,
        ExpirationStrategy::None,
    );
    assert_eq!(
        expiration_strategy(tempdir.path().join("honk/keepbeep4").as_path())?,
        ExpirationStrategy::None,
    );
    assert_eq!(
        expiration_strategy(tempdir.path().join("honk/keepbeep5").as_path())?,
        ExpirationStrategy::None,
    );

    Ok(())
}

#[test]
fn test_expiration_strategy_negative() -> Result<()> {
    let tempdir = tempdir()?;
    fs::create_dir_all(tempdir.path().join("honk"))?;

    File::create(tempdir.path().join("honk/retrybeep"))?.write_all(b"")?;

    assert_eq!(
        expiration_strategy(tempdir.path().join("honk/retrybeep").as_path())?,
        ExpirationStrategy::Negative,
    );

    Ok(())
}

#[test]
fn test_expiration_strategy_malformed() -> Result<()> {
    let tempdir = tempdir()?;
    fs::create_dir_all(tempdir.path().join("honk"))?;

    File::create(tempdir.path().join("honk/badbeep"))?.write_all(b"malformed")?;
    File::create(tempdir.path().join("honk/badbeep2"))?.write_all(b"malformedhonkbeep")?;
    File::create(tempdir.path().join("honk/badbeep3"))?.write_all(b"malformeddownloaderror")?;

    assert_eq!(
        expiration_strategy(tempdir.path().join("honk/badbeep").as_path())?,
        ExpirationStrategy::Malformed,
    );
    assert_eq!(
        expiration_strategy(tempdir.path().join("honk/badbeep2").as_path())?,
        ExpirationStrategy::Malformed,
    );
    assert_eq!(
        expiration_strategy(tempdir.path().join("honk/badbeep3").as_path())?,
        ExpirationStrategy::Malformed,
    );

    Ok(())
}

#[test]
fn test_expiration_strategy_downloaderror() -> Result<()> {
    let tempdir = tempdir()?;
    fs::create_dir_all(tempdir.path().join("honk"))?;

    File::create(tempdir.path().join("honk/badbeep"))?.write_all(b"downloaderror")?;
    File::create(tempdir.path().join("honk/badbeep2"))?.write_all(b"downloaderrorhonkbeep")?;
    File::create(tempdir.path().join("honk/badbeep3"))?.write_all(b"downloaderrormalformed")?;

    assert_eq!(
        expiration_strategy(tempdir.path().join("honk/badbeep").as_path())?,
        ExpirationStrategy::Negative,
    );
    assert_eq!(
        expiration_strategy(tempdir.path().join("honk/badbeep2").as_path())?,
        ExpirationStrategy::Negative,
    );
    assert_eq!(
        expiration_strategy(tempdir.path().join("honk/badbeep3").as_path())?,
        ExpirationStrategy::Negative,
    );
    Ok(())
}

#[test]
fn test_open_cachefile() -> Result<()> {
    // Assert that opening a cache touches the mtime but does not invalidate it.
    let tempdir = tempdir()?;
    let config = Config {
        cache_dir: Some(tempdir.path().to_path_buf()),
        ..Default::default()
    };
    let cache = Cache::from_config(
        CacheName::Objects,
        &config,
        CacheConfig::Downloaded(Default::default()),
        Default::default(),
        1024,
    )?;

    // Create a file in the cache, with mtime of 1h 15s ago since it only gets touched
    // if more than an hour old.
    let path = tempdir.path().join("objects/hello");
    File::create(&path)?.write_all(b"world")?;
    let now_unix = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)?
        .as_secs();
    let old_mtime_unix = (now_unix - 3600 - 15).try_into()?;
    filetime::set_file_mtime(&path, FileTime::from_unix_time(old_mtime_unix, 0))?;

    let old_mtime = fs::metadata(&path)?.modified()?;

    // Open it with the cache, check contents and new mtime.
    let (entry, _expiration) = cache.open_cachefile(&path)?.expect("No file found");
    assert_eq!(entry.unwrap().as_slice(), b"world");

    let new_mtime = fs::metadata(&path)?.modified()?;
    assert!(old_mtime < new_mtime);

    Ok(())
}

#[test]
fn test_cleanup() {
    let tempdir = tempdir().unwrap();

    // Create entries in our caches that are an hour old.
    let mtime = FileTime::from_system_time(SystemTime::now() - Duration::from_secs(3600));

    let create = |cache_name: &str| {
        let dir = tempdir.path().join(cache_name);
        fs::create_dir(&dir).unwrap();
        let entry = dir.join("entry");
        fs::write(&entry, "contents").unwrap();
        filetime::set_file_mtime(&entry, mtime).unwrap();
        entry
    };

    let object_entry = create("objects");
    let object_meta_entry = create("object_meta");
    let auxdifs_entry = create("auxdifs");
    let symcaches_entry = create("symcaches");
    let cficaches_entry = create("cficaches");
    let diagnostics_entry = create("diagnostics");

    // Configure the caches to expire after 1 minute.
    let caches = Caches::from_config(&Config {
        cache_dir: Some(tempdir.path().to_path_buf()),
        caches: CacheConfigs {
            downloaded: DownloadedCacheConfig {
                max_unused_for: Some(Duration::from_secs(60)),
                ..Default::default()
            },
            derived: DerivedCacheConfig {
                max_unused_for: Some(Duration::from_secs(60)),
                ..Default::default()
            },
            diagnostics: DiagnosticsCacheConfig {
                retention: Some(Duration::from_secs(60)),
            },
            ..Default::default()
        },
        ..Default::default()
    })
    .unwrap();

    // Finally do some testing
    assert!(object_entry.is_file());
    assert!(object_meta_entry.is_file());
    assert!(auxdifs_entry.is_file());
    assert!(symcaches_entry.is_file());
    assert!(cficaches_entry.is_file());
    assert!(diagnostics_entry.is_file());

    caches.cleanup(false).unwrap();

    assert!(!object_entry.is_file());
    assert!(!object_meta_entry.is_file());
    assert!(!auxdifs_entry.is_file());
    assert!(!symcaches_entry.is_file());
    assert!(!cficaches_entry.is_file());
    assert!(!diagnostics_entry.is_file());
}

#[tokio::test]
async fn test_cache_error_write_negative() -> Result<()> {
    let dir = tempdir()?;
    let path = dir.path().join("honk");

    // copying what compute does here instead of just using
    // tokio::fs::File::create() directly
    let sync_file = File::create(&path)?;
    let mut async_file = tokio::fs::File::from_std(sync_file);
    let error = CacheError::NotFound;
    error.write(&mut async_file).await?;

    // make sure write leaves the cursor at the end
    let current_pos = async_file.stream_position().await?;
    assert_eq!(current_pos, 0);

    let contents = fs::read(&path)?;
    assert_eq!(contents, b"");

    Ok(())
}

#[tokio::test]
async fn test_cache_error_write_negative_with_garbage() -> Result<()> {
    let dir = tempdir()?;
    let path = dir.path().join("honk");

    // copying what compute does here instead of just using
    // tokio::fs::File::create() directly
    let sync_file = File::create(&path)?;
    let mut async_file = tokio::fs::File::from_std(sync_file);
    async_file.write_all(b"beep").await?;
    let error = CacheError::NotFound;
    error.write(&mut async_file).await?;

    // make sure write leaves the cursor at the end
    let current_pos = async_file.stream_position().await?;
    assert_eq!(current_pos, 0);

    let contents = fs::read(&path)?;
    assert_eq!(contents, b"");

    Ok(())
}

#[tokio::test]
async fn test_cache_error_write_malformed() -> Result<()> {
    let dir = tempdir()?;
    let path = dir.path().join("honk");

    // copying what compute does here instead of just using
    // tokio::fs::File::create() directly
    let sync_file = File::create(&path)?;
    let mut async_file = tokio::fs::File::from_std(sync_file);

    let error_message = "unsupported object file format";
    let error = CacheError::Malformed(error_message.to_owned());
    error.write(&mut async_file).await?;

    // make sure write leaves the cursor at the end
    let current_pos = async_file.stream_position().await?;
    assert_eq!(
        current_pos as usize,
        CacheError::MALFORMED_MARKER.len() + error_message.len()
    );

    let contents = fs::read(&path)?;

    let mut expected: Vec<u8> = Vec::new();
    expected.extend(CacheError::MALFORMED_MARKER);
    expected.extend(error_message.as_bytes());

    assert_eq!(contents, expected);

    Ok(())
}

#[tokio::test]
async fn test_cache_error_write_malformed_truncates() -> Result<()> {
    let dir = tempdir()?;
    let path = dir.path().join("honk");

    // copying what compute does here instead of just using
    // tokio::fs::File::create() directly
    let sync_file = File::create(&path)?;
    let mut async_file = tokio::fs::File::from_std(sync_file);

    async_file
        .write_all(b"i'm a little teapot short and stout here is my handle and here is my spout")
        .await?;

    let error_message = "unsupported object file format";
    let error = CacheError::Malformed(error_message.to_owned());
    error.write(&mut async_file).await?;

    // make sure write leaves the cursor at the end
    let current_pos = async_file.stream_position().await?;
    assert_eq!(
        current_pos as usize,
        CacheError::MALFORMED_MARKER.len() + error_message.len()
    );

    let contents = fs::read(&path)?;

    let mut expected: Vec<u8> = Vec::new();
    expected.extend(CacheError::MALFORMED_MARKER);
    expected.extend(error_message.as_bytes());

    assert_eq!(contents, expected);

    Ok(())
}

#[tokio::test]
async fn test_cache_error_write_cache_error() -> Result<()> {
    let dir = tempdir()?;
    let path = dir.path().join("honk");

    // copying what compute does here instead of just using
    // tokio::fs::File::create() directly
    let sync_file = File::create(&path)?;
    let mut async_file = tokio::fs::File::from_std(sync_file);

    let error = CacheError::PermissionDenied("".into());
    error.write(&mut async_file).await?;

    // make sure write leaves the cursor at the end
    let current_pos = async_file.stream_position().await?;
    assert_eq!(
        current_pos as usize,
        CacheError::PERMISSION_DENIED_MARKER.len()
    );

    let contents = fs::read(&path)?;

    let mut expected: Vec<u8> = Vec::new();
    expected.extend(CacheError::PERMISSION_DENIED_MARKER);

    assert_eq!(contents, expected);

    Ok(())
}

#[tokio::test]
async fn test_cache_error_write_cache_error_truncates() -> Result<()> {
    let dir = tempdir()?;
    let path = dir.path().join("honk");

    // copying what compute does here instead of just using
    // tokio::fs::File::create() directly
    let sync_file = File::create(&path)?;
    let mut async_file = tokio::fs::File::from_std(sync_file);

    async_file
        .write_all(b"i'm a little teapot short and stout here is my handle and here is my spout")
        .await?;

    let error = CacheError::PermissionDenied("".into());
    error.write(&mut async_file).await?;

    // make sure write leaves the cursor at the end
    let current_pos = async_file.stream_position().await?;
    assert_eq!(
        current_pos as usize,
        CacheError::PERMISSION_DENIED_MARKER.len()
    );

    let contents = fs::read(&path)?;

    let mut expected: Vec<u8> = Vec::new();
    expected.extend(CacheError::PERMISSION_DENIED_MARKER);

    assert_eq!(contents, expected);

    Ok(())
}

#[test]
fn test_shared_cache_config_filesystem_common_defaults() {
    let yaml = r#"
            filesystem:
              path: "/path/to/somewhere"
        "#;
    let cfg: SharedCacheConfig = serde_yaml::from_reader(yaml.as_bytes()).unwrap();

    assert_eq!(cfg.max_upload_queue_size, 400);
    assert_eq!(cfg.max_concurrent_uploads, 20);
    match cfg.backend {
        SharedCacheBackendConfig::Gcs(_) => panic!("wrong backend"),
        SharedCacheBackendConfig::Filesystem(cfg) => {
            assert_eq!(cfg.path, Path::new("/path/to/somewhere"))
        }
    }
}

#[test]
fn test_shared_cache_config_common_settings() {
    let yaml = r#"
            max_upload_queue_size: 50
            max_concurrent_uploads: 50
            filesystem:
              path: "/path/to/somewhere"
        "#;
    let cfg: SharedCacheConfig = serde_yaml::from_reader(yaml.as_bytes()).unwrap();

    assert_eq!(cfg.max_upload_queue_size, 50);
    assert_eq!(cfg.max_concurrent_uploads, 50);
    assert!(matches!(
        cfg.backend,
        SharedCacheBackendConfig::Filesystem(_)
    ));
}

#[test]
fn test_shared_cache_config_gcs() {
    let yaml = r#"
            gcs:
              bucket: "some-bucket"
        "#;
    let cfg: SharedCacheConfig = serde_yaml::from_reader(yaml.as_bytes()).unwrap();

    match cfg.backend {
        SharedCacheBackendConfig::Gcs(gcs) => {
            assert_eq!(gcs.bucket, "some-bucket");
            assert!(gcs.service_account_path.is_none());
        }
        SharedCacheBackendConfig::Filesystem(_) => panic!("wrong backend"),
    }
}

#[test]
fn test_cache_entry() {
    fn read_cache_entry(bytes: &'static [u8]) -> CacheEntry<String> {
        cache_entry_from_bytes(ByteView::from_slice(bytes))
            .map(|bv| String::from_utf8_lossy(bv.as_slice()).into_owned())
    }

    let not_found = b"";

    assert_eq!(read_cache_entry(not_found), Err(CacheError::NotFound));

    let malformed = b"malformedDoesn't look like anything to me";

    assert_eq!(
        read_cache_entry(malformed),
        Err(CacheError::Malformed(
            "Doesn't look like anything to me".into()
        ))
    );

    let timeout = b"timeout4m33s";

    assert_eq!(
        read_cache_entry(timeout),
        Err(CacheError::Timeout(Duration::from_secs(273)))
    );

    let download_error = b"downloaderrorSomeone unplugged the internet";

    assert_eq!(
        read_cache_entry(download_error),
        Err(CacheError::DownloadError(
            "Someone unplugged the internet".into()
        ))
    );

    let permission_denied = b"permissiondeniedI'm sorry Dave, I'm afraid I can't do that";

    assert_eq!(
        read_cache_entry(permission_denied),
        Err(CacheError::PermissionDenied(
            "I'm sorry Dave, I'm afraid I can't do that".into()
        ))
    );

    let all_good = b"Not any of the error cases";

    assert_eq!(
        read_cache_entry(all_good),
        Ok("Not any of the error cases".into())
    );
}

#[derive(Clone, Default)]
struct TestCacheItem {
    computations: Arc<AtomicUsize>,
}

impl TestCacheItem {
    fn new() -> Self {
        Self {
            computations: Default::default(),
        }
    }
}

impl CacheItemRequest for TestCacheItem {
    type Item = String;

    const VERSIONS: CacheVersions = CacheVersions {
        current: 1,
        fallbacks: &[0],
    };

    fn compute<'a>(&'a self, temp_file: &'a mut NamedTempFile) -> BoxFuture<'a, CacheEntry> {
        self.computations.fetch_add(1, Ordering::SeqCst);

        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;

            fs::write(temp_file.path(), "some new cached contents")?;
            Ok(())
        })
    }

    fn load(&self, data: ByteView<'static>) -> CacheEntry<Self::Item> {
        Ok(std::str::from_utf8(data.as_slice()).unwrap().to_owned())
    }
}

/// This test asserts that the cache is served from outdated cache files, and that a computation
/// is being kicked off (and deduplicated) in the background
#[tokio::test]
async fn test_cache_fallback() {
    test::setup();
    let cache_dir = test::tempdir();

    let request = TestCacheItem::new();
    let key = CacheKey::for_testing("global/some_cache_key");

    {
        let cache_dir = cache_dir.path().join("objects");
        let cache_file = cache_dir.join(key.cache_path(0));
        fs::create_dir_all(cache_file.parent().unwrap()).unwrap();
        fs::write(cache_file, "some old cached contents").unwrap();
    }

    let config = Config {
        cache_dir: Some(cache_dir.path().to_path_buf()),
        ..Default::default()
    };
    let cache = Cache::from_config(
        CacheName::Objects,
        &config,
        CacheConfig::from(CacheConfigs::default().derived),
        Arc::new(AtomicIsize::new(1)),
        1024,
    )
    .unwrap();
    let cacher = Cacher::new(cache, Default::default());

    let first_result = cacher.compute_memoized(request.clone(), key.clone()).await;
    assert_eq!(first_result.unwrap().as_str(), "some old cached contents");

    let second_result = cacher.compute_memoized(request.clone(), key.clone()).await;
    assert_eq!(second_result.unwrap().as_str(), "some old cached contents");

    tokio::time::sleep(Duration::from_millis(200)).await;

    let third_result = cacher.compute_memoized(request.clone(), key).await;
    assert_eq!(third_result.unwrap().as_str(), "some new cached contents");

    // we only want to have the actual computation be done a single time
    assert_eq!(request.computations.load(Ordering::SeqCst), 1);
}

/// Makes sure that a `NotFound` result does not fall back to older cache versions.
#[tokio::test]
async fn test_cache_fallback_notfound() {
    test::setup();
    let cache_dir = test::tempdir();

    let request = TestCacheItem::new();
    let key = CacheKey::for_testing("global/some_cache_key");

    {
        let cache_dir = cache_dir.path().join("objects");
        let cache_file = cache_dir.join(key.cache_path(0));
        fs::create_dir_all(cache_file.parent().unwrap()).unwrap();
        fs::write(cache_file, "some old cached contents").unwrap();

        let cache_file = cache_dir.join(key.cache_path(1));
        fs::create_dir_all(cache_file.parent().unwrap()).unwrap();
        fs::write(cache_file, "").unwrap();
    }

    let config = Config {
        cache_dir: Some(cache_dir.path().to_path_buf()),
        ..Default::default()
    };
    let cache = Cache::from_config(
        CacheName::Objects,
        &config,
        CacheConfig::from(CacheConfigs::default().derived),
        Arc::new(AtomicIsize::new(1)),
        1024,
    )
    .unwrap();
    let cacher = Cacher::new(cache, Default::default());

    let first_result = cacher.compute_memoized(request.clone(), key).await;
    assert_eq!(first_result, Err(CacheError::NotFound));

    // no computation should be done
    assert_eq!(request.computations.load(Ordering::SeqCst), 0);
}

/// This test asserts that the bounded maximum number of recomputations is not exceeded.
#[tokio::test]
async fn test_lazy_computation_limit() {
    test::setup();

    let config = Config {
        cache_dir: Some(test::tempdir().path().to_path_buf()),
        ..Default::default()
    };
    let cache = Cache::from_config(
        CacheName::Objects,
        &config,
        CacheConfig::from(CacheConfigs::default().derived),
        Arc::new(AtomicIsize::new(1)),
        1024,
    )
    .unwrap();
    let cache_dir = cache.cache_dir.clone().unwrap();
    let cacher = Cacher::new(cache, Default::default());

    let keys = &["global/1", "global/2", "global/3"];
    let request = TestCacheItem::new();

    for key in keys {
        let request = request.clone();
        let key = CacheKey::for_testing(*key);

        let cache_file = cache_dir.join(key.cache_path(0));
        fs::create_dir_all(cache_file.parent().unwrap()).unwrap();
        fs::write(cache_file, "some old cached contents").unwrap();

        let result = cacher.compute_memoized(request.clone(), key).await;
        assert_eq!(result.unwrap().as_str(), "some old cached contents");
    }

    tokio::time::sleep(Duration::from_millis(200)).await;

    // we want the actual computation to be done only one time, as that is the
    // maximum number of lazy computations.
    assert_eq!(request.computations.load(Ordering::SeqCst), 1);

    // double check that we actually get outdated contents for two of the requests.
    let mut num_outdated = 0;

    for key in keys {
        let request = request.clone();
        let key = CacheKey::for_testing(*key);

        let result = cacher.compute_memoized(request.clone(), key).await;
        if result.unwrap().as_str() == "some old cached contents" {
            num_outdated += 1;
        }
    }

    assert_eq!(num_outdated, 2);
}
