/// Same as [`try!`] but to be used in functions that return `Box<dyn Future>` instead of `Result`.
///
/// Useful when calling synchronous (but cheap enough) functions in async code.
#[macro_export]
macro_rules! tryf {
    ($e:expr) => {
        match $e {
            Ok(value) => value,
            Err(e) => {
                return Box::new(::futures01::future::err(::std::convert::From::from(e)))
                    as Box<dyn ::futures01::future::Future<Item = _, Error = _>>;
            }
        }
    };
}

/// Same as [`tryf`] but a hack for futures 0.3.  These futures can be replaced with
/// async/await code and then this hack goes away.
#[macro_export]
macro_rules! tryf03 {
    ($e:expr) => {
        match $e {
            Ok(value) => value,
            Err(e) => {
                return Box::new(::futures::future::err(::std::convert::From::from(e)))
                    as Box<dyn ::futures::future::Future<Output = _>>;
            }
        }
    };
}

/// Same as [`tryf`] but a hack for futures 0.3.  These futures can be replaced with
/// async/await code and then this hack goes away.
#[macro_export]
macro_rules! tryf03pin {
    ($e:expr) => {
        match $e {
            Ok(value) => value,
            Err(e) => {
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
