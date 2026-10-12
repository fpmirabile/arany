# Native Guard fixture capture

Load for `src/tools/guard/macos/fixture.rs` and its native journey capture.
[Guard unit](./guard-unit.md) owns process cleanup and native storage;
[testing](./testing.md) owns the evidence budget.

- Enable capture only in debug builds with an explicit `ARANY_TEST_ACCOUNT_ROOT`
  and `ARANY_TEST_GUARD_TRACE` equal to its `guard-trace` child. Admit both existing
  private directories through no-follow handles. Forward those two variables
  only to the trusted native supervisor; worker/payload environments stay clear.
- Keep one fixed record per compiled stage, at most 128 bytes each, with no more
  than 8 KiB aggregate capture including temporary publication files. Repeated
  observations overwrite their stage; this is a lossy snapshot, not a trace log.
- Record `active`, `complete` or `interrupted` and numeric monotonic durations.
  An active stage has unknown elapsed time; the fixture reports its own elapsed
  time separately. Completion requires an explicit successful boundary; Drop
  records interruption. Capture failures cannot change Tool outcomes or budgets.
- Keep records independent of removable unit roots so controller cancellation
  and native cleanup cannot erase the evidence. Never add them to Events,
  Provider context, telemetry or the ordinary development log.
  Publish atomic snapshots without crash-durability synchronization; missing or
  unreadable records remain unknown rather than manufacturing a stage result.
- The parent native journey reads bounded fixed-stage snapshots on helper
  failure or observer timeout. Preserve the 30-second case deadline, 180-second
  parent envelope, exactly-one-helper receipt and existing closed replay cases.
  The case clock includes Tool admission, catalogue work and all unit phases;
  the native preparation/recovery budgets are independent of this observer bound.
