# Arany account setup

Run `arany` in a terminal to set up a new attached Session. If no Provider was selected, Arany looks for its saved default native API account. When none exists, it asks for an access method, Provider, API key, model, and reasoning effort before creating a Session. Use `arany --setup` to replace the default account deliberately. Ctrl+C or Escape cancels without creating a Session or Run.

The API key is entered with terminal echo disabled and saved in the operating system's credential store: Keychain Services on macOS or Secret Service on Linux. Arany does not write it to its SQLite Session history, a profile file, shell arguments, or telemetry. The OS store may prompt to unlock; if it is unavailable, setup fails without a plaintext fallback. On Linux, only a single local Unix D-Bus session address is accepted; executable and remote transports are rejected. This does not mean Arany's separate Session history is encrypted, nor does it guarantee that every Linux Secret Service backend encrypts secrets at rest.

The model picker lists the selected account's visible native models. Only models and efforts for which Arany currently has reviewed adapter support can be selected. Choosing an unsupported row does not send it a Run. One default native API account can be saved at a time; replacing it changes its account ID, so older Sessions pinned to the previous account will not silently use the new key. Start a new Session or select an explicit environment-backed Provider for those Sessions.

For headless `exec`, custom profiles, or attached use with explicit Provider flags, set the selected credential in its named environment variable (`OPENAI_API_KEY` or `ANTHROPIC_API_KEY`) and pass a Provider and model. For example:

```sh
arany exec --provider openai --model gpt-5.4 --output text "Your task"
```

`exec` never prompts or reads the saved default account. `arany provider models openai` also uses the explicit environment key; `/models` inside a Session uses that Session's pinned saved account when present.

The selected saved API account's UUID is recorded with each Run and can be inspected in JSONL history; the API key is never recorded there. Replacing the saved account leaves older Sessions unable to use their previous account selection.

ChatGPT-plan access is shown in setup but is not available yet. It will require an official sign-in flow and a clear warning that local time/read limits cannot guarantee a remote output-token or plan-usage cap. Each verified account must affirmatively accept that weaker bound before use; this product decision is not consent for your account. Selecting it now creates no Session and never falls back to API-key billing. Anthropic consumer subscriptions are not supported.
