// crates/flotilla-protocol/src/namespace.rs
//
// Wire types for the namespace-scoped stream carrying convoy state.
// Parallel to RepoSnapshot / HostSnapshot for the per-repo / per-host streams.
// Shape deliberately mirrors ConvoyStatus fields rather than introducing a new
// vocabulary — easier to replace when the wire protocol shifts k8s-shape.
