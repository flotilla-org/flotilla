//! Gate conditions judge an observation's age by the observers' windows composition installs
//! (#3108), as the old store's gates judged it by the observers' cadence.

use flotilla_engine_messages_testkit::EngineMessagesHome;
use flotilla_resources::{InputMeta, Issue, IssueSpec, IssueStatus, Observation, ObservedIssueState, ThreeValue};
use flotilla_store::ResourceBackend;
use flotilla_store_memory::InMemoryBackend;

const NAMESPACE: &str = "flotilla";

// Behaviour: an issue observed two minutes ago is fresh under the default five-minute window and
// unknown under an installed one-minute window.
#[tokio::test]
async fn an_observation_older_than_the_installed_window_is_unknown() {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let address = flotilla_resources::address::stored_issue_address("github.com", "team/repo", 1, &[]).expect("issue address");
    let name = flotilla_resources::address::record_name(&address).expect("record name");
    let issues = backend.using::<Issue>(NAMESPACE);
    let issue = issues
        .create(
            &InputMeta::builder().name(name.clone()).build(),
            &IssueSpec::builder()
                .service("github.com".into())
                .scope("team/repo".into())
                .number(1)
                .observing_authority("home".into())
                .build(),
        )
        .await
        .expect("issue");
    let observed = chrono::Utc::now() - chrono::Duration::minutes(2);
    let status = IssueStatus {
        title: Default::default(),
        assignees: Default::default(),
        state: Observation::known(ObservedIssueState::Closed, observed),
        labels: Observation::known(Vec::new(), observed),
        updated_at: Observation::known(observed, observed),
    };
    issues.update_status(&name, &issue.metadata.resource_version, &status).await.expect("observed");
    let condition = flotilla_address::Condition::resolve(&format!("{address} .state == \"closed\""), &flotilla_address::BuiltinForges)
        .expect("condition");

    let default = EngineMessagesHome::open("home-a", NAMESPACE, &backend);
    assert_eq!(default.primitives().conditions().value(NAMESPACE, &condition).await.expect("value"), ThreeValue::True);
    let installed = EngineMessagesHome::open("home-b", NAMESPACE, &backend);
    let minute = std::time::Duration::from_secs(60);
    installed.primitives().install_observation_staleness(minute, minute).expect("install the windows");
    assert_eq!(installed.primitives().conditions().value(NAMESPACE, &condition).await.expect("value"), ThreeValue::Unknown);
}
