use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use symbolicator_sources::{AzureRemoteFile, AzureSourceKey};
use url::Url;

use crate::caching::{CacheContents, CacheError};
use crate::download::DownloadLimits;
use crate::download::compression::Compression;

use super::Destination;

type AzureTokenCache = moka::future::Cache<Arc<AzureSourceKey>, CacheContents<AzureToken>>;

const TOKEN_EXPIRY_MARGIN: Duration = Duration::minutes(1);

#[derive(Debug, Clone)]
struct AzureToken {
    access_token: Arc<str>,
    expires_at: DateTime<Utc>,
}

#[derive(Deserialize)]
struct AzureTokenResponse {
    access_token: String,
    expires_in: i64,
}

#[derive(Debug)]
pub struct AzureDownloader {
    token_cache: AzureTokenCache,
    client: reqwest::Client,
    limits: DownloadLimits,
}

impl AzureDownloader {
    pub fn new(client: reqwest::Client, limits: DownloadLimits, token_capacity: u64) -> Self {
        Self {
            token_cache: AzureTokenCache::builder()
                .max_capacity(token_capacity)
                .build(),
            client,
            limits,
        }
    }

    async fn request_new_token(&self, source_key: &AzureSourceKey) -> CacheContents<AzureToken> {
        let url = format!(
            "https://login.microsoftonline.com/{}/oauth2/v2.0/token",
            source_key.tenant_id
        );
        let response = self
            .client
            .post(url)
            .form(&[
                ("grant_type", "client_credentials"),
                ("client_id", &source_key.client_id),
                ("client_secret", &source_key.client_secret.0),
                ("scope", "https://storage.azure.com/.default"),
            ])
            .send()
            .await?
            .error_for_status()?
            .json::<AzureTokenResponse>()
            .await?;

        Ok(AzureToken {
            access_token: response.access_token.into(),
            expires_at: Utc::now() + Duration::seconds(response.expires_in) - TOKEN_EXPIRY_MARGIN,
        })
    }

    async fn get_token(&self, source_key: &Arc<AzureSourceKey>) -> CacheContents<AzureToken> {
        metric!(counter("source.azure.token.access") += 1);

        let init = Box::pin(async {
            metric!(counter("source.azure.token.computation") += 1);
            self.request_new_token(source_key).await
        });
        let replace_if = |entry: &CacheContents<AzureToken>| {
            entry.as_ref().map_or(true, |t| t.expires_at < Utc::now())
        };

        self.token_cache
            .entry_by_ref(source_key)
            .or_insert_with_if(init, replace_if)
            .await
            .into_value()
    }

    pub async fn download_source(
        &self,
        source_name: &str,
        file_source: &AzureRemoteFile,
        destination: impl Destination,
    ) -> CacheContents<Compression> {
        let source = &file_source.source;
        let key = file_source.key();
        tracing::debug!("Fetching from Azure: {} (from {})", key, source.container);
        let token = self.get_token(&source.source_key).await?;

        let url = blob_url(&source.account, &source.container, &key)
            .ok_or_else(|| CacheError::DownloadError("invalid Azure blob URL".into()))?;

        let builder = self
            .client
            .get(url)
            .header("authorization", format!("Bearer {}", token.access_token))
            .header("x-ms-version", "2021-08-06");

        super::download_reqwest(
            source_name,
            builder,
            &self.limits,
            destination,
            &super::GenericErrorHandler,
        )
        .await
    }
}

fn is_valid_account(account: &str) -> bool {
    (3..=24).contains(&account.len())
        && account
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

fn blob_url(account: &str, container: &str, key: &str) -> Option<Url> {
    if !is_valid_account(account) {
        return None;
    }

    let mut url: Url = format!("https://{account}.blob.core.windows.net")
        .parse()
        .ok()?;
    url.path_segments_mut()
        .ok()?
        .push(container)
        .extend(key.split('/'));
    Some(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_blob_url() {
        let url = blob_url("account", "container", "a/key/with spaces").unwrap();
        assert_eq!(
            url.as_str(),
            "https://account.blob.core.windows.net/container/a/key/with%20spaces"
        );
        assert!(blob_url("evil.com/#", "container", "key").is_none());
        assert!(blob_url("evil.com", "container", "key").is_none());
        assert!(blob_url("ACCOUNT", "container", "key").is_none());
        assert!(blob_url("ab", "container", "key").is_none());
    }
}
