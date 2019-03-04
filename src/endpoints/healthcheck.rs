use crate::app::{ServiceApp, ServiceState};
use actix_web::{http::Method, HttpRequest};

fn healthcheck(_req: HttpRequest<ServiceState>) -> &'static str {
    "ok"
}

pub fn register(app: ServiceApp) -> ServiceApp {
    app.resource("/healthcheck", |r| {
        r.method(Method::GET).with(healthcheck);
    })
}
