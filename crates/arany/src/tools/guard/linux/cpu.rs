use super::{ToolLimits, ToolResourceClass};

const PERIOD_MICROS: u64 = 100_000;
const PERIOD_MILLIS: u64 = PERIOD_MICROS / 1_000;
const DEFAULT_PERCENT: u64 = 25;
const PERCENT_PER_CORE: u64 = 100;

pub(super) fn contract() -> String {
    format!(
        "cpu-period={PERIOD_MICROS};default-percent={DEFAULT_PERCENT};percent-per-core={PERCENT_PER_CORE}"
    )
}

fn percent(limits: &ToolLimits) -> u64 {
    match limits.resource_class {
        ToolResourceClass::Build => u64::from(limits.build_cpu_cores) * PERCENT_PER_CORE,
        ToolResourceClass::Default => DEFAULT_PERCENT,
    }
}

pub(super) fn quota_property(limits: &ToolLimits) -> String {
    format!("--property=CPUQuota={}%", percent(limits))
}

pub(super) fn period_property() -> String {
    format!("--property=CPUQuotaPeriodSec={PERIOD_MILLIS}ms")
}

pub(super) fn maximum(limits: &ToolLimits) -> String {
    format!(
        "{} {PERIOD_MICROS}",
        percent(limits) * PERIOD_MICROS / PERCENT_PER_CORE
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_attestation_and_phase_limits_share_the_pinned_cpu_quota() {
        for (class, cores, property, maximum) in [
            (
                ToolResourceClass::Default,
                1,
                "--property=CPUQuota=25%",
                "25000 100000",
            ),
            (
                ToolResourceClass::Build,
                1,
                "--property=CPUQuota=100%",
                "100000 100000",
            ),
            (
                ToolResourceClass::Build,
                8,
                "--property=CPUQuota=800%",
                "800000 100000",
            ),
            (
                ToolResourceClass::Build,
                u32::MAX,
                "--property=CPUQuota=429496729500%",
                "429496729500000 100000",
            ),
        ] {
            let limits = ToolLimits {
                resource_class: class,
                build_cpu_cores: cores,
                ..Default::default()
            };
            assert_eq!(quota_property(&limits), property);
            assert_eq!(self::maximum(&limits), maximum);
        }
        assert_eq!(period_property(), "--property=CPUQuotaPeriodSec=100ms");
    }
}
