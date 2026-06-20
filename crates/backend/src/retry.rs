use rustic_core::{ErrorKind, RusticError};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use std::fmt;
use std::str::FromStr;

#[serde_as]
#[derive(Default, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum RetrySetting {
    #[default]
    Disabled,
    Default,
    Count(usize),
}

impl fmt::Display for RetrySetting {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disabled => write!(f, "off"),
            Self::Default => write!(f, "default"),
            Self::Count(n) => write!(f, "{n}"),
        }
    }
}

impl FromStr for RetrySetting {
    type Err = Box<RusticError>;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "false" | "off" => Ok(Self::Disabled),
            "default" => Ok(Self::Default),
            value => {
                let count = value.parse::<usize>().map_err(|err| {
                    RusticError::with_source(
                        ErrorKind::InvalidInput,
                        "Parsing retry value `{value}` failed, the value must be a valid integer.",
                        err,
                    )
                    .attach_context("value", value.to_string())
                })?;

                Ok(Self::Count(count))
            }
        }
    }
}

impl RetrySetting {
    pub fn get_setting(&self, def: usize) -> usize {
        match self {
            RetrySetting::Disabled => 0,
            RetrySetting::Default => def,
            RetrySetting::Count(x) => *x,
        }
    }
}
