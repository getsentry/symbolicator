use actix_web::HttpRequest;

use crate::app::{ServiceApp, ServiceState};

fn healthcheck(_req: HttpRequest<ServiceState>) -> &'static str {
    metric!(counter("healthcheck") += 1);
    "ok"
}

pub fn configure(app: ServiceApp) -> ServiceApp {
    app.resource("/healthcheck", |r| {
        r.get().with(healthcheck);
    })
}
