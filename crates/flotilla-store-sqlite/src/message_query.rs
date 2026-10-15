use crate::MessageQuery;

pub(super) trait MessageQuerySql {
    fn sql_index(&self, replicas: bool) -> &'static str;
    fn sql(&self) -> (String, Option<String>);
    fn sql_at(&self, argument_index: usize) -> (String, Option<String>);
}
impl MessageQuerySql for MessageQuery {
    fn sql_index(&self, replicas: bool) -> &'static str {
        match (replicas, self) {
            (false, Self::Active { receiver: None }) => "resource_objects_message_inbox",
            (true, Self::Active { receiver: None }) => "replica_objects_message_inbox",
            (false, Self::Active { receiver: Some(_) }) => "resource_objects_message_active",
            (true, Self::Active { receiver: Some(_) }) => "replica_objects_message_active",
            (false, Self::ReplyTo { .. }) => "resource_objects_message_reply",
            (true, Self::ReplyTo { .. }) => "replica_objects_message_reply",
            (false, Self::Batch { .. }) => "resource_objects_message_batch",
            (true, Self::Batch { .. }) => "replica_objects_message_batch",
            (false, Self::AuditBefore { .. }) => "message_audit_age",
            (true, Self::AuditBefore { .. }) => "replica_message_audit_age",
        }
    }

    fn sql(&self) -> (String, Option<String>) {
        self.sql_at(5)
    }

    /// Build a predicate using the caller's next bind position.
    fn sql_at(&self, argument_index: usize) -> (String, Option<String>) {
        let terminal = terminal_phases_sql();
        let active = format!("COALESCE(json_extract(body_json, '$.status.phase'), 'accepted') NOT IN ({terminal})");
        match self {
            Self::Active { receiver: None } => (active, None),
            Self::Active { receiver: Some(receiver) } => (format!("{active} AND json_extract(body_json, '$.spec.receiver') = ?{argument_index}"), Some(receiver.clone())),
            Self::ReplyTo { name } => (format!("json_extract(body_json, '$.spec.in_reply_to') = ?{argument_index}"), Some(name.clone())),
            Self::AuditBefore { before } => (format!("COALESCE(json_extract(body_json, '$.status.phase'), 'accepted') IN ({terminal}) AND json_extract(body_json, '$.spec.body_digest') IS NULL AND julianday(json_extract(body_json, '$.status.since')) < julianday(?{argument_index})"), Some(before.to_rfc3339())),
            Self::Batch { id } => (format!("json_extract(body_json, '$.status.submission.batch_id') = ?{argument_index}"), Some(id.clone())),
        }
    }
}

fn terminal_phases_sql() -> &'static str {
    static PHASES: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
        flotilla_resources::MessagePhase::ALL
            .into_iter()
            .filter(|phase| phase.is_terminal())
            .map(|phase| format!("'{}'", serde_json::to_value(phase).expect("phase serializes").as_str().expect("phase is a string")))
            .collect::<Vec<_>>()
            .join(",")
    });
    &PHASES
}

#[cfg(test)]
mod tests {
    use super::*;
    // SQLite requires the query predicate to match its partial-index predicate.
    // Pin every persisted message index to the model's terminal classification.
    #[tokio::test]
    async fn partial_indexes_use_model_terminal_phases() {
        let backend = super::super::SqliteBackend::open_in_memory().expect("initialize message indexes");
        let indexes = backend
            .connection
            .call(|connection| {
                let mut statement = connection
                    .prepare("SELECT sql FROM sqlite_master WHERE type = 'index' AND sql LIKE '%WHERE kind=''Message'' AND COALESCE%'")?;
                let indexes = statement.query_map([], |row| row.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
                Ok::<_, rusqlite::Error>(indexes)
            })
            .await
            .expect("read message index definitions");
        let expected = format!("IN ({})", terminal_phases_sql());
        assert_eq!(indexes.len(), 6);
        for index in indexes {
            assert!(index.contains(&expected), "{index}");
        }
    }
}
