use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use tender::{ByteStream, Dial, Fingerprint, Router};
use tokio::net::UnixStream;

/// Stands in for one instance's links to the others: every listener path,
/// and this instance's choice of next hop toward each host.
pub struct Links {
    pub paths: Arc<BTreeMap<Fingerprint, PathBuf>>,
    pub next: BTreeMap<Fingerprint, Fingerprint>,
}

#[async_trait::async_trait]
impl Dial for Links {
    async fn dial(&self, instance: &Fingerprint) -> std::io::Result<ByteStream> {
        let path = self.paths.get(instance).ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))?;
        Ok(Box::new(UnixStream::connect(path).await?))
    }
}

impl Router for Links {
    fn next_hop(&self, host: &Fingerprint) -> Option<Fingerprint> {
        self.next.get(host).cloned()
    }
}
