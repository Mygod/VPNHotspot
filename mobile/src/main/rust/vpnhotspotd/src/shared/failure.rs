use std::fmt;
use std::io;

use crate::shared::ended::Ended;
use crate::shared::protocol::IoErrorReportExt;

/// One failed operation, classified by which side of it failed.
#[derive(Debug)]
pub enum Failure {
    /// This daemon's own local setup: creating a socket, binding it to the selected network, making it
    /// nonblocking, registering it with the runtime. Nothing a client or a peer can drive, so it becomes a
    /// structured report under [Local::context] - coalesced, like every other report.
    Local {
        context: &'static str,
        error: io::Error,
    },
    /// What the peer, the path or the platform answered: refused, unreachable, timed out, too busy. An
    /// ordinary per-operation outcome, never a report.
    Expected(io::Error),
}

impl Failure {
    /// The classifier for a setup step, shaped for `map_err`.
    pub fn local(context: &'static str) -> impl Fn(io::Error) -> Self {
        move |error| Self::Local { context, error }
    }

    /// What the platform answered, which is never this daemon's own failure.
    pub fn platform(error: io::Error) -> Self {
        Self::Expected(error)
    }

    /// The context a local failure is reported under, beside the error itself. `None` for everything the
    /// platform, the path or the peer answered.
    pub fn reportable(&self) -> Option<(&'static str, &io::Error)> {
        match self {
            Self::Local { context, error } => Some((context, error)),
            Self::Expected(_) => None,
        }
    }

    pub fn error(&self) -> &io::Error {
        match self {
            Self::Local { error, .. } | Self::Expected(error) => error,
        }
    }

    /// Splits one failure by whose it was: the error the owner meeting it has to end on, or the failure
    /// itself for the per-operation answer that owns it.
    #[track_caller]
    pub fn ending<I, K, V>(self, details: I) -> Result<Self, io::Error>
    where
        I: IntoIterator<Item = (K, V)>,
        K: ToString,
        V: ToString,
    {
        match self {
            Self::Local { context, error } => {
                Err(error.with_report_context_details(context, details))
            }
            expected => Ok(expected),
        }
    }

    /// How a worker's owner is told about it: `what` names the operation for the one line an expected outcome
    /// is worth.
    pub fn ended(self, what: &str) -> Ended {
        match self {
            Self::Local { context, error } => Ended::Failed { context, error },
            Self::Expected(error) => Ended::Reported(format!("{what} failed: {error}")),
        }
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Local { context, error } => write!(formatter, "{context}: {error}"),
            Self::Expected(error) => error.fmt(formatter),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::protocol::reported_io_error_report;

    fn errno(errno: i32) -> io::Error {
        io::Error::from_raw_os_error(errno)
    }

    #[test]
    fn a_local_setup_failure_is_structured_and_an_answer_is_not() {
        let local = Failure::local("shizuku.tcp_connect_bind")(errno(libc::EINVAL));
        assert_eq!(
            local.reportable().map(|(context, _)| context),
            Some("shizuku.tcp_connect_bind")
        );
        match local.ended("upstream connect") {
            Ended::Failed { context, error } => {
                assert_eq!(context, "shizuku.tcp_connect_bind");
                assert_eq!(error.raw_os_error(), Some(libc::EINVAL));
            }
            _ => panic!("the daemon's own setup failure must not be a per-record line"),
        }
        for code in [
            libc::ECONNREFUSED,
            libc::EHOSTUNREACH,
            libc::ENETUNREACH,
            libc::ETIMEDOUT,
            libc::ENONET,
        ] {
            let expected = Failure::Expected(errno(code));
            assert!(expected.reportable().is_none(), "{code}");
            match expected.ended("upstream connect") {
                Ended::Reported(line) => assert!(
                    line.starts_with("upstream connect failed: "),
                    "{code}: {line}"
                ),
                _ => panic!("{code} is an ordinary outcome, not a report"),
            }
        }
    }

    #[test]
    fn every_platform_resolver_outcome_stays_expected() {
        for code in [
            libc::EBUSY,
            libc::ETIMEDOUT,
            libc::ECONNREFUSED,
            libc::EINVAL,
            libc::ENOENT,
        ] {
            let failure = Failure::platform(errno(code));
            assert!(failure.reportable().is_none(), "{code}");
            assert_eq!(failure.error().raw_os_error(), Some(code));
            assert!(
                matches!(failure.ended("resolve"), Ended::Reported(_)),
                "{code}"
            );
        }
        let wrapper = Failure::local("resolver.register")(errno(libc::EMFILE));
        assert_eq!(
            wrapper.reportable().map(|(context, _)| context),
            Some("resolver.register")
        );
    }

    #[test]
    fn only_the_daemons_own_resolver_failure_ends_the_owner_that_met_it() {
        for code in [
            libc::EBUSY,
            libc::ETIMEDOUT,
            libc::ECONNREFUSED,
            libc::ENOENT,
        ] {
            let kept = Failure::platform(errno(code))
                .ending([("transaction", 7u64)])
                .expect("what the platform answered is one query's own outcome");
            assert_eq!(kept.error().raw_os_error(), Some(code));
            assert!(kept.reportable().is_none(), "{code}");
        }
        for context in ["resolver.nonblock", "resolver.register"] {
            let ending = Failure::local(context)(errno(libc::EMFILE))
                .ending([("transaction", 7u64)])
                .expect_err("the daemon's own wrapper step ends its owner");
            assert_eq!(ending.kind(), errno(libc::EMFILE).kind());
            let report = reported_io_error_report(&ending)
                .expect("exactly one report, built where it failed");
            assert_eq!(report.context, context);
            assert_eq!(report.errno, Some(libc::EMFILE));
            assert_eq!(
                report
                    .details
                    .iter()
                    .map(|detail| (detail.key.as_str(), detail.value.as_str()))
                    .collect::<Vec<_>>(),
                vec![("transaction", "7")]
            );
            assert_eq!(
                reported_io_error_report(&ending.with_report_context("teardown"))
                    .expect("still the first one")
                    .context,
                context
            );
        }
    }

    #[test]
    fn a_failure_names_its_step_when_it_is_the_daemons_own() {
        assert_eq!(
            Failure::local("resolver.nonblock")(errno(libc::EBADF)).to_string(),
            format!("resolver.nonblock: {}", errno(libc::EBADF))
        );
        assert_eq!(
            Failure::platform(errno(libc::EBUSY)).to_string(),
            errno(libc::EBUSY).to_string()
        );
    }
}
