use std::future::Future;
use std::path::PathBuf;

use tokio::fs::File;

pub trait GenerateCachePath<K> {
    fn cache_path(&self, key: K, version: u32) -> PathBuf;
}

pub trait CacheAccess {
    type Key;
    type Output;
    type CreateFut: Future<Output = Self::Output>;

    fn key_path(&self, key: Self::Key) -> PathBuf;
    // TODO: make this return a future?
    fn load(&self, file: File) -> Option<Self::Output>;
    fn create(&self, file: File) -> Self::CreateFut;

    fn convert_err(&self, err: std::io::Error) -> Self::Output;
}

pub struct VersionedFsCache<A> {
    access: A,
}

impl<A> VersionedFsCache<A>
where
    A: CacheAccess,
{
    pub async fn get(&self, key: A::Key) -> A::Output {
        // try open file
        // if found:
        //   try load
        //   if load success: return
        // not found:
        //   create file async
        //   write file
        // return return
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use tokio::fs;
    use tokio::io::{AsyncSeekExt, AsyncWriteExt};

    use super::*;

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
