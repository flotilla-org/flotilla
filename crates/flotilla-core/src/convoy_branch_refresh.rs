use flotilla_protocol::{result_set::ConvoyChangeRequest, RepositoryKey};

/// One refresh retains every repository result, including misses and failures,
/// so publishing the primary row and discovering subjects share provider reads.
#[derive(Clone, Debug)]
pub struct ConvoyBranchRefresh {
    pub primary: Result<Option<ConvoyChangeRequest>, String>,
    pub repositories: Vec<(RepositoryKey, Result<Option<ConvoyChangeRequest>, String>)>,
}
