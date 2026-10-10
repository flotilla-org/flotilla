//! Human verdicts and the watch-maintained queue.
use crate::Resolved;
use clap::{Parser, Subcommand, ValueEnum};
use flotilla_protocol::{Command, CommandAction};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, Parser)]
pub struct PromiseNoun {
    #[command(subcommand)]
    pub verb: PromiseVerb,
}
#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum PromiseVerb {
    /// List submissions awaiting a human verdict
    Queue,
    /// Record an operator verdict on the current submission
    Verdict {
        convoy: String,
        promise: String,
        verdict: Verdict,
        #[arg(long)]
        reason: String,
        #[arg(long)]
        namespace: Option<String>,
        #[arg(long)]
        vessel: Option<String>,
        #[arg(long)]
        role: Option<String>,
        /// Refuse if the displayed attempt has since been replaced
        #[arg(long)]
        submitted_at: Option<flotilla_protocol::result_set::Timestamp>,
    },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Verdict {
    Approve,
    Reject,
}
impl PromiseNoun {
    pub fn resolve(self) -> Result<Resolved, String> {
        let action = match self.verb {
            PromiseVerb::Queue => CommandAction::QueryPromiseQueue {},
            PromiseVerb::Verdict { convoy, promise, verdict, reason, namespace, vessel, role, submitted_at } => {
                if reason.trim().is_empty() {
                    return Err("a verdict requires a nonempty reason".into());
                }
                let (namespace, convoy) = if let Some((ns, name)) = convoy.split_once('/') {
                    if namespace.as_deref().is_some_and(|value| value != ns) {
                        return Err("convoy namespace conflicts with --namespace".into());
                    }
                    (Some(ns.to_owned()), name.to_owned())
                } else {
                    (namespace, convoy)
                };
                CommandAction::PromiseVerdict {
                    namespace,
                    convoy,
                    promise,
                    vessel,
                    role,
                    accepted: verdict == Verdict::Approve,
                    reason,
                    submitted_at,
                }
            }
        };
        Ok(Resolved::Ready(Command::builder().action(action).build()))
    }
}
impl fmt::Display for PromiseNoun {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.verb {
            PromiseVerb::Queue => write!(f, "promise queue"),
            PromiseVerb::Verdict { convoy, promise, verdict, reason, namespace, vessel, role, submitted_at } => {
                write!(
                    f,
                    "promise verdict {} {} {} --reason {}",
                    crate::quote_value(convoy),
                    crate::quote_value(promise),
                    if *verdict == Verdict::Approve { "approve" } else { "reject" },
                    crate::quote_value(reason)
                )?;
                for (flag, value) in [("namespace", namespace), ("vessel", vessel), ("role", role)] {
                    if let Some(value) = value {
                        write!(f, " --{flag} {}", crate::quote_value(value))?;
                    }
                }
                if let Some(at) = submitted_at {
                    write!(f, " --submitted-at {}", at.to_rfc3339())?;
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Glue: clap and the noun resolver must preserve verdict selection, reason
    // and attempt precondition, including a namespace-qualified convoy operand.
    #[test]
    fn parses_human_verdict_and_queue_commands() {
        let noun = PromiseNoun::try_parse_from([
            "promise",
            "verdict",
            "dev/video",
            "demo",
            "reject",
            "--reason",
            "missing narration",
            "--vessel",
            "work",
            "--role",
            "coder",
            "--submitted-at",
            "2026-10-10T12:00:00Z",
        ])
        .expect("verdict CLI");
        let rendered = noun.to_string();
        assert!(rendered.contains("--reason \"missing narration\""));
        let Resolved::Ready(command) = noun.resolve().expect("verdict") else { panic!("ready command") };
        assert!(
            matches!(command.action, CommandAction::PromiseVerdict { namespace: Some(ns), convoy, accepted: false, reason, submitted_at: Some(_), .. }
            if ns == "dev" && convoy == "video" && reason == "missing narration")
        );
        let queue = PromiseNoun::try_parse_from(["promise", "queue"]).expect("queue CLI").resolve().expect("queue");
        assert!(matches!(queue, Resolved::Ready(Command { action: CommandAction::QueryPromiseQueue {}, .. })));
        assert!(PromiseNoun::try_parse_from(["promise", "verdict", "video", "demo", "reject"]).is_err());
    }
}
