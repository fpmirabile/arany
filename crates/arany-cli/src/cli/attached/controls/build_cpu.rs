use super::build_jobs::cores;
use super::{Preferences, Setting, SettingInput};

fn display(preferences: &Preferences) -> String {
    let requested = preferences.resources.build_cpu_cores;
    let choice = requested.map_or_else(|| "automatic".into(), |cores| cores.to_string());
    match cores() {
        Some(cores) if requested.is_none_or(|value| value <= cores) => {
            format!(
                "{choice}; {} effective (1..{cores})",
                requested.unwrap_or(cores)
            )
        }
        _ => format!("{choice}; Build unavailable"),
    }
}

pub(super) const SETTING: Setting = Setting {
    label: "Build CPU cores",
    hint: "automatic or 1..cores; Linux quota",
    input: SettingInput::Typed,
    value: display,
    apply: |preferences, raw| {
        let raw = if raw == display(preferences) {
            raw.split_once(';')?.0
        } else {
            raw
        };
        let requested = if raw == "automatic" {
            None
        } else {
            let value = raw.parse().ok()?;
            (1..=cores()?).contains(&value).then_some(value)?;
            Some(value)
        };
        preferences.resources.build_cpu_cores = requested;
        Some(())
    },
    cycle: |preferences, forward| {
        let Some(cores) = cores() else {
            return;
        };
        preferences.resources.build_cpu_cores = match preferences.resources.build_cpu_cores {
            None => Some(if forward { 1 } else { cores }),
            Some(value) if forward && value < cores => Some(value + 1),
            Some(value) if !forward && value > 1 => Some(value - 1),
            Some(_) => None,
        };
    },
    reset: |preferences, default| {
        preferences.resources.build_cpu_cores = default.resources.build_cpu_cores
    },
    guidance: None,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_setting_uses_controller_range_independently_of_jobs() {
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
        preferences.resources.build_jobs = Some(1);
        let shown = display(&preferences);
        assert_eq!(shown, format!("{cores}; {cores} effective (1..{cores})"));
        assert!((SETTING.apply)(&mut preferences, &shown).is_some());
        assert!((SETTING.apply)(&mut preferences, "automatic").is_some());
        assert_eq!(preferences.resources.build_cpu_cores, None);
        (SETTING.cycle)(&mut preferences, false);
        assert_eq!(preferences.resources.build_cpu_cores, Some(cores));
        (SETTING.cycle)(&mut preferences, true);
        assert_eq!(preferences.resources.build_cpu_cores, None);
    }
}
