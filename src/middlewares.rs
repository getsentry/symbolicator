use std::time::Instant;

use actix_web::{
    middleware::{Finished, Middleware, Started},
    Error, HttpRequest, HttpResponse,
};

/// Basic metrics
pub struct Metrics;

struct StartTime(Instant);

impl<S> Middleware<S> for Metrics {
    fn start(&self, req: &HttpRequest<S>) -> Result<Started, Error> {
        req.extensions_mut().insert(StartTime(Instant::now()));
        Ok(Started::Done)
    }

    fn finish(&self, req: &HttpRequest<S>, resp: &HttpResponse) -> Finished {
        let start_time = req.extensions().get::<StartTime>().unwrap().0;
        metric!(timer("requests.duration") = start_time.elapsed());
        metric!(counter(&format!("responses.status_code.{}", resp.status())) += 1);
        Finished::Done
    }
}
