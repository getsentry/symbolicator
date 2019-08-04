use actix_web::{web, Error};

fn get_healthcheck() -> Result<&'static str, Error> {
    Ok("ok")
}

pub fn configure(config: &mut web::ServiceConfig) {
    config.route("/healthcheck", web::get().to(get_healthcheck));
}
