use actix_web::web;

pub fn configure(config: &mut web::ServiceConfig) {
    config.route("/healthcheck", web::get().to(|| "ok"));
}
