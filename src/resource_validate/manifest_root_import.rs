//! ManifestRoot's import rehearsal (#3824 layer 2c): before a roll, every old ManifestRoot the
//! daemon holds is mapped onto the engine's records, restored into a temporary store and read
//! back, reported as a count with each refusal named.

use flotilla_provisioning::engine::{import::ImportReport, manifest_root_import::rehearse_roots};
use flotilla_resources::ManifestRoot;

pub(super) use super::import_rows::Documents;

/// The kind the rehearsal reads, by its plural.
pub(super) const KIND: &str = "manifestroots";

pub(super) fn rehearse(documents: &Documents) -> ImportReport {
    let mut report = ImportReport::default();
    let roots = documents.decoded::<ManifestRoot>(KIND, "ManifestRoot", &mut report);
    rehearse_roots(&roots, &mut report);
    report
}

/// One line of counts, then one line per refusal.
pub(super) fn print(namespace: &str, report: &ImportReport) {
    let total: usize = report.imported.values().sum();
    if total == 0 && report.refused.is_empty() {
        return;
    }
    println!("manifest root import rehearsal {namespace}: {total} imported, {} refused", report.refused.len());
    for refusal in &report.refused {
        println!("manifest root import rehearsal {namespace}: {refusal}");
    }
}
