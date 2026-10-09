//! Transport-related configuration types.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::address::{NetLocation, NetLocationPortRange};
use crate::option_util::OneOrSome;

use super::common::default_true;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum BindLocation {
    #[serde(alias = "addresses")]
    Address(OneOrSome<NetLocationPortRange>),
    Path(PathBuf),
}

impl std::fmt::Display for BindLocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BindLocation::Address(addresses) => {
                for (index, address) in addresses.iter().enumerate() {
                    if index > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{address}")?;
                }
                Ok(())
            }
            BindLocation::Path(p) => write!(f, "{}", p.display()),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    #[default]
    Tcp,
    Udp,
}

impl Transport {
    /// Returns true if this is the default transport (TCP)
    pub fn is_default(&self) -> bool {
        matches!(self, Transport::Tcp)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TcpConfig {
    #[serde(default = "default_true")]
    pub no_delay: bool,
}

impl Default for TcpConfig {
    fn default() -> Self {
        TcpConfig { no_delay: true }
    }
}

impl From<NetLocation> for BindLocation {
    fn from(loc: NetLocation) -> Self {
        BindLocation::Address(OneOrSome::One(loc.into()))
    }
}
