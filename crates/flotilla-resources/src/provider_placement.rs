//! Provider matching is a filter over declarations, never a ranking policy:
//! admission freezes the instance it matches, and realisation matches the same
//! way for a vessel no admission froze one for.
use crate::FulfilmentRealisation;
use flotilla_protocol::provider::{CheckoutStrategy, EnvironmentClass, MaterialMode, ProviderAdvertisement, ProviderReadiness};

/// The provider classes that can realise `realisation`, and the checkout it takes.
fn realised_by(realisation: &FulfilmentRealisation) -> (&'static str, &'static [EnvironmentClass], CheckoutStrategy) {
    const HOST_DIRECT: &[EnvironmentClass] = &[EnvironmentClass::HostDirect];
    const PER_VESSEL: &[EnvironmentClass] = &[EnvironmentClass::Container, EnvironmentClass::MicroVm, EnvironmentClass::Dummy];
    const CLOUD_SANDBOX: &[EnvironmentClass] = &[EnvironmentClass::CloudSandbox];
    match realisation {
        FulfilmentRealisation::HostDirect => ("host-direct", HOST_DIRECT, CheckoutStrategy::HostWorktree),
        FulfilmentRealisation::DockerPerVessel { .. } => ("per-vessel", PER_VESSEL, CheckoutStrategy::HostWorktree),
        FulfilmentRealisation::CloudSandboxPerVessel { .. } => ("cloud sandbox", CLOUD_SANDBOX, CheckoutStrategy::CloneInVessel),
    }
}

/// The checkout a realisation takes.
pub fn checkout_for(realisation: &FulfilmentRealisation) -> CheckoutStrategy {
    realised_by(realisation).2
}

/// Resolve the one provider instance that realises a kind on `host`: the frozen instance when
/// one is given, else the only advertised provider of the realisation's classes.
pub fn match_provider<'a>(
    host: &str,
    realisation: &FulfilmentRealisation,
    instance: Option<&str>,
    advertisements: &'a [ProviderAdvertisement],
) -> Result<&'a ProviderAdvertisement, String> {
    let (strategy, admits, checkout) = realised_by(realisation);
    let class_of = |provider: &ProviderAdvertisement| provider.kind.as_ref().map(|kind| kind.class);
    let identity_matches = advertisements
        .iter()
        .filter(|provider| match instance {
            Some(instance) => provider.instance == instance,
            None => class_of(provider).is_some_and(|class| admits.contains(&class)),
        })
        .collect::<Vec<_>>();
    if identity_matches.is_empty() {
        return Err(match instance {
            Some(instance) => format!("{host}: no advertised provider is instance `{instance}`"),
            None => format!("{host}: no advertised provider realises a {strategy} kind"),
        });
    }
    // Duplicates count across the whole slice, including entries of other classes.
    for provider in &identity_matches {
        if advertisements.iter().filter(|other| other.instance == provider.instance).count() != 1 {
            return Err(format!("{host}: ambiguous provider instance `{}` advertised more than once", provider.instance));
        }
    }
    let mut matching = Vec::new();
    let mut reasons = Vec::new();
    for provider in identity_matches {
        let missing = match class_of(provider).filter(|class| !admits.contains(class)) {
            Some(class) => Some(format!("class {class:?} cannot realise a {strategy} kind")),
            None => missing_requirement(provider, checkout),
        };
        match missing {
            None => matching.push(provider),
            Some(reason) => reasons.push(format!("{host}: {} {reason}", provider.instance)),
        }
    }
    match matching.as_slice() {
        [provider] => Ok(provider),
        [] => Err(reasons.join("; ")),
        _ => Err(format!(
            "{host}: ambiguous provider instances: {}",
            matching.iter().map(|provider| provider.instance.as_str()).collect::<Vec<_>>().join(", ")
        )),
    }
}

fn missing_requirement(provider: &ProviderAdvertisement, checkout: CheckoutStrategy) -> Option<String> {
    // When several defects coexist, report the actionable readiness failure
    // first. Both unknown readiness and incomplete identity still fail closed.
    match &provider.readiness {
        ProviderReadiness::Ready => {}
        ProviderReadiness::MissingTool { reason } => return Some(format!("missing tool: {reason}")),
        ProviderReadiness::MissingCredential { reason } => return Some(format!("missing credential: {reason}")),
        ProviderReadiness::Unknown { reason } => return Some(format!("readiness unknown: {reason}")),
    }
    if provider.instance.trim().is_empty() || provider.kind.is_none() {
        return Some("has incomplete identity".into());
    }
    let capabilities = &provider.capabilities;
    if !capabilities.checkout_strategies.contains(&checkout) {
        return Some(format!("ready but no {checkout:?} checkout"));
    }
    if checkout == CheckoutStrategy::HostWorktree && !capabilities.material_modes.contains(&MaterialMode::Shared) {
        return Some(format!("ready but no {:?} material", MaterialMode::Shared));
    }
    None
}
