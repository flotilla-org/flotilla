use flotilla_resources::{CrewMessageDelivery, CrewMessageSender, TerminalCrewMessage};
use hegel::generators as gs;

fn message(id: usize) -> TerminalCrewMessage {
    TerminalCrewMessage {
        id: format!("turn-{id}"),
        text: "operator brief payload".repeat(128),
        sender: CrewMessageSender::OperatorResume { principal: None },
        delivery: CrewMessageDelivery::Queued,
        following: Vec::new(),
        acknowledged: Default::default(),
    }
}

// Acknowledgment pruning bounds payload history to pending turns, preserves
// FIFO delivery and exact receipts, and never replays an acknowledged turn.
#[hegel::test]
fn long_running_acknowledgments_preserve_order_and_duplicate_retries(tc: hegel::TestCase) {
    // Cross empty, single-message and long-lived histories; draw duplicate IDs
    // across the entire acknowledged prefix, including its endpoints.
    let count = tc.draw(gs::integers::<usize>().min_value(0).max_value(512));
    let mut head = message(0);
    assert!(!head.prune_acknowledged(None));
    assert!(!head.prune_acknowledged(Some("unknown")));
    let mut delivered = None;
    for id in 0..=count {
        if id > 0 {
            head.append(message(id));
        }
        head.append(message(id + 1));
        assert_eq!(head.next_after(delivered.as_deref()).expect("next turn").id, format!("turn-{id}"));
        delivered = Some(format!("turn-{id}"));
        assert!(head.prune_acknowledged(delivered.as_deref()));
        assert!(!head.prune_acknowledged(delivered.as_deref()), "pruning is idempotent");
        assert!(head.text.is_empty(), "acknowledged body is released");
        assert_eq!(head.following.len(), 1, "only the pending brief has a body");
        assert_eq!(head.following[0], message(id + 1), "pending operator brief stays intact");
        let replay = tc.draw(gs::integers::<usize>().min_value(0).max_value(id));
        head.append(message(replay));
        assert_eq!(head.following.len(), 1, "acknowledged retry cannot enqueue again");
        assert!(head.delivered_through(delivered.as_deref(), &format!("turn-{replay}")));
        assert!(!head.delivered_through(delivered.as_deref(), &format!("turn-{}", id + 1)));
        assert_eq!(head.pending_after(delivered.as_deref()).len(), 1);
        assert_eq!(head.pending_after(Some("unknown-old-launch")).len(), 1);
        let mut launch = head.clone();
        assert_eq!(launch.mark_next_for_launch(Some("unknown-old-launch")), Some(message(id + 1).text));
        assert_eq!(launch.following[0].delivery, CrewMessageDelivery::LaunchBrief);
        assert_eq!(head.next_after(None).expect("restart pending turn").id, format!("turn-{}", id + 1));
        let stored = serde_json::to_vec(&head).expect("encode");
        head = serde_json::from_slice(&stored).expect("decode receipts after restart");
        assert_eq!(head.id, "turn-0", "pruning preserves the nudge-tracking identity");
    }
}

// One-generation compatibility: the previous shape has no receipts; its
// acknowledged prefix can be compacted without altering pending delivery.
#[test]
fn previous_generation_message_decodes_and_prunes() {
    let old = serde_json::json!({
        "id": "old", "text": "delivered brief", "following": [
            {"id": "pending", "text": "operator follow-up", "following": []}
        ]
    });
    let mut head: TerminalCrewMessage = serde_json::from_value(old).expect("old message");
    assert!(head.prune_acknowledged(Some("old")));
    assert_eq!(head.next_after(Some("old")).expect("pending").text, "operator follow-up");
    assert!(head.delivered_through(Some("old"), "old"));
}
