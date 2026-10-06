# Local development diagnostics

Run the development executable normally with `cargo run --locked`. After admitted chat or `exec` startup, `development.log` appears beneath the Session State directory: an explicit `--state-dir`, otherwise the platform default (on Linux, `$XDG_STATE_HOME/arany` or `$HOME/.local/state/arany`). An actual submitted task can consume Provider usage. Optimized `--release` builds keep this log disabled.

Look for the new process's `DiagnosticsEnabled` marker, then closed failure categories and numeric stream/argument-shape counters. Records contain bounded local stack frames, not upstream responses, task text or credentials. A new record diagnoses an observed execution; older journals cannot reconstruct this detail.

The log is private, lossy and bounded, separate from canonical Session history. See the [diagnostic contract](./specs/diagnostics.md) for activation, privacy, retention, causes and acceptance scenarios, and [next steps](../NEXT_STEPS.md) for verification limits and unresolved checks.
