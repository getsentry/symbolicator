use std::fmt;
use std::sync::Arc;

use aws_types::region::Region;
use serde::ser::SerializeTuple;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{CommonSourceConfig, RemoteFile, RemoteFileUri, SourceId, SourceLocation};

/// Configuration for S3 symbol buckets.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct S3SourceConfig {
    /// Unique source identifier.
    pub id: SourceId,

    /// Name of the bucket in the S3 account.
    pub bucket: String,

    /// A path from the root of the bucket where files are located.
    #[serde(default)]
    pub prefix: String,

    /// Authorization information for this bucket. Needs read access.
    #[serde(flatten)]
    pub source_key: Arc<S3SourceKey>,

    /// Configuration common to all sources.
    #[serde(flatten)]
    pub files: CommonSourceConfig,
}

/// The S3-specific [`RemoteFile`].
#[derive(Debug, Clone)]
pub struct S3RemoteFile {
    /// The underlying [`S3SourceConfig`].
    pub source: Arc<S3SourceConfig>,
    pub(crate) location: SourceLocation,
}

impl From<S3RemoteFile> for RemoteFile {
    fn from(source: S3RemoteFile) -> Self {
        Self::S3(source)
    }
}

impl S3RemoteFile {
    /// Creates a new [`S3RemoteFile`].
    pub fn new(source: Arc<S3SourceConfig>, location: SourceLocation) -> Self {
        Self { source, location }
    }

    /// Returns the S3 key.
    ///
    /// This is equivalent to the pathname within the bucket.
    pub fn key(&self) -> String {
        self.location.prefix(&self.source.prefix)
    }

    /// Returns the S3 bucket name.
    pub fn bucket(&self) -> String {
        self.source.bucket.clone()
    }

    /// Returns the `s3://` URI from which to download this object file.
    pub fn uri(&self) -> RemoteFileUri {
        RemoteFileUri::from_parts("s3", &self.source.bucket, &self.key())
    }

    pub(crate) fn host(&self) -> String {
        self.bucket()
    }
}

fn serialize_region<S>(region: &S3Region, s: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    match region.endpoint.as_ref() {
        Some(endpoint) => {
            let mut seq = s.serialize_tuple(2)?;

            seq.serialize_element(region.region.as_ref())?;
            seq.serialize_element(endpoint.as_str())?;

            seq.end()
        }
        None => s.serialize_str(region.region.as_ref()),
    }
}

/// Local helper to deserialize an S3 region string in `S3SourceKey`.
fn deserialize_region<'de, D>(deserializer: D) -> Result<S3Region, D::Error>
where
    D: Deserializer<'de>,
{
    // This is a Visitor that treats string types as a builtin region and
    // tuples as custom regions.
    struct SdkRegion;

    impl<'de> serde::de::Visitor<'de> for SdkRegion {
        type Value = S3Region;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("string or tuple")
        }

        fn visit_str<E>(self, value: &str) -> Result<S3Region, E>
        where
            E: serde::de::Error,
        {
            Ok(S3Region::from(value))
        }

        fn visit_seq<S>(self, seq: S) -> Result<S3Region, S::Error>
        where
            S: serde::de::SeqAccess<'de>,
        {
            let tup: (String, String) =
                Deserialize::deserialize(serde::de::value::SeqAccessDeserializer::new(seq))?;

            Ok(S3Region {
                region: Region::new(tup.0),
                endpoint: Some(tup.1),
            })
        }
    }

    deserializer.deserialize_any(SdkRegion)
}

/// The types of Amazon IAM credentials providers we support.
///
/// For details on the AWS side, see:
/// <https://docs.aws.amazon.com/AmazonECS/latest/developerguide/task-iam-roles.html>.
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum AwsCredentialsProvider {
    /// Static Credentials
    #[default]
    Static,
    /// Credentials derived from the container.
    Container,
}

/// Amazon S3 authorization information.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct S3SourceKey {
    /// The region of the S3 bucket.
    #[serde(
        deserialize_with = "deserialize_region",
        serialize_with = "serialize_region"
    )]
    pub region: S3Region,

    /// AWS IAM credentials provider for obtaining S3 access.
    #[serde(default)]
    pub aws_credentials_provider: AwsCredentialsProvider,

    /// S3 authorization key.
    #[serde(default)]
    pub access_key: String,

    /// S3 secret key.
    #[serde(default)]
    pub secret_key: String,
}

