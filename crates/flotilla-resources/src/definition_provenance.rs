//! The provenance keys the old store's definition writers stamp as labels and annotations: the
//! manifest reconciler, charter delegation, project operations and project image layers. They are
//! declared here, below every writer, so the cut-over import (#3824 layer 2b) maps them onto a
//! definition's typed origin with the writers' own keys. They go with the old store.

pub const MANIFEST_MANAGED_BY_VALUE: &str = "manifest";
pub const MANIFEST_SOURCE_ANNOTATION: &str = "flotilla.work/manifest-source";
pub const MANIFEST_PATH_ANNOTATION: &str = "flotilla.work/manifest-path";
pub const MANIFEST_REVISION_ANNOTATION: &str = "flotilla.work/manifest-revision";
pub const MANIFEST_RECONCILER_ROOT_ANNOTATION: &str = "flotilla.work/manifest-reconciler-root";
pub const CHARTER_SOURCE_ANNOTATION: &str = "flotilla.work/charter-source";
pub const CHARTER_REVISION_ANNOTATION: &str = "flotilla.work/charter-revision";
pub const CHARTER_SCOPE_ANNOTATION: &str = "flotilla.work/charter-scope";
pub const MATERIALIZED_PROJECT_ANNOTATION: &str = "flotilla.work/materialized-project";
pub const SOURCE_REPOSITORY_ANNOTATION: &str = "flotilla.work/source-repository";
pub const SOURCE_COMMIT_ANNOTATION: &str = "flotilla.work/source-commit";
pub const SOURCE_ENTRY_PATH_ANNOTATION: &str = "flotilla.work/source-entry-path";
pub const PROJECT_IMAGE_MANAGER: &str = "project-image";

/// Repository's provenance keys (#3824 layer 3a): the project whose operational entries declare a
/// repository's verification commands, those entries' provenance, and the record a forge-split
/// sweep merged a repository into. Declared here for the import, below every writer.
pub const VERIFICATION_PROJECT_ANNOTATION: &str = "flotilla.work/verification-command-project";
pub const VERIFICATION_PROVENANCE_ANNOTATION: &str = "flotilla.work/verification-command-provenance";
pub const SUPERSEDED_BY_ANNOTATION: &str = "flotilla.work/superseded-by";

/// Project's provenance keys (#3824 layer 3b): the repository, commit and file of the
/// `project.yaml` declaration project operations materialised a Project from, and the managed-by
/// value of a Project generated for a whole repository. Moved here from `flotilla-charter` and
/// project operations for the import, below every writer.
pub const BOOTSTRAP_REPOSITORY_ANNOTATION: &str = "flotilla.work/project-bootstrap-repository";
pub const BOOTSTRAP_COMMIT_ANNOTATION: &str = "flotilla.work/project-bootstrap-commit";
pub const DECLARATION_FILE_ANNOTATION: &str = "flotilla.work/project-declaration-file";
pub const WHOLE_REPOSITORY_PROJECT_MANAGER: &str = "whole-repository-project";
