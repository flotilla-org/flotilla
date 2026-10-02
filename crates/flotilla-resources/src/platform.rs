use std::{fmt, str::FromStr};

/// The supported platform vocabulary for capability needs, fulfilment grants
/// and project platform matrices. Stored records keep platforms as strings
/// (ADR 0047), so this type validates and classifies those strings rather
/// than replacing them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Platform {
    Linux,
    Macos,
    Windows,
}

impl Platform {
    pub const ALL: [Self; 3] = [Self::Linux, Self::Macos, Self::Windows];

    /// Role-need placeholder that admission expands over the Project's
    /// platform matrix. It is not itself a platform.
    pub const MATRIX_PLACEHOLDER: &'static str = "$matrix";

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Linux => "linux",
            Self::Macos => "macos",
            Self::Windows => "windows",
        }
    }

    /// Scarce platform capacity, held back for work that names it while an
    /// unreserved candidate also covers the needs (ADR 0046 §4).
    pub const fn is_reserved(self) -> bool {
        matches!(self, Self::Macos | Self::Windows)
    }
}

impl FromStr for Platform {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL.into_iter().find(|platform| platform.as_str() == value).ok_or_else(|| format!("unknown platform `{value}`"))
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
