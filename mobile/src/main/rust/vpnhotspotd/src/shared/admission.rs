//! Session-wide descriptor accounting. Leases are refunded only after their owner releases the state.
use std::fmt;

/// Which side of the DNS descriptor floor a request is on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Class {
    /// Ordinary relayed descriptors: UDP mapping sockets, TCP upstream-flow sockets, and family Echo sockets.
    #[default]
    General,
    /// Resolver work that may enter the descriptor floor kept for DNS.
    Reserved,
}

/// Why descriptor admission refused a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Denied {
    Descriptors,
    /// The descriptor counters would wrap.
    Arithmetic,
}

/// One descriptor unit. Not `Clone`, inert, and meaningless outside the [Admission] that issued it.
#[derive(Debug, PartialEq, Eq)]
pub struct Lease {
    admission: u64,
    class: Class,
}

/// Descriptor admission and the outstanding units charged against it.
#[derive(Debug)]
pub struct Admission {
    /// Distinguishes leases issued by this instance from leases issued by any other.
    admission: u64,

    /// Every descriptor this process may hold, derived from `RLIMIT_NOFILE` less what is already open. The
    /// DNS floor is part of this, not subtracted from it.
    descriptor_total: u32,
    /// Descriptors reserved for DNS-class work, within [Admission::descriptor_total].
    dns_descriptor_floor: u32,
    general_descriptors: u32,
    reserved_descriptors: u32,

    peak_descriptors: u32,
    denied: u64,
    /// Releases of a lease issued by another admission, or an impossible counter underflow. Counted rather
    /// than trusted, and never turned into capacity.
    invariant_violations: u64,
}

impl Admission {
    /// Builds the owner from measured descriptor totals.
    pub fn new(totals: Totals) -> Result<Self, Misconfigured> {
        if totals.dns_descriptor_floor > totals.descriptor_total {
            return Err(Misconfigured::DescriptorFloor {
                floor: totals.dns_descriptor_floor,
                total: totals.descriptor_total,
            });
        }
        Ok(Self {
            admission: totals.admission_id,
            descriptor_total: totals.descriptor_total,
            dns_descriptor_floor: totals.dns_descriptor_floor,
            general_descriptors: 0,
            reserved_descriptors: 0,
            peak_descriptors: 0,
            denied: 0,
            invariant_violations: 0,
        })
    }

    /// Every descriptor, including the DNS floor.
    pub fn descriptor_total(&self) -> u32 {
        self.descriptor_total
    }

    /// What general work may reach: the total less the floor DNS keeps inside it.
    fn general_descriptor_ceiling(&self) -> u32 {
        self.descriptor_total
            .saturating_sub(self.dns_descriptor_floor)
    }

    pub(crate) fn descriptors_charged(&self) -> u32 {
        self.general_descriptors + self.reserved_descriptors
    }

    /// Grants exactly one descriptor-capacity unit. A lease can authorize at most one descriptor; DNS may
    /// reserve that unit while receiving a framed query, before resolver submission returns the descriptor.
    pub fn reserve(&mut self, class: Class) -> Result<Lease, Denied> {
        self.check_capacity(class)?;
        match class {
            Class::General => self.general_descriptors += 1,
            Class::Reserved => self.reserved_descriptors += 1,
        }
        self.peak_descriptors = self.peak_descriptors.max(self.descriptors_charged());
        Ok(Lease {
            admission: self.admission,
            class,
        })
    }

    fn check_capacity(&mut self, class: Class) -> Result<(), Denied> {
        let Some(descriptors) = self.descriptors_charged().checked_add(1) else {
            self.denied += 1;
            return Err(Denied::Arithmetic);
        };
        if descriptors > self.descriptor_total {
            self.denied += 1;
            return Err(Denied::Descriptors);
        }
        if class == Class::General {
            let Some(general) = self.general_descriptors.checked_add(1) else {
                self.denied += 1;
                return Err(Denied::Arithmetic);
            };
            if general > self.general_descriptor_ceiling() {
                self.denied += 1;
                return Err(Denied::Descriptors);
            }
        }
        Ok(())
    }

