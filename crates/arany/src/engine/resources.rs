use crate::tools::ToolResourceSettings;

pub(super) fn admit(settings: ToolResourceSettings) -> ToolResourceSettings {
    resolve(
        settings,
        std::thread::available_parallelism()
            .ok()
            .and_then(|cores| cores.get().try_into().ok()),
    )
}

fn resolve(mut settings: ToolResourceSettings, cores: Option<u32>) -> ToolResourceSettings {
    settings.build_jobs = cores.filter(|cores| *cores > 0).and_then(|cores| {
        let requested = settings.build_jobs.unwrap_or(cores);
        (1..=cores)
            .contains(&requested)
            .then_some(requested.min(settings.build_processes))
    });
    settings.build_cpu_cores = cores.filter(|cores| *cores > 0).and_then(|cores| {
        let requested = settings.build_cpu_cores.unwrap_or(cores);
        (1..=cores).contains(&requested).then_some(requested)
    });
    settings
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn goal_jobs_resolve_once_with_positive_core_and_process_bounds() {
        for (cores, requested, processes, expected, cpu) in [
            (Some(8), None, 4, Some(4), Some(8)),
            (Some(8), Some(2), 4, Some(2), Some(2)),
            (Some(8), Some(8), 4, Some(4), Some(8)),
            (Some(1), None, 4, Some(1), Some(1)),
            (None, None, 4, None, None),
            (Some(0), None, 4, None, None),
            (Some(8), Some(0), 4, None, None),
            (Some(8), Some(9), 4, None, None),
            (Some(u32::MAX), None, 4, Some(4), Some(u32::MAX)),
        ] {
            let settings = resolve(
                ToolResourceSettings {
                    build_jobs: requested,
                    build_cpu_cores: requested,
                    build_processes: processes,
                    ..Default::default()
                },
                cores,
            );
            assert!(settings.valid());
            assert_eq!(settings.build_jobs, expected);
            assert_eq!(settings.build_cpu_cores, cpu);
        }
    }
}
