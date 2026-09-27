use std::fmt;

use clap::{Parser, Subcommand};
use flotilla_protocol::{Command, CommandAction};

use crate::Resolved;

#[derive(Debug, Clone, PartialEq, Eq, Parser)]
#[command(about = "Inspect fulfilment kinds and live host facts")]
pub struct FulfilmentNoun {
    #[command(subcommand)]
    pub verb: FulfilmentVerb,
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum FulfilmentVerb {
    /// List kinds, grants, harnesses and models
    List,
}

impl FulfilmentNoun {
    pub fn resolve(self) -> Result<Resolved, String> {
        Ok(Resolved::Ready(Command::builder().action(CommandAction::QueryFulfilmentList {}).build()))
    }
}

impl fmt::Display for FulfilmentNoun {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "fulfilment list")
    }
}
