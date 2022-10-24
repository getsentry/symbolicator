use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};

use tempfile::NamedTempFile;
use tokio::fs;

/// Driver for the Filesystem Cache Layer.
///
/// The driver is responsible for loading an item from a file on disk,
/// or to write an item into an os-disk file.
pub trait FsCacheDriver {
    /// Input argument to the driver.
    type Arg;
    /// The resulting output that is read from / writen to disk.
    type Output;

    /// The future type for the `create` trait fn.
    type CreateFut: Future<Output = io::Result<Self::Output>>;

    /// The desired path corresponding to the `arg`.
    fn cache_path(&self, arg: &Self::Arg) -> PathBuf;

    /// Loads the output from the `file`.
    ///
    /// Returning `None` signals the [`FsCache`] to remove the existing file and to
    /// try recreating it.
    // TODO: make this return a future and take a tokio file instead?
    fn load(&self, file: std::fs::File) -> Option<Self::Output>;

    /// Write the cache item out into `file`.
    ///
    /// This should create a new cache item and write it out into `file`.
    fn create(&self, arg: Self::Arg, file: fs::File) -> Self::CreateFut;

    /// How to create an output from an [`io::Error`].
    ///
    /// The [`FsCache`] will always return a valid output, even in the presence of
    /// unexpected errors.
    fn output_for_err(&self, err: io::Error) -> Self::Output;
}

/// A Filesystem Cache Layer.
///
/// The Cache abstracts away loading cached items from disk, and writing items onto disk.
pub struct FsCache<D> {
    driver: D,
}

impl<D> FsCache<D>
where
    D: FsCacheDriver,
{
    /// Creates a new Filesystem Cache.
    pub fn new(driver: D) -> Self {
        Self { driver }
    }

    /// Load the output value for the provided `arg` from disk.
    pub async fn get(&self, arg: D::Arg) -> D::Output {
        let cache_path = self.driver.cache_path(&arg);

        macro_rules! try_err {
            ($expr:expr) => {
                match $expr {
                    Ok(output) => output,
                    Err(err) => return self.driver.output_for_err(err),
                }
            };
        }

        match fs::OpenOptions::new().read(true).open(&cache_path).await {
            Ok(file) => {
                let std_file = file.into_std().await;
                if let Some(output) = self.driver.load(std_file) {
                    // everything good
                    return output;
                }
                // else: file not usable, remove and recreate

                // TODO: log, ignore or return error?
                let _ = fs::remove_file(&cache_path).await;
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                // fall through -> create cache
            }
            Err(err) => return self.driver.output_for_err(err),
        }

        let tempfile = try_err!(create_tempfile(&cache_path));
        let file = try_err!(tempfile.reopen());
        let file = fs::File::from_std(file);

        let output = try_err!(self.driver.create(arg, file).await);

        // TODO: log, ignore or return error?
        let _ = tempfile.persist(cache_path);

        output
    }
}

fn create_tempfile(path: &Path) -> io::Result<NamedTempFile> {
    // TODO: handle errors
    // TODO: should we enforce a parent dir?
    let parent_path = path.parent().unwrap();
    std::fs::create_dir_all(parent_path)?;

    tempfile::Builder::new()
        .prefix("tmp")
        .tempfile_in(parent_path)
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::fs;
    use tokio::io::{AsyncSeekExt, AsyncWriteExt};

    use super::*;

    struct Driver {
        calls: AtomicUsize,
        path: PathBuf,
    }

    impl FsCacheDriver for Driver {
        type Arg = &'static str;

        type Output = String;

        type CreateFut = Pin<Box<dyn Future<Output = io::Result<Self::Output>>>>;

        fn cache_path(&self, arg: &Self::Arg) -> PathBuf {
            self.path.join(arg)
        }

        fn load(&self, mut file: std::fs::File) -> Option<Self::Output> {
            let mut output = String::new();
            file.read_to_string(&mut output).ok()?;

            // simulate failure to load -> triggers recreate
            if output == "1" {
                return None;
            }

            Some(output)
        }

        fn create(&self, _arg: Self::Arg, mut file: fs::File) -> Self::CreateFut {
            let output = self.calls.fetch_add(1, Ordering::Relaxed);
            Box::pin(async move {
                let output = format!("{}", output);
                file.write_all(output.as_bytes()).await?;

                Ok(output)
            })
        }

        fn output_for_err(&self, err: io::Error) -> Self::Output {
            format!("{}", err)
        }
    }

    #[tokio::test]
    async fn test_cached_file() {
        let dir = tempfile::tempdir().unwrap();

        let driver = Driver {
            calls: AtomicUsize::default(),
            path: dir.path().to_path_buf(),
        };
        let file_path = dir.path().join("file_a");

        let cache = FsCache::new(driver);

        // create:
        let _item = cache.get("file_a").await;
        // load:
        let item = cache.get("file_a").await;
        assert_eq!(&item, "0");

        // re-create:
        fs::remove_file(&file_path).await.unwrap();
        let item = cache.get("file_a").await;
        assert_eq!(&item, "1");

        // trying to load "1" will fail and re-create as "2":
        let item = cache.get("file_a").await;
        assert_eq!(&item, "2");
        let item = cache.get("file_a").await;
        assert_eq!(&item, "2");

        dir.close().unwrap();
    }

    /// just some testing for write/rename/remove of opened file handles.
    #[ignore]
    #[tokio::test]
    async fn test_move_open_file() {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open("foo.txt")
            .await
            .unwrap();

        dbg!(&file);

        fs::rename("foo.txt", "bar.txt").await.unwrap();
        dbg!(&file);

        file.write_all("foo renamed bar".as_bytes()).await.unwrap();

        fs::remove_file("bar.txt").await.unwrap();
        dbg!(&file);

        file.rewind().await.unwrap();
        file.write_all("bar deleted".as_bytes()).await.unwrap();
    }
}
