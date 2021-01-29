/// Same as [`try!`] but to be used in functions that return `Pin<Box<dyn Future>>`.
///
/// When these futures can be replaced with async/await code and then this hack goes away.
#[macro_export]
macro_rules! tryf {
    ($e:expr) => {
        match $e {
            Ok(value) => value,
            Err(e) => {
                use ::std::boxed::Box;
                use ::std::pin::Pin;
                return Box::pin(::futures::future::err(::std::convert::From::from(e)))
                    as Pin<Box<dyn ::futures::future::Future<Output = _>>>;
            }
        }
    };
}

/// Declare a closure that clone specific values before moving them into their bodies. Mainly useful
/// when using combinator functions such as [`Future::and_then`](futures01::Future::and_then) or
/// [`Future::map`](futures01::Future::map).
#[macro_export]
macro_rules! clone {
    ($($n:ident),+ , || $body:expr) => (
        {
            $( let $n = $n.clone(); )+
            move || $body
        }
    );

    ($($n:ident),+ , |$($p:pat),*| $body:expr) => (
        {
            $( let $n = $n.clone(); )+
            move |$($p),*| $body
        }
    );

    ($($n:ident),+ , |$($p:ident : $t:ty),*| $body:expr) => (
        {
            $( let $n = $n.clone(); )+
            move |$($p : $t),*| $body
        }
    );
}

macro_rules! compat_handler {
    ($func:ident , $($param:ident),*) => {{
        use ::futures::{FutureExt, TryFutureExt};
        use ::sentry::SentryFutureExt;

        |__hub: crate::utils::sentry::ActixHub, $($param),*| {
            $func ( $($param),* )
                .bind_hub(__hub)
                .boxed_local()
                .compat()
        }
    }};
}
