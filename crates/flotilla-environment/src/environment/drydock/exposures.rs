//! One vessel's connections, as exposures drydock makes and keeps on its own
//! machine (drydock#132). Each is named and stands until the vessel is
//! destroyed: drydock re-establishes it after a lost link, a resume or its own
//! restart, so the adapter sets one only when it is not already standing.
use super::*;
use sha2::Digest;
use wire::{Guest, Inward, InwardRequest, InwardTarget, Local, Outward, OutwardDestination, OutwardRequest};

#[derive(Clone)]
pub(super) struct Exposures {
    pub api: Arc<dyn ProviderApi>,
    pub instance: String,
    pub backing: String,
}

impl Exposures {
    /// The local Unix socket that reaches `target` in the vessel, under `name`.
    /// A standing exposure that is not ready is returned as it is: drydock keeps
    /// trying, and the caller's connection reports what fails.
    pub async fn inward(&self, name: &str, target: InwardTarget) -> Result<(PathBuf, Option<wire::ExposureError>), String> {
        let standing = match self.api.inward(&self.instance, &self.backing, name).await {
            Ok(inward) if inward.target == target => Some(inward),
            Ok(_) | Err(::drydock::client::Error::Protocol(wire::ProtocolError::NotFound { .. })) => None,
            Err(error) => return Err(exposure_error("inward", name, error)),
        };
        let inward = match standing {
            Some(inward) => inward,
            None => self
                .api
                .put_inward(&self.instance, &self.backing, name, &InwardRequest { target })
                .await
                .map_err(|error| exposure_error("inward", name, error))?
                .into_inner(),
        };
        let Inward { local: Local::Unix(local), error, .. } = inward;
        Ok((local.into(), error))
    }

    /// The local Unix socket that reaches `target`, once drydock reports it ready.
    pub async fn ready_inward(&self, name: &str, target: InwardTarget) -> Result<PathBuf, String> {
        match self.inward(name, target).await? {
            (local, None) => Ok(local),
            (_, Some(error)) => Err(format!("inward exposure {name}: {:?}: {}", error.code, error.reason)),
        }
    }

    /// The endpoint `request.guest` in the vessel, reaching `request.destination`
    /// on this machine, under `name`; set again only when it does not stand ready.
    pub async fn outward(&self, name: &str, request: OutwardRequest) -> Result<Outward, String> {
        match self.api.outward(&self.instance, &self.backing, name).await {
            Ok(outward) if outward.ready && outward.guest == request.guest && outward.destination == request.destination => {
                return Ok(outward);
            }
            Ok(_) | Err(::drydock::client::Error::Protocol(wire::ProtocolError::NotFound { .. })) => {}
            Err(error) => return Err(exposure_error("outward", name, error)),
        }
        let outward = self
            .api
            .put_outward(&self.instance, &self.backing, name, &request)
            .await
            .map_err(|error| exposure_error("outward", name, error))?
            .into_inner();
        match &outward.error {
            None => Ok(outward),
            Some(error) => Err(format!("outward exposure {name} at {}: {:?}: {}", guest(&outward.guest), error.code, error.reason)),
        }
    }
}

/// Exposure names are drydock file names: a guest path is named by its digest.
pub(super) fn path_name(prefix: &str, path: &Path) -> String {
    format!("{prefix}-{}", &format!("{:x}", sha2::Sha256::digest(path.as_os_str().as_encoded_bytes()))[..16])
}

pub(super) fn unix(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

pub(super) fn outward_unix(guest: &Path, destination: &Path) -> OutwardRequest {
    OutwardRequest { guest: Guest::Unix { unix: unix(guest) }, destination: OutwardDestination::Unix(unix(destination)) }
}

fn guest(guest: &Guest) -> String {
    match guest {
        Guest::Unix { unix } => unix.clone(),
        Guest::Name { name, port } => format!("{}:{}", name.as_str(), port.get()),
    }
}

/// A refusal's reason is drydock's own text about the request, never a credential.
fn exposure_error(direction: &str, name: &str, error: ::drydock::client::Error) -> String {
    use ::drydock::client::Error;
    match error {
        Error::Protocol(wire::ProtocolError::Refused { reason }) | Error::Protocol(wire::ProtocolError::NotFound { reason }) => {
            format!("{direction} exposure {name}: {reason}")
        }
        other => format!("{direction} exposure {name}: {}", api_error(other)),
    }
}
