use std::sync::Arc;
use std::time::{Duration, Instant};

use actix_web::client::ClientConnector;
use actix_web::{client, HttpMessage};

use actix::{Actor, Addr};

use failure::Fail;
use futures01::future::Either;
use futures01::{Future, IntoFuture};
use parking_lot::Mutex;
use tokio_retry::strategy::{jitter, ExponentialBackoff};
use tokio_retry::Retry;
use url::Url;

use crate::actors::objects::{ObjectError, ObjectErrorKind, USER_AGENT};
use crate::sources::{FileType, SentryFileId, SentrySourceConfig, SourceFileId};
use crate::types::ObjectId;

lazy_static::lazy_static! {
    static ref SENTRY_SEARCH_RESULTS: Mutex<lru::LruCache<SearchQuery, (Instant, Vec<SearchResult>)>> =
        Mutex::new(lru::LruCache::new(100_000));
    static ref CLIENT_CONNECTOR: Addr<ClientConnector> = ClientConnector::default().start();
}

#[derive(Debug, Fail, Clone, Copy)]
pub enum SentryErrorKind {
    #[fail(display = "failed parsing JSON response from Sentry")]
    Parsing,

    #[fail(display = "bad status code from Sentry")]
    BadStatusCode,

    #[fail(display = "failed sending request to Sentry")]
    SendRequest,
}

#[derive(Clone, Debug, serde::Deserialize)]
struct SearchResult {
    id: String,
    // TODO: Add more fields
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct SearchQuery {
    index_url: Url,
    token: String,
}

symbolic::common::derive_failure!(
    SentryError,
    SentryErrorKind,
    doc = "Errors happening while fetching data from Sentry"
);

fn perform_search(
    query: SearchQuery,
) -> impl Future<Item = Vec<SearchResult>, Error = ObjectError> {
    if let Some((created, entries)) = SENTRY_SEARCH_RESULTS.lock().get(&query) {
        if created.elapsed() < Duration::from_secs(3600) {
            return Either::A(Ok(entries.clone()).into_future());
        }
    }

    let index_url = query.index_url.clone();
    let token = query.token.clone();

    log::debug!("Fetching list of Sentry debug files from {}", index_url);
    let index_request = move || {
        client::get(&index_url)
            .with_connector((*CLIENT_CONNECTOR).clone())
            .header("Accept-Encoding", "identity")
            .header("User-Agent", USER_AGENT)
            .header("Authorization", format!("Bearer {}", token.clone()))
            .finish()
            .unwrap()
            .send()
            .map_err(|e| e.context(SentryErrorKind::SendRequest).into())
            .and_then(move |response| {
                if response.status().is_success() {
                    log::trace!("Success fetching index from Sentry");
                    Either::A(
                        response
                            .json::<Vec<SearchResult>>()
                            .map_err(|e| e.context(SentryErrorKind::Parsing).into()),
                    )
                } else {
                    log::warn!("Sentry returned status code {}", response.status());
                    Either::B(Err(SentryError::from(SentryErrorKind::BadStatusCode)).into_future())
                }
            })
    };

    let index_request = Retry::spawn(
        ExponentialBackoff::from_millis(10).map(jitter).take(3),
        index_request,
    );

    let index_request = index_request.map(move |entries| {
        SENTRY_SEARCH_RESULTS
            .lock()
            .put(query, (Instant::now(), entries.clone()));
        entries
    });

    Either::B(
        future_metrics!("downloads.sentry.index", None, index_request).map_err(|e| match e {
            tokio_retry::Error::OperationError(e) => e.context(ObjectErrorKind::Sentry).into(),
            e => panic!("{}", e),
        }),
    )
}

pub(super) fn prepare_downloads(
    source: &Arc<SentrySourceConfig>,
    _filetypes: &'static [FileType],
    object_id: &ObjectId,
) -> Box<dyn Future<Item = Vec<SourceFileId>, Error = ObjectError>> {
    let index_url = {
        let mut url = source.url.clone();
        if let Some(ref debug_id) = object_id.debug_id {
            url.query_pairs_mut()
                .append_pair("debug_id", &debug_id.to_string());
        }

        if let Some(ref code_id) = object_id.code_id {
            url.query_pairs_mut()
                .append_pair("code_id", &code_id.to_string());
        }

        url
    };

    let query = SearchQuery {
        index_url,
        token: source.token.clone(),
    };

    let entries = perform_search(query).map(clone!(source, |entries| {
        entries
            .into_iter()
            .map(move |api_response| {
                SourceFileId::Sentry(source.clone(), SentryFileId::new(api_response.id))
            })
            .collect()
    }));

    Box::new(entries)
}
