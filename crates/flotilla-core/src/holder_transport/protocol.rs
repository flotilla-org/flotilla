//! The subset of Codex's JSON-RPC protocol consumed by Flotilla.
//! Unknown harness fields and event kinds are tolerated; required correlation
//! and identity fields remain required so malformed replies cannot prove input.
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum RequestId {
    Number(i64),
    Text(String),
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum Incoming {
    Event {
        method: String,
        #[serde(default)]
        id: Option<RequestId>,
        params: Value,
    },
    Failure {
        id: RequestId,
        error: Error,
    },
    Success {
        id: RequestId,
        result: Value,
    },
}
#[derive(Debug, Deserialize)]
pub struct Error {
    pub code: i64,
    pub message: String,
    #[serde(default)]
    pub data: Option<Value>,
}
#[derive(Serialize)]
pub struct Outgoing<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<i64>,
    pub method: &'a str,
    pub params: &'a Value,
}

#[derive(Debug, Deserialize)]
pub struct ThreadResponse {
    pub thread: Thread,
}
#[derive(Debug, Deserialize)]
pub struct Thread {
    pub id: String,
    pub status: ThreadStatus,
    #[serde(default)]
    pub turns: Vec<Turn>,
}
#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ThreadStatus {
    Idle,
    Active {
        #[serde(default, rename = "activeFlags")]
        flags: Vec<String>,
    },
    #[serde(other)]
    Unavailable,
}
#[derive(Debug, Deserialize)]
pub struct Turn {
    pub id: String,
    pub status: String,
    #[serde(default)]
    pub items: Vec<Item>,
}
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Item {
    UserMessage {
        #[serde(default, rename = "clientId")]
        client_id: Option<String>,
        #[serde(default)]
        content: Vec<Content>,
    },
    #[serde(other)]
    Other,
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Content {
    Text {
        text: String,
        #[serde(default)]
        text_elements: Vec<Value>,
    },
    #[serde(other)]
    Other,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadRead<'a> {
    pub thread_id: &'a str,
    pub include_turns: bool,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadResume<'a> {
    pub thread_id: &'a str,
    pub exclude_turns: bool,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Input<'a> {
    pub thread_id: &'a str,
    pub client_user_message_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_turn_id: Option<&'a str>,
    pub input: Vec<Content>,
}
#[derive(Deserialize)]
pub struct Notification {
    #[serde(default)]
    pub id: Option<RequestId>,
    #[serde(flatten)]
    pub event: Event,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "method", content = "params")]
pub enum Event {
    #[serde(rename = "turn/started")]
    TurnStarted(TurnEvent),
    #[serde(rename = "turn/completed")]
    TurnCompleted(TurnEvent),
    #[serde(rename = "item/commandExecution/requestApproval")]
    CommandApproval(Approval),
    #[serde(rename = "item/fileChange/requestApproval")]
    FileApproval(Approval),
    #[serde(rename = "item/permissions/requestApproval")]
    PermissionsApproval(Approval),
    #[serde(rename = "serverRequest/resolved")]
    Resolved(Resolved),
    #[serde(other)]
    Other,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnEvent {
    pub thread_id: String,
    pub turn: Turn,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Approval {
    pub thread_id: String,
    pub item_id: String,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Resolved {
    pub thread_id: String,
    pub request_id: RequestId,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadId<'a> {
    pub thread_id: &'a str,
}

#[cfg(test)]
mod tests {
    use super::*;

    // The first-party schema permits signed int64 and string request IDs.
    // Generate the full integer range and both representations, including negatives.
    #[hegel::test]
    fn signed_and_string_request_ids_survive_server_event_decoding(tc: hegel::TestCase) {
        let number = tc.draw(hegel::generators::integers::<i64>());
        let id = if tc.draw(hegel::generators::booleans()) { RequestId::Number(number) } else { RequestId::Text(number.to_string()) };
        let event = serde_json::json!({"method":"future/event","id":id,"params":{}});
        let Incoming::Event { id: decoded, .. } = serde_json::from_value(event).expect("valid signed or string ID") else {
            panic!("server event became a reply")
        };
        assert_eq!(decoded, Some(id));
    }

    #[test]
    fn server_request_with_a_colliding_id_cannot_decode_as_a_reply() {
        let message: Incoming = serde_json::from_str(
            r#"{"id":7,"method":"item/commandExecution/requestApproval","params":{"threadId":"thread","itemId":"command"}}"#,
        )
        .expect("server request");
        assert!(matches!(message, Incoming::Event { id: Some(RequestId::Number(7)), .. }));
        assert!(serde_json::from_str::<Incoming>(r#"{"result":{}}"#).is_err());
        assert!(serde_json::from_str::<ThreadResponse>(r#"{"thread":{"status":{"type":"idle"}}}"#).is_err());
    }

    #[test]
    fn unknown_harness_fields_and_items_do_not_hide_a_correlated_receipt() {
        let response: ThreadResponse = serde_json::from_str(r#"{"thread":{"id":"thread","status":{"type":"active","activeFlags":["waitingOnApproval"]},"futureField":true,"turns":[{"id":"turn","status":"inProgress","items":[{"type":"futureTool","opaque":42},{"type":"userMessage","clientId":"batch","content":[{"type":"text","text":"input"}]}]}]}}"#).expect("forward compatible reply");
        assert!(matches!(response.thread.status, ThreadStatus::Active { .. }));
        assert!(super::super::CodexTransport::receipt(&response.thread, "batch").is_some());
        assert!(super::super::CodexTransport::receipt(&response.thread, "different-batch").is_none());
    }
}
