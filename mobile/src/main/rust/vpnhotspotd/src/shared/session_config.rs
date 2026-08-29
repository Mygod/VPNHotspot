use crate::shared::proto::daemon::ShizukuSessionConfig;

/// Why a config was refused. Each is terminal for the session: a peer that sent one is not one whose next
/// message can be trusted either, and there is nothing to resynchronize on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Invalid {
    /// Zero on the retirement counter. Zero is what an unset proto field decodes to, so admitting it would
    /// let a truncated or default-constructed message look like a valid first config - and the daemon's own
    /// zero-initialised view would then compare equal to it and skip the retirement it owes.
    Unset(&'static str),
    /// A counter that did not advance where it had to, or moved backwards.
    NotAdvanced {
        field: &'static str,
        previous: u64,
        next: u64,
    },
    /// A field the dataplane pins state behind changed without the generation that retires that state
    /// advancing.
    Unretired(&'static str),
}

/// Checks one config against its predecessor, or against nothing for the first.
pub fn check(
    previous: Option<&ShizukuSessionConfig>,
    next: &ShizukuSessionConfig,
) -> Result<(), Invalid> {
    if next.upstream_generation == 0 {
        return Err(Invalid::Unset("upstream_generation"));
    }
    let Some(previous) = previous else {
        return Ok(());
    };
    if next.upstream_generation < previous.upstream_generation {
        return Err(Invalid::NotAdvanced {
            field: "upstream_generation",
            previous: previous.upstream_generation,
            next: next.upstream_generation,
        });
    }
    // The egress handle and the interface its replies must arrive on are one fact, and every upstream socket
    // and resolver submission is bound to it. Changing either without advancing the generation would leave
    // sockets bound to a network the config no longer names, with nothing having retired them.
    if next.upstream_generation == previous.upstream_generation
        && (previous.upstream_network != next.upstream_network
            || previous.upstream_interface_index != next.upstream_interface_index)
    {
        return Err(Invalid::Unretired("upstream_network"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{check, Invalid};
    use crate::shared::proto::daemon::ShizukuSessionConfig;

    fn base() -> ShizukuSessionConfig {
        ShizukuSessionConfig {
            upstream_generation: 1,
            admit: false,
            upstream_network: Some(0x1234),
            upstream_interface_index: Some(7),
        }
    }

    #[test]
    fn the_first_config_needs_a_nonzero_generation() {
        assert_eq!(Ok(()), check(None, &base()));
    }

    #[test]
    fn a_zero_generation_is_an_unset_field_rather_than_a_value() {
        let config = ShizukuSessionConfig {
            upstream_generation: 0,
            ..base()
        };
        assert_eq!(
            Err(Invalid::Unset("upstream_generation")),
            check(None, &config)
        );
        assert_eq!(
            Err(Invalid::Unset("upstream_generation")),
            check(Some(&base()), &config)
        );
    }

    #[test]
    fn an_admit_only_update_is_accepted_with_an_unchanged_generation() {
        let previous = base();
        let next = ShizukuSessionConfig {
            admit: true,
            ..base()
        };
        assert_eq!(Ok(()), check(Some(&previous), &next));
        let closing = ShizukuSessionConfig {
            admit: false,
            ..base()
        };
        assert_eq!(Ok(()), check(Some(&next), &closing));
        let reopened = ShizukuSessionConfig {
            admit: true,
            ..base()
        };
        assert_eq!(Ok(()), check(Some(&closing), &reopened));
    }

    #[test]
    fn the_generation_may_not_move_backwards() {
        let previous = ShizukuSessionConfig {
            upstream_generation: 4,
            ..base()
        };
        assert_eq!(
            Err(Invalid::NotAdvanced {
                field: "upstream_generation",
                previous: 4,
                next: 3,
            }),
            check(
                Some(&previous),
                &ShizukuSessionConfig {
                    upstream_generation: 3,
                    ..base()
                }
            )
        );
    }

    #[test]
    fn the_upstream_may_not_move_without_the_generation() {
        let previous = base();
        for mutate in [
            (|c: &mut ShizukuSessionConfig| c.upstream_network = Some(0x5678)) as fn(&mut _),
            |c: &mut ShizukuSessionConfig| c.upstream_network = None,
            |c: &mut ShizukuSessionConfig| c.upstream_interface_index = Some(9),
            |c: &mut ShizukuSessionConfig| c.upstream_interface_index = None,
        ] {
            let mut next = base();
            mutate(&mut next);
            assert_eq!(
                Err(Invalid::Unretired("upstream_network")),
                check(Some(&previous), &next)
            );
            next.upstream_generation = 2;
            assert_eq!(Ok(()), check(Some(&previous), &next));
        }
    }
}
