use log::{Level, log};
use std::fmt::Display;
use opendal_ext::{Error, ErrorKind};
use opendal_ext::layers::LoggingInterceptor;
use opendal_ext::raw::{AccessorInfo, Operation};

static LOGGING_TARGET: &str = "opendal::services";

/// The DefaultLoggingInterceptor will log the message by the standard logging macro.
#[derive(Clone, Copy, Debug, Default)]
pub struct OpenLogLayer;

impl LoggingInterceptor for OpenLogLayer {
    #[inline]
    fn log(
        &self,
        info: &AccessorInfo,
        operation: Operation,
        context: &[(&str, &str)],
        message: &str,
        err: Option<&Error>,
    ) {
        if let Some(err) = err {
            // Print error if it's unexpected, otherwise in warn.
            if err.kind() == ErrorKind::NotFound && operation == Operation::Stat {
                return; // *** we don't need to log this condition in Rustic.
            }

            let lvl = if err.kind() == ErrorKind::Unexpected {
                Level::Error
            } else {
                Level::Warn
            };

            log!(
                target: LOGGING_TARGET,
                lvl,
                "service={} name={}{}: {operation} {message} {}",
                info.scheme(),
                info.name(),
                LoggingContext(context),
                // Print error message with debug output while unexpected happened.
                //
                // It's super sad that we can't bind `format_args!()` here.
                // See: https://github.com/rust-lang/rust/issues/92698
                if err.kind() != ErrorKind::Unexpected {
                   format!("{err}")
                } else {
                   format!("{err:?}")
                }
            );
        }

        log!(
            target: LOGGING_TARGET,
            Level::Debug,
            "service={} name={}{}: {operation} {message}",
            info.scheme(),
            info.name(),
            LoggingContext(context),
        );
    }
}

struct LoggingContext<'a>(&'a [(&'a str, &'a str)]);

impl Display for LoggingContext<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (k, v) in self.0.iter() {
            write!(f, " {k}={v}")?;
        }
        Ok(())
    }
}
