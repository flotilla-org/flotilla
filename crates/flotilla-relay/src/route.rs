//! Pure public route classification, shared by the Worker handler and host tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Method {
    Get,
    Post,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Route<'a> {
    Ingress { install: &'a str, source: &'a str },
    Stream { install: &'a str },
    Ack { install: &'a str },
}

impl<'a> Route<'a> {
    pub(crate) fn install(self) -> &'a str {
        match self {
            Self::Ingress { install, .. } | Self::Stream { install } | Self::Ack { install } => install,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RouteError {
    InvalidInstall,
    NotFound,
}

pub(crate) fn parse(method: Method, path: &str) -> Result<Route<'_>, RouteError> {
    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
    let ["i", install, tail @ ..] = segments.as_slice() else { return Err(RouteError::NotFound) };
    if install.is_empty() || !install.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_') {
        return Err(RouteError::InvalidInstall);
    }
    match (method, tail) {
        (Method::Post, ["stream", "ack"]) => Ok(Route::Ack { install }),
        (Method::Get, ["stream"]) => Ok(Route::Stream { install }),
        (Method::Post, [source]) => Ok(Route::Ingress { install, source }),
        _ => Err(RouteError::NotFound),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_routes_and_rejects_invalid_install_or_method() {
        let ingress = parse(Method::Post, "/i/install-1/github").expect("ingress route");
        assert_eq!(ingress, Route::Ingress { install: "install-1", source: "github" });
        assert_eq!(ingress.install(), "install-1");
        assert_eq!(parse(Method::Get, "/i/install-1/stream"), Ok(Route::Stream { install: "install-1" }));
        assert_eq!(parse(Method::Post, "/i/install-1/stream/ack"), Ok(Route::Ack { install: "install-1" }));
        assert_eq!(parse(Method::Get, "/i/bad.install/stream"), Err(RouteError::InvalidInstall));
        assert_eq!(parse(Method::Other, "/i/install-1/stream"), Err(RouteError::NotFound));
        assert_eq!(parse(Method::Post, "/i/install-1/stream/extra"), Err(RouteError::NotFound));
    }
}
