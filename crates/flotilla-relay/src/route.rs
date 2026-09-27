//! Pure public route classification, shared by the Worker and the Durable Object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Method {
    Get,
    Post,
    Delete,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Route<'a> {
    Ingress { install: &'a str, source: &'a str },
    Stream { install: &'a str },
    Ack { install: &'a str },
    Admin { install: &'a str, op: AdminOp<'a> },
}

/// Operator provisioning operations; see `flotilla_relay_protocol::admin`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AdminOp<'a> {
    CreateInstall,
    DescribeInstall,
    DeleteInstall,
    MintToken,
    RevokeToken { id: &'a str },
    AddSecret { source: &'a str },
    RevokeSecret { source: &'a str, id: &'a str },
}

impl<'a> Route<'a> {
    pub(crate) fn install(self) -> &'a str {
        match self {
            Self::Ingress { install, .. } | Self::Stream { install } | Self::Ack { install } | Self::Admin { install, .. } => install,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RouteError {
    InvalidInstall,
    NotFound,
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && name.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

pub(crate) fn parse(method: Method, path: &str) -> Result<Route<'_>, RouteError> {
    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
    match segments.as_slice() {
        ["i", install, tail @ ..] => {
            if !valid_name(install) {
                return Err(RouteError::InvalidInstall);
            }
            match (method, tail) {
                (Method::Post, ["stream", "ack"]) => Ok(Route::Ack { install }),
                (Method::Get, ["stream"]) => Ok(Route::Stream { install }),
                (Method::Post, [source]) if *source != "stream" => Ok(Route::Ingress { install, source }),
                _ => Err(RouteError::NotFound),
            }
        }
        ["admin", "installs", install, tail @ ..] => {
            if !valid_name(install) {
                return Err(RouteError::InvalidInstall);
            }
            let op = match (method, tail) {
                (Method::Post, []) => AdminOp::CreateInstall,
                (Method::Get, []) => AdminOp::DescribeInstall,
                (Method::Delete, []) => AdminOp::DeleteInstall,
                (Method::Post, ["tokens"]) => AdminOp::MintToken,
                (Method::Delete, ["tokens", id]) => AdminOp::RevokeToken { id },
                (Method::Post, ["sources", source, "secrets"]) => AdminOp::AddSecret { source },
                (Method::Delete, ["sources", source, "secrets", id]) => AdminOp::RevokeSecret { source, id },
                _ => return Err(RouteError::NotFound),
            };
            Ok(Route::Admin { install, op })
        }
        _ => Err(RouteError::NotFound),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_mailbox_routes_and_rejects_invalid_install_or_method() {
        let ingress = parse(Method::Post, "/i/install-1/github").expect("ingress route");
        assert_eq!(ingress, Route::Ingress { install: "install-1", source: "github" });
        assert_eq!(ingress.install(), "install-1");
        assert_eq!(parse(Method::Get, "/i/install-1/stream"), Ok(Route::Stream { install: "install-1" }));
        assert_eq!(parse(Method::Post, "/i/install-1/stream/ack"), Ok(Route::Ack { install: "install-1" }));
        assert_eq!(parse(Method::Post, "/i/install-1/stream"), Err(RouteError::NotFound));
        assert_eq!(parse(Method::Get, "/i/bad.install/stream"), Err(RouteError::InvalidInstall));
        assert_eq!(parse(Method::Get, &format!("/i/{}/stream", "x".repeat(65))), Err(RouteError::InvalidInstall));
        assert_eq!(parse(Method::Other, "/i/install-1/stream"), Err(RouteError::NotFound));
        assert_eq!(parse(Method::Post, "/i/install-1/stream/extra"), Err(RouteError::NotFound));
    }

    #[test]
    fn classifies_admin_routes() {
        let admin = |method, path| {
            parse(method, path).map(|route| match route {
                Route::Admin { install: "lab", op } => op,
                other => panic!("expected admin route for lab, got {other:?}"),
            })
        };
        assert_eq!(admin(Method::Post, "/admin/installs/lab"), Ok(AdminOp::CreateInstall));
        assert_eq!(admin(Method::Get, "/admin/installs/lab"), Ok(AdminOp::DescribeInstall));
        assert_eq!(admin(Method::Delete, "/admin/installs/lab"), Ok(AdminOp::DeleteInstall));
        assert_eq!(admin(Method::Post, "/admin/installs/lab/tokens"), Ok(AdminOp::MintToken));
        assert_eq!(admin(Method::Delete, "/admin/installs/lab/tokens/t1"), Ok(AdminOp::RevokeToken { id: "t1" }));
        assert_eq!(admin(Method::Post, "/admin/installs/lab/sources/github/secrets"), Ok(AdminOp::AddSecret { source: "github" }));
        assert_eq!(
            admin(Method::Delete, "/admin/installs/lab/sources/github/secrets/s1"),
            Ok(AdminOp::RevokeSecret { source: "github", id: "s1" })
        );
        assert_eq!(parse(Method::Get, "/admin/installs/lab/tokens"), Err(RouteError::NotFound));
        assert_eq!(parse(Method::Post, "/admin/installs/bad.name"), Err(RouteError::InvalidInstall));
        assert_eq!(parse(Method::Get, "/admin/installs"), Err(RouteError::NotFound));
    }
}
