use actix_web::{App, HttpRequest};

use crate::services::Service;

fn healthcheck(_req: HttpRequest<Service>) -> &'static str {
    metric!(counter("healthcheck") += 1);
    "ok"
}

pub fn configure(app: App<Service>) -> App<Service> {
    app.resource("/healthcheck", |r| {
        r.get().with(healthcheck);
    })
}
