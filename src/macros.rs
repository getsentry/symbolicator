#[macro_export]
macro_rules! tryf {
    ($e:expr) => {
        match $e {
            Ok(value) => value,
            Err(e) => {
                return Box::new(::futures::future::err(::std::convert::From::from(e)))
                    as Box<dyn Future<Item = _, Error = _>>
            }
        }
    };
}

#[macro_export]
macro_rules! tryfa {
    ($e:expr) => {
        match $e {
            Ok(value) => value,
            Err(e) => return Box::new(::actix::fut::result(Err(::std::convert::From::from(e)))),
        }
    };
}

#[macro_export]
macro_rules! clone {
    (@param _) => ( _ );
    (@param $x:ident) => ( $x );
    ($($n:ident),+ , || $body:expr) => (
        {
            $( let $n = $n.clone(); )+
            move || $body
        }
    );
    ($($n:ident),+ , |$($p:tt),+| $body:expr) => (
        {
            $( let $n = $n.clone(); )+
            move |$(clone!(@param $p),)+| $body
        }
    );
}
