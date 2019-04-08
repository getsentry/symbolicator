/// Same as `try` but to be used in functions that return `Box<Future>` instead of `Result`.
///
/// Useful when calling synchronous (but cheap enough) functions in async code.
#[macro_export]
macro_rules! tryf {
    ($e:expr) => {
        match $e {
            Ok(value) => value,
            Err(e) => {
                return Box::new(::futures::future::err(::std::convert::From::from(e)))
                    as Box<dyn Future<Item = _, Error = _>>;
            }
        }
    };
}

/// Declare a closure that clone specific values before moving them into their bodies. Mainly
/// useful when using combinator functions such as `Future::and_then` or `Future::map`.
#[macro_export]
macro_rules! clone {
    ($($n:ident),+ , |$($p:pat),*| $body:expr) => (
        {
            $( let $n = $n.clone(); )+
            move |$($p),*| $body
        }
    );
}
