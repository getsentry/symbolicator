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
