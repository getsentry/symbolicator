macro_rules! compat_handler {
    ($func:ident , $($param:ident),*) => {{
        use ::futures::{FutureExt, TryFutureExt};
        use ::sentry::SentryFutureExt;

        |__hub: crate::utils::sentry::ActixHub, $($param),*| {
            $func ( $($param),* )
                .bind_hub(__hub)
                .boxed_local()
                .compat()
        }
    }};
}
