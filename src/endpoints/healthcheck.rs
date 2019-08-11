use actix_web::{web, Error};

fn get_healthcheck() -> Result<&'static str, Error> {
    Ok("ok")
}

pub fn configure(config: &mut web::ServiceConfig) {
    config.route("/healthcheck", web::get().to(get_healthcheck));
}

#[cfg(test)]
mod tests {
    use actix_web::dev::Service as _;
    use actix_web::http::StatusCode;
    use actix_web::web::Bytes;

    use crate::test;

    #[test]
    fn test_get() {
        test::setup();

        let mut server = test::test_service(Default::default());
        let request = test::TestRequest::with_uri("/healthcheck").to_request();
        let response = test::block_fn(|| server.call(request)).unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(test::read_body(response), Bytes::from("ok"));
    }
}
