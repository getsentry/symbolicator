# Shared Cache

When running multiple symbolicators each instance has its own local
cache.  Starting up a new symbolicator instance in such an environment
means it will have a cold cache and take a long time before it can
process events at the same rate.  To remedy this symbolicators can be
configured to share their caches.

When configured like this all caches will be shared, both original
files and derived caches, thus a new symbolicator will not need to do
all the computation to create missing common derived caches.  However
cache-computation is not coordinated, so if new caches are needed the
shared cache works on a first-write-wins principle as all caches are
identical for a given cache key.

## Configuration

The shared cache is enabled by adding a new map in the config file:

```yaml
shared_cache:
  # The number of allowed concurrent uploads to the shared cache.
  #
  # Uploading to the shared cache is not critical for symbolicator's operation and
  # should not disrupt any normal work it does.  This limits the number of concurrent
  # uploads so that associated resources are kept in check.
  max_concurrent_uploads: 20
  # The number of queued up uploads to the cache.
  #
  # If more items need to be uploaded to the shared cache than there are allowed
  # concurrently the uploads will be queued.  If the queue is full the uploads are
  # simply dropped as they are not critical to symbolicator's operation and not
  # disrupting symbolicator is more important than uploading to the shared cache.
  max_upload_queue_size: 100

  # In production only Google Cloud Service is supported.
  gcs:
    # Required
    bucket: "bucket-name"
    # Optional name to service account .json file
    #
    # If not used the GCP internal metadata service will be used to retrieve tokens.
    service_account_path: "/path/to/service-account.json"

  # For testing an alternative backend is supported, this **can not** be used
  # at the same time as the `gcs` option.
  filesystem:
    path: "/some/path/to/a/dir/"
```
