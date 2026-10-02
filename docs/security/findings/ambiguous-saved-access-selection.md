# Finding: bare startup could prefer API billing over a saved ChatGPT selection

Status: **repaired in the offline Linux beta 1 path**. Severity before repair: **medium, release-blocking when both account types are saved**. No real account or paid call was used to identify or verify this path.

## Evidence and failure path

Previously, the attached [setup owner](../../../src/cli/attached/setup.rs) inspected the native API account first and returned it when present; only then did it inspect the selected ChatGPT account ID. [Bare new Session entry](../../../src/cli/attached.rs) used those defaults without a fresh access-method choice. A user who last completed ChatGPT setup but still had a saved API account therefore started a later bare `arany` Session on the API-key route.

The old `saved_defaults` called `credentials::inspect`, which loaded the full API account before deciding the route. No key disclosure or API request was observed, but this crossed the intended selected-credential boundary.

This contradicted the [beta decision](../../research/next-step-decision-register.md) to keep ChatGPT-plan and API-key billing distinct without a hidden fallback. It was a local selection/cost failure, not a claim that a third party could read the key or that an API request bypassed normal Run admission.

## Resolution and remaining evidence

The user chose a prompt on every bare new start when both access types are saved, not a remembered preference. `StateRoot` now opens both fixed account files through its no-follow, regular-file, owner, link, mode, and size checks and returns only their presence before the CLI presents the existing named-choice UI. Choosing API does not read the ChatGPT index; choosing ChatGPT does not read the native API record. The chosen ChatGPT index is read as a whole and may itself contain multiple private-file tokens, though only the selected account is eligible for use. Cancellation creates no Session or Run. An invalid or unavailable chosen account fails without fallback. Explicit Provider flags and resumed/forked/continued Sessions bypass this startup choice. No preference record, file timestamp inference, Provider request, or paid call was added.

The owning debug product-process PTY seeds both synthetic account records, requires the visible billing-route choice in screen-reader mode, exercises each route and cancellation, strictly replays the pinned Provider/account UUID with no Run, and checks terminal restoration. A separate ignored optimized-binary gate runs both choices in successive product processes under one private passwd-home account root and two Session StateRoots inside no-network Bubblewrap. Each succeeds when the other route's protected record contains malformed JSON, emits no private canary, restores terminal settings, and leaves only its chosen defaults in strict replay. Store tests reject linked and FIFO account files on the presence path. These observations and source review support non-reading of the other billing route's record body, but they do not establish native macOS behavior, full isolated Secret Service setup, a live account, or paid inference. A legacy unpinned native keyring item is not auto-discovered when a ChatGPT index exists; use explicit native setup/inspection to admit it.
