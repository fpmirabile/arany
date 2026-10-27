# Testing fixture details

Load the section named by [testing rules](../testing.md) when its task applies. Those rules remain in force.

## Local verification and CI

Use the standard checks in [the development guide](../../docs/development.md#verification), which owns CI profile selection, changed-path detection and skipped-job behavior.

Manual dispatch offers three inputs. `linux_arm64` runs the `Linux ARM64 Tests` job. `linux_native_advanced` runs the `Native Linux Guard and tmux` job, where each ignored `--exact` invocation must report one completed test. `history_benchmark` runs the `Measure native long-history behavior` step, and its numbers describe that runner only. The [focused workflow](../../.github/workflows/ci-focused.yml) runs one manually chosen `ci.yml` lane; [failure and maintenance discipline](../testing.md#failure-and-maintenance-discipline) governs its use. Linux terminal fixtures need Bubblewrap, util-linux `script`, and user and network namespaces. To reproduce the default suite:

```sh
RUSTFLAGS='--cfg arany_ci' cargo test --workspace --locked --offline
```

Repeat with `--release` for the optimized profile. Run one ignored gate with `cargo test <path> -- --exact --ignored`; the isolated macOS Keychain gate also needs `ARANY_TEST_EXE` set to a built debug `arany`. CI uses no account credentials or paid calls. Compilation or an OS-specific subset does not prove platform parity.

## Release-policy CI

The [release-policy workflow](../../.github/workflows/release-policy.yml) runs on every pull request and every push to `main`, and is never a required check. It builds pinned `cargo-audit` and `cargo-deny` without a cache, fetches the RustSec databases into a new private directory, and fails on a finding, version mismatch, timeout, or more than 64 KiB of output from one command. It checks the lockfile and selected-target policy only; the [dependency inventory](../../docs/security/beta-dependency-inventory.md) owns the other release gates. Check each workflow-expression context against GitHub's [context availability table](https://docs.github.com/en/actions/reference/workflows-and-actions/contexts#context-availability); YAML parsing does not validate it.

## Native and live evidence

- Live runs are ignored, explicitly activated, and assert structure, provenance, persistence, channels, and replay, not prose or model quality. The ChatGPT-plan gate needs `ARANY_LIVE_CONFORMANCE=1` and `ARANY_LIVE_CHATGPT=1`.
- Record host load beside named-host latency gates. A spawn-to-accept bound includes scheduling; a miss under contention stays red until a quiet-host rerun or direct timing isolates the cause, and the threshold is not raised.
- Apply the [admission timestamp ordering](../../docs/specs/execution.md#admission-persistence-and-recovery) when measuring startup. Reject a startup diagnostic when wall-clock and monotonic spans disagree materially; it cannot replace a monotonic gate it straddles.

## Host and account isolation

- Linux containers need a usable systemd user manager, delegated cgroup v2, namespace controls, and an executable temporary filesystem. Configure only the disposable fixture's procfs and cgroup namespaces; never drop production restrictions to pass.
- Remove a verification container, image, volumes, and any build directory written into the checkout when the task ends; Cargo targets reach tens of GiB.
- Keep the host dynamic-loader layout and its full symlink chain in Linux mount fixtures; a missing loader yields an empty PTY transcript.
- A Bubblewrap fixture binding only `/etc/passwd` must make the generated `/etc` traversable before the UID switch, and must give the second UID an accessible synthetic `XDG_STATE_HOME`.
- Offline setup fixtures can serve compiled HTTPS endpoints from a private network namespace with hosts-only resolution and a test CA under ordinary TLS validation. Use `s_server -quiet` with a bounded parser and static reply; `-HTTP` did not answer the Responses POST. This proves synthetic flow, not live models.
- The offline ChatGPT journeys own first sign-in and checked-turn coverage; a matching mock URL or unsigned token is not OAuth evidence.
- On macOS, run private-state fixtures under an admitted physical root: `env TMPDIR=/private/tmp cargo test --locked --offline --workspace`. Do not weaken the `/tmp` and `/var` rejection for fixtures.
- Run macOS liveness-lock and drop corpora in an isolated, exactly-one-test helper. A concurrent fork can hold the same `flock` until exec, so parent drop is not immediate unlock evidence.
- Linux tests that enable write-capable Engine Tools or the account-wide Workspace lock must use the private passwd-home account fixture, following [account-root identity](../../docs/specs/credentials.md#account-scope-and-identity); a `HOME` override alone is insufficient isolation.
- Native macOS Keychain evidence uses the [admitted debug fixture](../../docs/specs/credentials.md#account-scope-and-identity) with a privately named synthetic Keychain, never `login.keychain` and never a search-list change.
- Reset an accepted socket to blocking mode before timeout-bounded reads when its listener is nonblocking; macOS keeps the nonblocking mode on accepted streams.
- Keep a fixture's initial socket peer alive until the helper acknowledges the next stage; Darwin returns `EINVAL` for `SO_RCVTIMEO` on a disconnected Unix socket.
- For local PTY probes use `script`, not a `python3` child: the latter reports root-owned ancestors as UID 65534 and fails private-state admission.
- With GNU `timeout` supervising an interactive `script`, use `--foreground`, and keep a separate bounded parent wait with descendant cleanup.
- A full first-run setup PTY must not write the fixed default OS credential slot of a live Secret Service or Keychain. Isolate the native service and account, since a private State directory alone does not.
- Follow the [debug account-root admission contract](../../docs/specs/credentials.md#account-scope-and-identity): set absolute synthetic `HOME`, `XDG_STATE_HOME` and `XDG_DATA_HOME` beneath the fixture root's parent; a stable-root override alone is insufficient isolation. Isolate account and Session roots before opening either. Prove the no-leaf control reaches the intended boundary first, or an early rejection can pass a failure test. Test optimized account behavior with a private passwd home and a no-network mount namespace.
- Native Linux Guard fixtures keep the host user-manager socket, cgroup hierarchy, and `/tmp` seed paths. Their private namespace uses passwordless `sudo` only to create it, mount the synthetic passwd file, and raise loopback with `/usr/sbin/ip`; the helper then drops to the original UID and GID with cleared groups and environment. `iproute2` is a required prerequisite. Serialize with `NATIVE_GATE` and retain the fixture after a failed capture, because reaping `sudo` does not prove the helper exited.
- The no-account composer uses a private default State root and an admitted, test-owned unavailable credential-bus address. It proves local rejection and setup cancellation, not keyring behavior.
- Answer or dismiss [folder consent](../../docs/specs/tools.md#folder-trust-and-approvals) in every attached PTY fixture, including explicit Provider or `--tools` cases. Isolate account and environment roots as well as Session storage.
- Snapshot-refusal fixtures use admitted executable identities and assert the actual capture disposition. A denied system executable or missing interpreter proves only no-snapshot rejection; use a pinned synthetic actor when payload execution must be guaranteed.

## Terminal process fixtures

- `script` is not a terminal emulator. For [presentation acquisition](../../docs/specs/terminal.md#terminal-continuity-and-provisional-text), give Classic Inline fixtures an explicit bounded cursor peer or exercise the specified linear fallback. Never fabricate coordinates. Gate on a completed frame or actual settings. A Ctrl+C wrapper handles its own SIGINT and leaves the product's handling intact.
- Direct macOS PTYs need a child-owned session and controlling terminal with close-on-exec descriptors. Normalize only Darwin's transient `PENDIN` input state on restored canonical input, per the [native tty owner](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/tty.c); compare every other flag, control character, and speed exactly.
- Clear fresh fullscreen backends directly: Ratatui's `Terminal::clear` queries the cursor through stdout and can time out on a piped stdout.
- Before `SIGCONT`, observe both the stopped status and the mouse-disable sequence, then assert exact mode counts and restored settings. Declare PTY handles after the child guard so unwind closes masters before blocking reap. A deadline fault must positively establish the reap; timeout is not disappearance.
- In raw-mode PTY tests send carriage return for Enter; line feed is a distinct event and can leave a draft unsubmitted.
- A test that kills its `script` wrapper must separately guard the product PID: verify its exact executable (including the exact Linux ` (deleted)` path after replacement) and private State argument before killing it.
- `script` forwards stdin asynchronously. Before signaling during hidden canonical input, wait for output confirming the synthetic segment was consumed.
- To prove a signal during [Classic output](../../docs/specs/terminal.md#terminal-continuity-and-provisional-text), pause the `script` stdout reader on the answer header, verify raw settings, then signal and drain.
- Match synthetic peers to the selected [preview contract](../../docs/specs/provider.md#streaming-and-provisional-text): use SSE (`send_response_for(..., true)`) for attached Classic and fullscreen Runs, and the ordinary admitted encoding for other calls.
- For broken-stderr proof, keep input on one PTY and stderr on a second. Close only the second master after the Provider-start gate, then trigger a redraw. Drain controlling-input echo too, since undrained echo can stall Darwin exit. Retain a master for settings observation: a retained slave prevents EOF, and a disconnected slave returns `ENOTTY`, which is loss evidence, not restoration proof.
- A dead stderr sink cannot prove delivery of a mouse-disable sequence.
- Restored-output signal tests need bounded writer backpressure; a marker may otherwise be read only after reacquisition.
- Start a held peer's cancellation deadline only after the renderer fault triggers. A bare SIGWINCH with unchanged geometry does not fire the bounded reader's resize event; use an admitted draft edit or resize the PTY.
- For terminal-owner panic proof, run the exported owner in a supervised helper under a PTY, hold it at a private gate, compare exact settings before, during, and after a deliberate unwind, and observe the panic and one failed child test. This verifies RAII cleanup, not an arbitrary CLI panic site.
- For a broken-stderr consent fault, probe the slave nonblocking until `EIO` under a bound, then trigger a real redraw. Wait for readiness and settings markers on their separate pipes.
- Include disconnected-stderr cleanup from consent as well as the composer when proving [terminal restoration](../../docs/specs/terminal.md#interruption-suspension-and-restoration); composer-only tests miss Ratatui's destructor diagnostic panicking before raw-mode restoration.
- For picker mouse proof, send SGR hover, wheel, and click bytes only after the inline picker's `EnableMouseCapture`. Assert one matching disable before selection returns, then verify the chosen Session by exact-ID replay.
- SGR coordinates are absolute screen positions; inline frames can start below row zero, so use the rendered frame position.
- In canonical-line tests, wait for the complete prompt or labeled feedback before sending input. Declare reply and signal sequences per case and require every action to be consumed. Gate on the answer and the next prompt before quitting.
- Keep PTY input open until startup authorization feedback and the chat prompt appear. EOF cancels authorization and can win before a local rejection.
- In inline panel PTYs, wait for the finished frame and footer before navigating, and scope markers to bytes after the current input. Wait for the restored composer after Escape. Ratatui can place cursor controls between footer words, so a contiguous text search is not a completion gate.
- Gate a known-geometry draft edit on cursor-show followed by its fresh caret position. Gate Classic on the caret column, a composer row, or an emulated screen, since its viewport moves as transcript is inserted. Send Escape alone and wait for the restored frame before typing a suffix, since adjacent bytes can encode an Alt key.
- In a 16-column model browser, the model ID and close notice clip. Wait for the complete `Up/Dn Enter Esc` footer before controlling it.
- For native tmux proof, use an exact test-owned `-S` socket under a private root and a detached fixed-size pane for capture; send keys through a supervised outer PTY client. After a resize, wait for the frame at the new size before sending history keys. Verify pane and outer settings, and query the exact socket after shutdown; a lingering socket path does not prove a live server.
- In long-lived tmux journeys, find the product only in a bounded descendant walk from the exact pane PID, and check its executable and private State argument before retaining or signaling it. A captured pane can keep its last frame after exit, so frame readiness does not prove liveness.
- When a tmux status predicate gate times out, rule out predicate drift against the spec's status priority before calling it latency.

## Storage, Workspace and resource faults

- A server-side half-write marker proves bytes were sent, not consumed. Pair the mid-response RSS sample with the product's peak, an accepted exact-cap response, and strict replay.
- For Linux Store file-growth faults, open the journal before applying `RLIMIT_FSIZE` in an isolated helper with `SIGXFSZ` ignored; otherwise the limit kills the test process. Compare the accepted prefix and the absent failed Event after a read-only reopen, then exit the helper before proving recovery.
- For the Linux Store transaction-death gate, hold the owner after its terminal insert but before commit, or after append acknowledgement. Kill and reap the exact child at each stage, reopen the SQLite file, and compare replay, sequence, integrity, and recovery. Wait for the complete stage value, since a marker file can exist before its bytes. This proves the application boundary, not power-loss timing.
- For shipped `exec` file-growth faults, finish data-free custom conformance and commit a Session before launching the limited child. Ignore `SIGXFSZ` only there, then compare exact channels, absent runtime Provider requests, read-only replay, and a post-fault append.
- For real Linux `ENOSPC`, mount a bounded `tmpfs` only inside a supervised child user and mount namespace, and fail visibly if admission is unavailable. Chroot the child before opening State; do not weaken StateRoot admission. Require `StorageFull`, SQLite `DiskFull`, strict read-only replay, integrity after capacity returns, and a successful append. This is not block-device or power-loss evidence.
- For a shipped `exec` in that root, bind the binary, its loader and libraries, the host CA bundle read-only, and `/dev/null`. Reqwest loads the system trust store even for numeric-loopback HTTP, so a missing bundle fails before the fault. Keep a PID namespace around descendants and prove Store open succeeds at zero free blocks.
- For a nonblocking Workspace-input gate, use a real FIFO with no writer in a supervised child under a parent deadline; a Unix socket's rejection does not prove the open cannot hang. Confirm no Provider call or canonical Event. See [Linux `fifo(7)`](https://man7.org/linux/man-pages/man7/fifo.7.html).
- For the Linux Workspace replacement gate, atomically exchange each selected component with an outside-pointing symlink using [`renameat2(2)`](https://man7.org/linux/man-pages/man2/renameat2.2.html). Prove the safe and rejected stable states, then require real swap and load overlap with no outside bytes across finite attempts; keep it ignored and explicitly timed.
- For paused Tokio deadline tests involving the SQLite owner thread, pause after the Provider-start gate and resume before awaiting durable shutdown; otherwise virtual time can outrun the real store thread.

## Native Guard fixtures

- The Linux runtime-topology fixture runs its outer test owner in a private PID namespace with `--kill-child`. The outer fixture keeps the host procfs view so each process's own `/proc/self/status` reports host PIDs in its [`NSpid` field](https://man7.org/linux/man-pages/man5/proc_pid_status.5.html); the production Bubblewrap profile still mounts its own `/proc`.
- A separate forced-timeout gate records the namespace init and held-descendant PIDs from those reports, binds both pidfds before killing and reaping the unshare owner, then awaits both exits before dropping the fixture directory. The topology path gates the init and each exact-filter worker until its host identity is bound and confirms its pidfd exit after capture on success and failure. Keep the fixture directory if any identity or exit stays unproved; assert only after confirmed exit.
- Publish PID records and release gates atomically so pollers observe either nothing or a complete record.
- Run captured Rust helpers with a full exact test filter and prove that exactly one test completed.
- Require positive helper exit before reading a captured helper's completed-test summary; control-socket EOF can precede the runner's final output.
