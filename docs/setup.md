# Arany account setup

Run `arany` in a terminal to set up a new attached Session. If no Provider was selected, Arany looks for its saved default native API account. When none exists, it asks for an access method, Provider, API key, model, and reasoning effort before creating a Session. Use `arany --setup` to replace the default account deliberately. Ctrl+C or Escape cancels without creating a Session or Run.

The API key is entered with terminal echo disabled and saved in the operating system's credential store: Keychain Services on macOS or Secret Service on Linux. Arany does not write it to its SQLite Session history, a profile file, shell arguments, or telemetry. The OS store may prompt to unlock; if it is unavailable, setup fails without a plaintext fallback. On Linux, only a single local Unix D-Bus session address is accepted; executable and remote transports are rejected. This does not mean Arany's separate Session history is encrypted, nor does it guarantee that every Linux Secret Service backend encrypts secrets at rest or prevents another application in the same login session from accessing an unlocked item. The [Linux client-isolation finding](./security/findings/linux-secret-service-client-isolation.md) remains open.

Account replacement holds a private, empty StateRoot lock until the OS credential store finishes writing. Another Arany process replacing an account through that same StateRoot causes this setup attempt to report a busy error without writing. The OS-store slot is currently global, so different StateRoots are not yet coordinated; do not treat this as complete cross-process protection. The lock contains no key or token; it does not encrypt secrets in process memory or make a platform credential store infallible.

The setup picker lists every model visible to the selected API account. Reviewed models offer their supported efforts. For another visible model, choose an explicit effort, then separately accept a potentially billable synthetic check: up to three inference calls with at most 3,072 remote output tokens in total, plus bounded input and catalog usage. The check uses no Workspace content. Declining returns to the same model row; cancelling or failing the check saves no key and creates no Session. A canceled request may already have incurred Provider usage. A successful check is bound to the new saved-account UUID and expires within 24 hours; an expired or failed check cannot authorize a later Run. An empty private StateRoot may remain after a failed or canceled check.

In an attached Session, `/models` lists every visible model and can set an unreviewed model ID for the next Run; it cannot make that model runnable by itself. In the inline browser, type to filter model IDs, use Backspace to edit, navigate with arrow keys, and press Enter to select; Escape closes it. Linear/screen-reader mode keeps numbered pages. A separate explicit check can also authorize an exact catalog model and effort for an environment-backed `exec` Run or an existing attached saved-account Run. After checking, choose `/effort LEVEL` in the attached Session; `/model MODEL` is also available if you know the ID. One default native API account can be saved at a time; replacing it changes its account ID, so older Sessions pinned to the previous account will not silently use the new key. Start a new Session or select an explicit environment-backed Provider for those Sessions.

For headless `exec`, custom profiles, or attached use with explicit Provider flags, set the selected credential in its named environment variable (`OPENAI_API_KEY` or `ANTHROPIC_API_KEY`) and pass a Provider and model. For example:

```sh
arany exec --provider openai --model gpt-5.4 --output text "Your task"
```

`exec` never prompts or reads the saved default account. `arany provider models openai` uses the explicit environment key; add `--saved-account` to list the current saved API account instead. `/models` inside a Session uses that Session's pinned saved account when present.

For a model outside the reviewed table, use the same selected environment key for the explicit check and subsequent `exec` Run:

```sh
arany provider check openai MODEL --effort high --accept-cost
arany exec --provider openai --model MODEL --effort high "Your task"
```

After explicit cost consent and selection validation, the check creates a private StateRoot if needed and confirms that the model appears in the selected account's catalog. It can incur up to three inference calls with a total remote output cap of 3,072 tokens, plus bounded catalog and input usage; pricing varies by model. It sends only synthetic text and no Workspace content. Successful evidence lasts at most 24 hours and is tied to the exact Provider, model, effort, API key, and credential source. Listing a model does not check or authorize it. A failed or expired check cannot authorize a Run; a saved account cannot reuse evidence created with an environment key, even if the key text matches. Paid live Provider conformance remains a beta release gate.

To check the current saved API account instead, run `arany provider check openai MODEL --effort high --saved-account --accept-cost`. This creates evidence bound to that account's UUID. In its attached Session, select `/model MODEL` and `/effort high` before submitting work. Arany rechecks the exact account, key, model, effort, and evidence before reading Workspace input. A replacement account needs its own check, even if its key text is unchanged. No paid native check has been run in the beta environment yet; live support remains unverified.

The selected saved API account's UUID is recorded with each Run and can be inspected in JSONL history; the API key is never recorded there. Replacing the saved account leaves older Sessions unable to use their previous account selection.

ChatGPT-plan access is shown in setup but is not available yet. It will require an official sign-in flow and a clear warning that local time/read limits cannot guarantee a remote output-token or plan-usage cap. Each verified account must affirmatively accept that weaker bound before use; this product decision is not consent for your account. Selecting it now creates no Session and never falls back to API-key billing. Anthropic consumer subscriptions are not supported.
