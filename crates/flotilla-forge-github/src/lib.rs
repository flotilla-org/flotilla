pub mod discovery;
pub mod forge_budget;
pub mod github;
pub mod github_observation;
pub mod github_poll;

/// Call `GhApi::get` with an automatically derived channel label.
#[macro_export]
macro_rules! gh_api_get {
    ($api:expr, $endpoint:expr, $repo_root:expr, $labeler:expr $(,)?) => {{
        let __endpoint = $endpoint;
        let __label = flotilla_provider_runtime::gh_api_channel_label_with::<{ flotilla_provider_runtime::REPLAY_LABELS_ENABLED }, _>(
            "GET", __endpoint, &$labeler,
        );
        $api.get(__endpoint, $repo_root, &__label).await
    }};
    ($api:expr, $endpoint:expr, $repo_root:expr $(,)?) => {{
        let __endpoint = $endpoint;
        let __label = flotilla_provider_runtime::gh_api_channel_label("GET", __endpoint);
        $api.get(__endpoint, $repo_root, &__label).await
    }};
}

/// Macro that calls `GhApi::get_with_headers`, auto-deriving the channel label from the endpoint.
#[macro_export]
macro_rules! gh_api_get_with_headers {
    ($api:expr, $endpoint:expr, $repo_root:expr, $labeler:expr $(,)?) => {{
        let __endpoint = $endpoint;
        let __label = flotilla_provider_runtime::gh_api_channel_label_with::<{ flotilla_provider_runtime::REPLAY_LABELS_ENABLED }, _>(
            "GET", __endpoint, &$labeler,
        );
        $api.get_with_headers(__endpoint, $repo_root, &__label).await
    }};
    ($api:expr, $endpoint:expr, $repo_root:expr $(,)?) => {{
        let __endpoint = $endpoint;
        let __label = flotilla_provider_runtime::gh_api_channel_label("GET", __endpoint);
        $api.get_with_headers(__endpoint, $repo_root, &__label).await
    }};
}
