//! What a `ShizukuSessionConfig` is allowed to say next, given what the last one said.
//!
//! The control contract is level-triggered, so the daemon acts on whatever the newest config claims. That
//! makes the *shape* of a config an invariant rather than a formality: a field the dataplane has pinned state
//! behind can only change together with the axis that retires that state, and an axis that went backwards or
//! a sequence that repeated means the two sides no longer agree about which session this is.
//!
//! Enforced here rather than at each use, and in the library rather than beside the session loop, because this
//! is the part with no I/O in it: every rule below is a comparison of two decoded messages, which is what
//! makes it table-testable without a device, a TUN, or a controller.

use crate::shared::proto::daemon::ShizukuSessionConfig;

/// Why a config was refused. Each is terminal for the session: a peer that sent one is not one whose next
/// message can be trusted either, and there is nothing to resynchronize on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Invalid {
    /// Zero on any of the three counters. Zero is what an unset proto field decodes to, so admitting it would
    /// let a truncated or default-constructed message look like a valid first config - and the daemon's own
    /// zero-initialised view would then compare equal to it and skip the retirement it owes.
    Unset(&'static str),
    /// A counter that did not advance where it had to, or moved backwards.
    NotAdvanced {
        field: &'static str,
        previous: u64,
        next: u64,
    },
    /// A field the dataplane pins state behind changed without the axis that retires that state advancing.
    Unretired(&'static str),
}

/// Checks one config against its predecessor, or against nothing for the first.
///
/// The admit flag is deliberately absent from every rule: an update that only opens or closes admission, with
/// both axes unchanged, is the ordinary way a session moves between `ACTIVE` and everything else, and it
/// retires nothing.
pub fn check(
    previous: Option<&ShizukuSessionConfig>,
    next: &ShizukuSessionConfig,
) -> Result<(), Invalid> {
    for (field, value) in [
        ("sequence", next.sequence),
        ("upstream_generation", next.upstream_generation),
        ("downstream_epoch", next.downstream_epoch),
    ] {
        if value == 0 {
            return Err(Invalid::Unset(field));
        }
    }
    let Some(previous) = previous else {
        return Ok(());
    };
    // Strictly increasing, because it is what matches an acknowledgement to the config that caused it: a
    // repeat would let the wrong config be acknowledged, and the app treats an acknowledgement naming the
    // wrong sequence as a failure it cannot tell apart from a lost one.
    if next.sequence <= previous.sequence {
        return Err(Invalid::NotAdvanced {
            field: "sequence",
            previous: previous.sequence,
            next: next.sequence,
        });
    }
    for (field, before, after) in [
        (
            "upstream_generation",
            previous.upstream_generation,
            next.upstream_generation,
        ),
        (
            "downstream_epoch",
            previous.downstream_epoch,
            next.downstream_epoch,
        ),
    ] {
        if after < before {
            return Err(Invalid::NotAdvanced {
                field,
                previous: before,
                next: after,
            });
        }
    }
    // The egress handle and the interface its replies must arrive on are one fact, and every upstream socket
    // and resolver submission is bound to it. Changing either without advancing the generation would leave
    // sockets bound to a network the config no longer names, with nothing having retired them.
    let generation_advanced = next.upstream_generation > previous.upstream_generation;
    if !generation_advanced
        && (previous.upstream_network != next.upstream_network
            || previous.upstream_interface_index != next.upstream_interface_index)
    {
        return Err(Invalid::Unretired("upstream_network"));
    }
    // These come from the agent's `LinkProperties`, which the design builds once and never mutates: the
    // virtual addresses are matched exactly on ingress and the gateway addresses are what an originated ICMP
    // error is sourced from. There is no axis that retires them, so they may not change at all within a
    // session - a change means this is a different session's config on the same connection.
    if previous.virtual_addresses != next.virtual_addresses {
        return Err(Invalid::Unretired("virtual_addresses"));
    }
    if previous.gateway_addresses != next.gateway_addresses {
        return Err(Invalid::Unretired("gateway_addresses"));
    }
    // The floor is what every already-queued packet was sized against. A packet built for the old floor and a
    // floor that has already moved cannot both be right, and the epoch is the axis that retires the queue - so
    // a floor that moves without it is a downstream change nothing retired. The floor *not* moving is the
    // ordinary case at every other axis, which is why an epoch may advance on its own.
    if next.downstream_epoch == previous.downstream_epoch
        && previous.downstream_mtu_floor != next.downstream_mtu_floor
    {
        return Err(Invalid::Unretired("downstream_mtu_floor"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{check, Invalid};
    use crate::shared::proto::daemon::ShizukuSessionConfig;

    /// The shape every case below varies one field of, so what a case is *about* is the difference.
    fn base() -> ShizukuSessionConfig {
        ShizukuSessionConfig {
            sequence: 1,
            upstream_generation: 1,
            downstream_epoch: 1,
            admit: false,
            upstream_network: Some(0x1234),
            upstream_interface_index: Some(7),
            virtual_addresses: vec![vec![192, 0, 2, 5]],
            gateway_addresses: vec![vec![192, 0, 2, 1]],
            downstream_mtu_floor: 1500,
        }
    }

    #[test]
    fn the_first_config_needs_only_nonzero_counters() {
        assert_eq!(Ok(()), check(None, &base()));
    }

    #[test]
    fn a_zero_counter_is_an_unset_field_rather_than_a_value() {
        for (field, mutate) in [
            (
                "sequence",
                (|c: &mut ShizukuSessionConfig| c.sequence = 0) as fn(&mut _),
            ),
            ("upstream_generation", |c: &mut ShizukuSessionConfig| {
                c.upstream_generation = 0
            }),
            ("downstream_epoch", |c: &mut ShizukuSessionConfig| {
                c.downstream_epoch = 0
            }),
        ] {
            let mut config = base();
            mutate(&mut config);
            assert_eq!(Err(Invalid::Unset(field)), check(None, &config), "{field}");
            // and a zero is refused just as firmly on a later config as on the first
            assert_eq!(
                Err(Invalid::Unset(field)),
                check(Some(&base()), &config),
                "{field}"
            );
        }
    }

    /// The case the contract exists to permit: closing or opening admission retires nothing.
    #[test]
    fn an_admit_only_update_is_accepted_with_unchanged_axes() {
        let previous = base();
        let next = ShizukuSessionConfig {
            sequence: 2,
            admit: true,
            ..base()
        };
        assert_eq!(Ok(()), check(Some(&previous), &next));
        let closing = ShizukuSessionConfig {
            sequence: 3,
            admit: false,
            ..base()
        };
        assert_eq!(Ok(()), check(Some(&next), &closing));
    }

    #[test]
    fn the_sequence_must_strictly_increase() {
        let previous = ShizukuSessionConfig {
            sequence: 5,
            ..base()
        };
        for sequence in [1u64, 5] {
            let next = ShizukuSessionConfig { sequence, ..base() };
            assert_eq!(
                Err(Invalid::NotAdvanced {
                    field: "sequence",
                    previous: 5,
                    next: sequence,
                }),
                check(Some(&previous), &next),
                "{sequence}"
            );
        }
    }

    #[test]
    fn neither_axis_may_move_backwards() {
        let previous = ShizukuSessionConfig {
            sequence: 1,
            upstream_generation: 4,
            downstream_epoch: 4,
            ..base()
        };
        for (field, mutate) in [
            (
                "upstream_generation",
                (|c: &mut ShizukuSessionConfig| c.upstream_generation = 3) as fn(&mut _),
            ),
            ("downstream_epoch", |c: &mut ShizukuSessionConfig| {
                c.downstream_epoch = 3
            }),
        ] {
            let mut next = ShizukuSessionConfig {
                sequence: 2,
                upstream_generation: 4,
                downstream_epoch: 4,
                ..base()
            };
            mutate(&mut next);
            assert_eq!(
                Err(Invalid::NotAdvanced {
                    field,
                    previous: 4,
                    next: 3,
                }),
                check(Some(&previous), &next),
                "{field}"
            );
        }
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
            let mut next = ShizukuSessionConfig {
                sequence: 2,
                ..base()
            };
            mutate(&mut next);
            assert_eq!(
                Err(Invalid::Unretired("upstream_network")),
                check(Some(&previous), &next)
            );
            // the same change is fine once the generation that retires those sockets advances
            next.upstream_generation = 2;
            assert_eq!(Ok(()), check(Some(&previous), &next));
        }
    }

    /// A change in the immutable `LinkProperties` halves has no axis that retires it, so no advance excuses it.
    #[test]
    fn the_address_sets_may_not_change_at_all() {
        let previous = base();
        for (field, mutate) in [
            (
                "virtual_addresses",
                (|c: &mut ShizukuSessionConfig| c.virtual_addresses = vec![vec![192, 0, 2, 6]])
                    as fn(&mut _),
            ),
            ("gateway_addresses", |c: &mut ShizukuSessionConfig| {
                c.gateway_addresses.clear()
            }),
        ] {
            let mut next = ShizukuSessionConfig {
                sequence: 2,
                upstream_generation: 9,
                downstream_epoch: 9,
                ..base()
            };
            mutate(&mut next);
            assert_eq!(
                Err(Invalid::Unretired(field)),
                check(Some(&previous), &next),
                "{field}"
            );
        }
    }

    /// The downstream MTU floor is what queued packets were sized against, so it moves only with the epoch
    /// that retires them.
    ///
    /// Both directions, and both are the same failure: a floor that dropped leaves packets already built too
    /// large for the link, and one that rose leaves the daemon fragmenting for a limit that is gone. Neither
    /// is visible in a config whose epoch stayed still, which is why it is refused rather than absorbed.
    #[test]
    fn the_mtu_floor_moves_only_with_the_downstream_epoch() {
        let previous = base();
        for floor in [1280, 9000] {
            assert_eq!(
                Err(Invalid::Unretired("downstream_mtu_floor")),
                check(
                    Some(&previous),
                    &ShizukuSessionConfig {
                        sequence: 2,
                        downstream_mtu_floor: floor,
                        ..base()
                    }
                ),
                "floor {floor} at an unchanged epoch"
            );
            assert_eq!(
                Ok(()),
                check(
                    Some(&previous),
                    &ShizukuSessionConfig {
                        sequence: 2,
                        downstream_epoch: base().downstream_epoch + 1,
                        downstream_mtu_floor: floor,
                        ..base()
                    }
                ),
                "floor {floor} with the epoch that retires the queue"
            );
        }
        // An epoch that advances on its own is the ordinary shape of every other retirement.
        assert_eq!(
            Ok(()),
            check(
                Some(&previous),
                &ShizukuSessionConfig {
                    sequence: 2,
                    downstream_epoch: base().downstream_epoch + 1,
                    ..base()
                }
            )
        );
        // And so is an admit-only change at both axes.
        assert_eq!(
            Ok(()),
            check(
                Some(&previous),
                &ShizukuSessionConfig {
                    sequence: 2,
                    admit: !base().admit,
                    ..base()
                }
            )
        );
    }
}
