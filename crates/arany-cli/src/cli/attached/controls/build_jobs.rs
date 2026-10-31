use super::{Preferences, Setting, SettingInput};

pub(super) fn cores() -> Option<u32> {
    std::thread::available_parallelism()
        .ok()?
        .get()
        .try_into()
        .ok()
}

fn display(preferences: &Preferences) -> String {
    let requested = preferences.resources.build_jobs;
    let choice = requested.map_or_else(|| "automatic".into(), |jobs| jobs.to_string());
    let Some(cores) = cores() else {
        return format!("{choice}; Build unavailable");
    };
    if requested.is_some_and(|jobs| jobs > cores) {
        return format!("{choice}; Build unavailable (cores {cores})");
    }
    let jobs = requested
        .unwrap_or(cores)
        .min(cores)
        .min(preferences.resources.build_processes);
    format!(
        "{choice}; {jobs} effective (cores {cores}, process limit {})",
        preferences.resources.build_processes
    )
}

pub(super) const SETTING: Setting = Setting {
    label: "Build jobs",
    hint: "automatic or 1..cores",
    input: SettingInput::Typed,
    value: display,
    apply: |preferences, raw| {
        let raw = if raw == display(preferences) {
            raw.split_once(';')?.0
        } else {
            raw
        };
        preferences.resources.build_jobs = if raw == "automatic" {
            None
        } else {
            let jobs = raw.parse().ok()?;
            (1..=cores()?).contains(&jobs).then_some(jobs)?;
            Some(jobs)
        };
        Some(())
    },
    cycle: |preferences, forward| {
        let Some(cores) = cores() else {
            return;
        };
        preferences.resources.build_jobs = match preferences.resources.build_jobs {
            None => Some(if forward { 1 } else { cores }),
            Some(jobs) if forward && jobs < cores => Some(jobs + 1),
            Some(jobs) if !forward && jobs > 1 => Some(jobs - 1),
            Some(_) => None,
        };
    },
    reset: |preferences, default| preferences.resources.build_jobs = default.resources.build_jobs,
    guidance: None,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jobs_settings_validate_cores_and_show_process_narrowing() {
        let mut preferences = Preferences::default();
        let cores = cores().unwrap();
        for raw in [
            "0".into(),
            (u64::from(cores) + 1).to_string(),
            "invalid".into(),
        ] {
            let prior = preferences;
            assert!((SETTING.apply)(&mut preferences, &raw).is_none());
            assert_eq!(preferences, prior);
        }
        assert!((SETTING.apply)(&mut preferences, &cores.to_string()).is_some());
        preferences.resources.build_processes = 1;
        assert!(display(&preferences).contains("1 effective"));
        assert!((SETTING.apply)(&mut preferences, "automatic").is_some());
        assert_eq!(preferences.resources.build_jobs, None);
        assert!(display(&preferences).starts_with("automatic; 1 effective"));
    }
}
