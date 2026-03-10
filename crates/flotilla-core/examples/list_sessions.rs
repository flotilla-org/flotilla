//! Quick integration smoke-test: list coding-agent sessions from all configured
//! providers for a given repo slug.
//!
//! Usage:
//!   cargo run -p flotilla-core --example list_sessions -- owner/repo

use flotilla_core::providers::coding_agent::claude::ClaudeCodingAgent;
use flotilla_core::providers::coding_agent::cursor::CursorCodingAgent;
use flotilla_core::providers::coding_agent::CodingAgent;
use flotilla_core::providers::types::RepoCriteria;
use std::sync::Arc;

#[tokio::main]
async fn main() {
    let slug = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "rjwittams/flotilla".to_string());

    let criteria = RepoCriteria {
        repo_slug: Some(slug.clone()),
    };

    println!("Listing sessions for {slug}\n");

    let providers: Vec<(&str, Arc<dyn CodingAgent>)> = vec![
        (
            "claude",
            Arc::new(ClaudeCodingAgent::new(
                "claude".to_string(),
                Arc::new(flotilla_core::providers::ProcessCommandRunner),
                Arc::new(flotilla_core::providers::ReqwestHttpClient::new()),
            )),
        ),
        (
            "cursor",
            Arc::new(CursorCodingAgent::new("cursor".to_string())),
        ),
    ];

    for (name, provider) in &providers {
        print!("{name} ({}): ", provider.display_name());
        match provider.list_sessions(&criteria).await {
            Ok(sessions) => {
                println!("{} sessions", sessions.len());
                for (id, session) in &sessions {
                    println!("  {id}  {:?}  {}", session.status, session.title);
                }
            }
            Err(e) => println!("ERROR: {e}"),
        }
        println!();
    }
}