/// A wrapper around an S3 region that allows using custom regions.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct S3Region {
    /// The underlying [`Region`].
    pub region: Region,
    /// An optional endpoint for custom regions.
    pub endpoint: Option<String>,
}

impl From<&str> for S3Region {
    fn from(value: &str) -> Self {
        let region = Region::new(String::from(value));
        Self {
            region,
            endpoint: None,
        }
    }
}

impl PartialEq for S3SourceKey {
    fn eq(&self, other: &S3SourceKey) -> bool {
        self.access_key == other.access_key
            && self.secret_key == other.secret_key
            && self.region == other.region
    }
}

impl Eq for S3SourceKey {}

impl std::hash::Hash for S3SourceKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.access_key.hash(state);
        self.secret_key.hash(state);
        self.region.hash(state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::SourceConfig;

    #[test]
    fn test_s3_config_builtin_region() {
        let text = r#"
          - id: us-east
            type: s3
            bucket: my-supermarket-bucket
            region: us-east-1
            access_key: the-access-key
            secret_key: the-secret-key
            layout:
              type: unified
                  "#;
        let sources: Vec<SourceConfig> = serde_yaml::from_str(text).unwrap();
        assert_eq!(*sources[0].id(), SourceId("us-east".to_string()));
        match &sources[0] {
            SourceConfig::S3(cfg) => {
                assert_eq!(cfg.id, SourceId("us-east".to_string()));
                assert_eq!(cfg.bucket, "my-supermarket-bucket");
                assert_eq!(cfg.source_key.region, S3Region::from("us-east-1"));
                assert_eq!(cfg.source_key.access_key, "the-access-key");
                assert_eq!(cfg.source_key.secret_key, "the-secret-key");
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn test_s3_config_custom_region() {
        let text = r#"
          - id: minio
            type: s3
            bucket: my-homemade-bucket
            region:
              - minio
              - http://minio.minio.svc.cluster.local:9000
            access_key: the-access-key
            secret_key: the-secret-key
            layout:
              type: unified
                  "#;
        let sources: Vec<SourceConfig> = serde_yaml::from_str(text).unwrap();
        match &sources[0] {
            SourceConfig::S3(cfg) => {
                assert_eq!(cfg.id, SourceId("minio".to_string()));
                assert_eq!(
                    cfg.source_key.region,
                    S3Region {
                        region: Region::new("minio".to_string()),
                        endpoint: Some("http://minio.minio.svc.cluster.local:9000".to_string()),
                    }
                );
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn test_s3_config_plain_empty_region() {
        let text = r#"
          - id: honk
            type: s3
            bucket: me-bucket
            region:
            access_key: the-access-key
            secret_key: the-secret-key
            layout:
              type: unified
                  "#;
        let result: Result<Vec<SourceConfig>, serde_yaml::Error> = serde_yaml::from_str(text);
        assert!(result.is_err())
    }

    #[test]
    fn test_s3_config_custom_empty_region() {
        let text = r#"
          - id: honk
            type: s3
            bucket: me-bucket
            region:
                -
            access_key: the-access-key
            secret_key: the-secret-key
            layout:
              type: unified
                  "#;
        let result: Result<Vec<SourceConfig>, serde_yaml::Error> = serde_yaml::from_str(text);
        assert!(result.is_err())
    }

    #[test]
    fn test_s3_config_custom_region_not_enough_fields() {
        let text = r#"
          - id: honk
            type: s3
            bucket: me-bucket
            region:
              - honk
            access_key: the-access-key
            secret_key: the-secret-key
            layout:
              type: unified
                  "#;
        let result: Result<Vec<SourceConfig>, serde_yaml::Error> = serde_yaml::from_str(text);
        assert!(result.is_err())
    }

    #[test]
    fn test_s3_config_custom_region_too_many_fields() {
        let text = r#"
          - id: honk
            type: s3
            bucket: me-bucket
            region:
              - honk
              - http://honk.honk.beep.local:9000
              - beep
            access_key: the-access-key
            secret_key: the-secret-key
            layout:
              type: unified
                  "#;
        let result: Result<Vec<SourceConfig>, serde_yaml::Error> = serde_yaml::from_str(text);
        assert!(result.is_err())
    }
}
