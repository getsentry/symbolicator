use futures::future;
use futures::sync::oneshot;
use lazy_static::lazy_static;

use crate::utils::futures::{RemoteFuture, ThreadPool};

lazy_static! {
    static ref FS_THREAD_POOL: ThreadPool = ThreadPool::new();
}

pub fn blocking_io<F, T, E>(f: F) -> RemoteFuture<T, E>
where
    T: 'static + Send,
    E: 'static + Send,
    F: FnOnce() -> Result<T, E> + 'static + Send,
{
    let (sender, receiver) = oneshot::channel();
    FS_THREAD_POOL.spawn(future::lazy(move || {
        sender.send(f()).ok();
        Ok(())
    }));
    RemoteFuture(receiver)
}
