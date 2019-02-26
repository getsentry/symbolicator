#[macro_export]
macro_rules! tryf {
    ($e:expr) => {
        match $e {
            Ok(value) => value,
            Err(e) => return Box::new(::futures::future::err(::std::convert::From::from(e))),
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
