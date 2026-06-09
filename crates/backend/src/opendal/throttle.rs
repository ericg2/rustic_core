use std::fmt;
use std::str::FromStr;
use bytesize::ByteSize;
use derive_setters::Setters;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use rustic_core::{ErrorKind, RusticError, RusticResult};

/// Throttling parameters
///
/// Note: Throttle implements [`FromStr`] to read it from something like "10kiB,10MB"
#[serde_as]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Setters)]
#[setters(into)]
#[non_exhaustive]
pub struct Throttle {
    pub(crate) bandwidth: u32,
    pub(crate) burst: u32,
}

impl Default for Throttle {
    fn default() -> Self {
        Self {
            bandwidth: u32::MAX,
            burst: u32::MAX,
        }
    }
}

impl fmt::Display for Throttle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{},{}",
            ByteSize::b(u64::from(self.bandwidth)),
            ByteSize::b(u64::from(self.burst)),
        )
    }
}

impl FromStr for Throttle {
    type Err = Box<RusticError>;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut values = s
            .split(',')
            .map(|s| {
                ByteSize::from_str(s.trim()).map_err(|err| {
                    RusticError::with_source(
                        ErrorKind::InvalidInput,
                        "Parsing ByteSize from throttle string `{string}` failed",
                        err,
                    )
                        .attach_context("string", s)
                })
            })
            .map(|b| -> RusticResult<u32> {
                let bytesize = b?.as_u64();
                bytesize.try_into().map_err(|err| {
                    RusticError::with_source(
                        ErrorKind::Internal,
                        "Converting ByteSize `{bytesize}` to u32 failed",
                        err,
                    )
                        .attach_context("bytesize", bytesize.to_string())
                })
            });

        let bandwidth = values
            .next()
            .transpose()?
            .ok_or_else(|| RusticError::new(ErrorKind::MissingInput, "No bandwidth given."))?;

        let burst = values
            .next()
            .transpose()?
            .ok_or_else(|| RusticError::new(ErrorKind::MissingInput, "No burst given."))?;

        Ok(Self { bandwidth, burst })
    }
}
