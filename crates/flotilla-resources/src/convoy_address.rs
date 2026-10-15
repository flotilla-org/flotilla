use std::collections::BTreeSet;
pub fn convoy_disambiguation_address(role: &str, project: Option<&str>) -> String {
    format!("{role}@{}", project.unwrap_or_default())
}
pub fn parse_role_address(value: &str) -> Result<(&str, Option<&str>), String> {
    match value.split_once('@') {
        Some((role, project)) if !role.is_empty() && !project.contains('@') => Ok((role, Some(project))),
        Some(_) => Err(format!("invalid convoy address `{value}`: expected role@project")),
        None if value.is_empty() => Err("convoy role cannot be empty".to_string()),
        None => Ok((value, None)),
    }
}
pub struct ConvoyAddressIdentity<'a> {
    pub record_name: &'a str,
    pub role: Option<&'a str>,
    pub project: Option<&'a str>,
    pub terminal: bool,
}
pub fn resolve_convoy_candidate_indices(identities: &[ConvoyAddressIdentity<'_>], address: &str) -> Result<Vec<usize>, String> {
    let exact = identities
        .iter()
        .enumerate()
        .filter_map(|(index, identity)| (identity.record_name == address).then_some(index))
        .collect::<Vec<_>>();
    if !exact.is_empty() {
        return Ok(exact);
    }

    let (role, project) = parse_role_address(address)?;
    let matching = identities
        .iter()
        .enumerate()
        .filter_map(|(index, identity)| {
            (identity.role == Some(role) && project.is_none_or(|project| identity.project.unwrap_or_default() == project)).then_some(index)
        })
        .collect::<Vec<_>>();
    let (live, terminal): (Vec<_>, Vec<_>) = matching.into_iter().partition(|index| !identities[*index].terminal);
    let candidates = if live.is_empty() { terminal } else { live };
    let record_names = candidates.iter().map(|index| identities[*index].record_name).collect::<BTreeSet<_>>();
    if record_names.len() <= 1 {
        return Ok(candidates);
    }

    let address_options = candidates
        .iter()
        .filter_map(|index| identities[*index].role.map(|role| convoy_disambiguation_address(role, identities[*index].project)))
        .collect::<BTreeSet<_>>();
    if candidates.iter().all(|index| identities[*index].terminal) && address_options.len() == 1 {
        return Err(format!(
            "convoy address `{address}` matches multiple terminal records; use an exact record name: {}",
            record_names.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }
    if address_options.len() > 1 {
        return Err(format!(
            "convoy role `{role}` is ambiguous; use one of: {}",
            address_options.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }
    Err(format!(
        "convoy address `{address}` matches multiple records; use an exact record name: {}",
        record_names.into_iter().collect::<Vec<_>>().join(", ")
    ))
}
