use actix;
use actix::Actor;
use actix::Addr;

use actix_web::server;
use actix_web::App;

use crate::endpoints;

use crate::actors::cache::CacheActor;
use crate::actors::objects::ObjectsActor;
use crate::actors::symbolication::SymbolicationActor;

#[derive(Clone)]
pub struct ServiceState {
    pub symbolication: Addr<SymbolicationActor>,
}

pub type ServiceApp = App<ServiceState>;

fn get_app(state: ServiceState) -> ServiceApp {
    let mut app = App::with_state(state);
    app = endpoints::symbolicate::register(app);
    app
}

pub fn main() {
    env_logger::init();
    let sys = actix::System::new("symbolicator");

    let download_cache = CacheActor::new("/tmp/symbolicator-objects").start();
    let objects = ObjectsActor::new(download_cache).start();

    let sym_cache = CacheActor::new("/tmp/symbolicator-symcaches").start();
    let symbolication = SymbolicationActor::new(sym_cache, objects).start();

    let state = ServiceState { symbolication };

    server::new(move || get_app(state.clone()))
        .bind("127.0.0.1:8080")
        .unwrap()
        .start();

    println!("Started http server: 127.0.0.1:8080");
    let _ = sys.run();
}
