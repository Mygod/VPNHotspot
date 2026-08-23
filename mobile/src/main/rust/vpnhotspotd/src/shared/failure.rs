//! Whose failure it was: this daemon's own local setup, or what the peer, the path or the platform answered.
//!
//! The distinction is not cosmetic and it cannot be recovered from an errno. An `EINVAL` from `setsockopt` on
//! a socket this process just created is a bug or a platform difference worth a structured report; an
//! `ECONNREFUSED` from a connect is the ordinary answer to asking. Both arrive as an `io::Error`, so whichever
//! step produced it is the only place that knows which one it is - and that is why classification happens at
//! the `map_err`, not at the report.
//!
//! What rides on it is flood resistance. Every one of these operations is reachable from packet input by an
//! unknown local principal: a client chooses how many connections it opens and how many names it looks up. A
//! structured report per expected outcome would therefore be a report flood the client drives, so an expected
//! outcome is at most one line per record. A local setup failure is not client-driven and is reported in full,
//! with the coalescer bounding a repeat.

use std::fmt;
use std::io;

use crate::shared::ended::Ended;

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
    ///
    /// `Fn` rather than `FnOnce` so one context can classify several steps of the same operation.
    pub fn local(context: &'static str) -> impl Fn(io::Error) -> Self {
        move |error| Self::Local { context, error }
    }

    /// What the platform answered, which is never this daemon's own failure.
    ///
    /// Named for the resolver, where every outcome comes back as an errno and the temptation to report them is
    /// strongest: `EBUSY` is the per-UID limiter being full, `ETIMEDOUT` is a resolver that did not answer in
    /// time, and the rest is whatever the remote said. None of it is the daemon's doing, all of it is
    /// reachable from a client's own queries, and all of it reaches that client as SERVFAIL.
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

    fn errno(errno: i32) -> io::Error {
        io::Error::from_raw_os_error(errno)
    }

    /// The whole table, in the two directions that matter: a local setup failure has to arrive as a
    /// structured report naming its step, and everything a client can drive has to arrive as one line.
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
        // Every one of these is something a client's own traffic produces at will.
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

    /// The resolver's own table. `EBUSY` is the platform's per-UID limiter, which a burst of client queries
    /// reaches on its own, so it must stay as expected as a timeout does.
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
        // And the wrapper around it is the daemon's own, so that one is reported.
        let wrapper = Failure::local("resolver.register")(errno(libc::EMFILE));
        assert_eq!(
            wrapper.reportable().map(|(context, _)| context),
            Some("resolver.register")
        );
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
