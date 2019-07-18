use actix_web::web;

#[derive(Debug, derive_more::Display)]
struct CustomError;

impl actix_web::ResponseError for CustomError {}

fn get_healthcheck() -> Result<&'static str, CustomError> {
    Ok("ok")
}

pub fn configure(config: &mut web::ServiceConfig) {
    config.route("/healthcheck", web::get().to(get_healthcheck));
}