    /// Gives one unit back after its descriptor owner is gone.
    pub fn release(&mut self, lease: Lease) {
        if lease.admission != self.admission {
            self.invariant_violations += 1;
            return;
        }
        let charged = match lease.class {
            Class::General => &mut self.general_descriptors,
            Class::Reserved => &mut self.reserved_descriptors,
        };
        match charged.checked_sub(1) {
            Some(remaining) => *charged = remaining,
            None => self.invariant_violations += 1,
        }
    }

    /// The line a session prints on the way out. Outstanding leases are the leak report.
    pub fn describe(&self) -> String {
        format!(
            "{} of {} descriptors ({} general of {}, {} reserved, floor {}), peak {}; \
             {} leases outstanding; {} denied; {} invariant violations",
            self.descriptors_charged(),
            self.descriptor_total,
            self.general_descriptors,
            self.general_descriptor_ceiling(),
            self.reserved_descriptors,
            self.dns_descriptor_floor,
            self.peak_descriptors,
            self.descriptors_charged(),
            self.denied,
            self.invariant_violations,
        )
    }
}

/// Descriptor totals measured by the platform owner.
#[derive(Debug, Clone, Copy)]
pub struct Totals {
    /// Distinguishes this admission's leases from any other's.
    pub admission_id: u64,
    /// `RLIMIT_NOFILE` less already-open descriptors. The DNS floor is inside this.
    pub descriptor_total: u32,
    /// Reserved DNS descriptor floor within [Totals::descriptor_total]; not a DNS ceiling.
    pub dns_descriptor_floor: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Misconfigured {
    DescriptorFloor { floor: u32, total: u32 },
}

impl fmt::Display for Misconfigured {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DescriptorFloor { floor, total } => {
                write!(
                    f,
                    "a DNS floor of {floor} descriptors is not inside a total of {total}"
                )
            }
        }
    }
}

impl std::error::Error for Misconfigured {}

#[cfg(test)]
mod tests {
    use super::*;

    fn admission() -> Admission {
        Admission::new(Totals {
            admission_id: 1,
            descriptor_total: 10,
            dns_descriptor_floor: 2,
        })
        .expect("valid totals")
    }

    #[test]
    fn general_work_stops_before_the_dns_floor() {
        let mut admission = admission();
        let general = (0..8)
            .map(|_| admission.reserve(Class::General).expect("general ceiling"))
            .collect::<Vec<_>>();
        assert_eq!(admission.reserve(Class::General), Err(Denied::Descriptors));
        let dns = (0..2)
            .map(|_| admission.reserve(Class::Reserved).expect("DNS floor"))
            .collect::<Vec<_>>();
        assert_eq!(admission.descriptors_charged(), 10);
        for lease in general.into_iter().chain(dns) {
            admission.release(lease);
        }
        assert_eq!(admission.descriptors_charged(), 0);
    }

    #[test]
    fn foreign_release_never_invents_capacity() {
        let mut admission = admission();
        let mut other = Admission::new(Totals {
            admission_id: 2,
            descriptor_total: 10,
            dns_descriptor_floor: 2,
        })
        .expect("valid totals");
        let foreign = other.reserve(Class::General).expect("granted");
        admission.release(foreign);
        assert_eq!(admission.invariant_violations, 1);
        assert_eq!(admission.descriptors_charged(), 0);
    }

    #[test]
    fn a_floor_outside_the_total_is_rejected() {
        assert_eq!(
            Admission::new(Totals {
                admission_id: 1,
                descriptor_total: 1,
                dns_descriptor_floor: 2,
            })
            .expect_err("invalid floor"),
            Misconfigured::DescriptorFloor { floor: 2, total: 1 }
        );
    }

    #[test]
    fn a_floor_may_use_the_complete_total_without_inventing_general_capacity() {
        let mut admission = Admission::new(Totals {
            admission_id: 1,
            descriptor_total: 1,
            dns_descriptor_floor: 1,
        })
        .expect("a floor inside the total");
        assert_eq!(admission.general_descriptor_ceiling(), 0);
        assert_eq!(admission.reserve(Class::General), Err(Denied::Descriptors));
        let dns = admission
            .reserve(Class::Reserved)
            .expect("the floor remains available to DNS");
        admission.release(dns);
    }
}
