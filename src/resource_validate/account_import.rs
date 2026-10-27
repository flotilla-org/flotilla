//! The account kinds' import rehearsal (#3824 layer 1): before a roll, every old AccountBinding
//! and AccountProvision the daemon holds is mapped onto the engine, restored into a temporary
//! store (bindings first, so each provision's reference finds its binding), read back, and
//! reported as counts by kind and state with each refusal named.

use flotilla_provisioning::engine::{account::rehearse_accounts, import::ImportReport};
use flotilla_resources::{AccountBinding, AccountProvision};

/// The kinds the rehearsal reads, by their plural.
pub(super) const KINDS: [&str; 2] = ["accountbindings", "accountprovisions"];

pub(super) use super::import_rows::Documents;

impl Documents {
    pub(super) fn rehearse(&self) -> ImportReport {
        let mut report = ImportReport::default();
        let bindings = self.decoded::<AccountBinding>("accountbindings", "AccountBinding", &mut report);
        let provisions = self.decoded::<AccountProvision>("accountprovisions", "AccountProvision", &mut report);
        rehearse_accounts(&bindings, &provisions, &mut report);
        report
    }
}

/// One line of counts, then one line per refusal.
pub(super) fn print(namespace: &str, report: &ImportReport) {
    let total: usize = report.imported.values().sum();
    if total == 0 && report.refused.is_empty() {
        return;
    }
    let counts = report.imported.iter().map(|((kind, label), count)| format!("{kind} {label} {count}")).collect::<Vec<_>>().join(", ");
    println!("account import rehearsal {namespace}: {total} imported ({counts}), {} refused", report.refused.len());
    for refusal in &report.refused {
        println!("account import rehearsal {namespace}: {refusal}");
    }
}
