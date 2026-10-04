//! Network-boundary stand-in support; preserve the production request except its authority.
use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use axum::Router;
use bytes::Bytes;
use tokio::task::JoinHandle;
use url::Url;

use super::{ChannelLabel, HttpClient, ReqwestHttpClient};

pub(crate) struct StandIn {
    pub http: Arc<dyn HttpClient>,
    task: JoinHandle<()>,
}

impl StandIn {
    pub async fn start(origin: &'static str, router: Router) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("stand-in listener");
        let base = Url::parse(&format!("http://{}", listener.local_addr().expect("address"))).expect("URL");
        let task = tokio::spawn(async move { axum::serve(listener, router).await.expect("stand-in server") });
        Self { http: Arc::new(Redirect { origin, base }), task }
    }
}

impl Drop for StandIn {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Redirect {
    origin: &'static str,
    base: Url,
}

#[async_trait]
impl HttpClient for Redirect {
    async fn execute(&self, mut request: reqwest::Request, label: &ChannelLabel) -> Result<http::Response<Bytes>, String> {
        assert_eq!(request.url().origin().ascii_serialization(), self.origin);
        let mut url = self.base.clone();
        url.set_path(request.url().path());
        url.set_query(request.url().query());
        *request.url_mut() = url;
        tokio::time::timeout(Duration::from_secs(5), ReqwestHttpClient::new().execute(request, label))
            .await
            .map_err(|error| error.to_string())?
    }
}
