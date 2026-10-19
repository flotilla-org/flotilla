use flotilla_crew::review_bundle::{ReviewBundleObjects, ReviewBundleStore, ReviewBundleStoreError};
use flotilla_resources::*;
use object_store::{path::Path, ObjectStore, PutPayload};
use std::sync::Arc;

pub struct ObjectStoreReviewBundle(pub Arc<dyn ObjectStore>);
#[async_trait::async_trait]
impl ReviewBundleObjects for ObjectStoreReviewBundle {
    async fn put(&self, key: &str, contents: Vec<u8>) -> Result<(), ReviewBundleStoreError> {
        self.0
            .put(&Path::from(key), PutPayload::from_bytes(contents.into()))
            .await
            .map_err(|e| ReviewBundleStoreError::Store(Box::new(e)))?;
        Ok(())
    }
    async fn get(&self, key: &str) -> Result<Vec<u8>, ReviewBundleStoreError> {
        let result = self.0.get(&Path::from(key)).await.map_err(|e| ReviewBundleStoreError::Store(Box::new(e)))?;
        Ok(result.bytes().await.map_err(|e| ReviewBundleStoreError::Store(Box::new(e)))?.to_vec())
    }
}
pub fn connect_review_bundle(
    config: &ReviewBundleStoreConfig,
    credential: &ReviewBundleWriteCredential,
) -> Result<ReviewBundleStore, ReviewBundleStoreError> {
    let mut builder = object_store::aws::AmazonS3Builder::new()
        .with_endpoint(&config.endpoint)
        .with_bucket_name(&config.bucket)
        .with_region(&config.region)
        .with_access_key_id(&credential.access_key_id)
        .with_secret_access_key(&credential.secret_access_key)
        .with_allow_http(config.allow_http)
        .with_virtual_hosted_style_request(config.virtual_hosted_style);
    if let Some(token) = &credential.session_token {
        builder = builder.with_token(token);
    }
    let store = builder.build().map_err(|error| ReviewBundleStoreError::Configuration(error.to_string()))?;
    Ok(ReviewBundleStore::new(Arc::new(ObjectStoreReviewBundle(Arc::new(store))), &config.public_base_url))
}
