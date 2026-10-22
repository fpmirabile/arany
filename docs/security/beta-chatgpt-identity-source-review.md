# ChatGPT-plan identity source review

Status: **source and signed-synthetic review; a user-reported live identity rejection remains undiagnosed**. Updated on 2026-10-03 against the [OpenAI registration guide](https://developers.openai.com/siwc/token-sharing-open-source/sign-in), [OpenID Connect validation rules](https://openid.net/specs/openid-connect-core-1_0.html#IDTokenValidation) and current implementation. Account registration, storage, renewal, catalog and checked Runs are implemented; this review does not certify live compatibility. A bypass of signature, issuer, client, nonce or selected-subject binding would be critical; none is introduced by the consent controls, diagnostic categories or singleton-audience normalization. This is not an independent AWS-LC cryptographic audit or evidence of isolation from other same-user processes.

## Finding and evidence limits

The user saw browser authorization and a permission email, followed by Arany's generic identity rejection. No user token or account was inspected. A credential-free read of the compiled issuer's public JWKS returned four RSA/RS256 keys with locally admissible sizes, ruling out only a globally unsupported published algorithm in that response. A signed synthetic singleton audience reproduced a local `ClaimsFormat` rejection before normalization; the corrected owner accepts that representation while retaining exact client binding and rejecting untrusted extra audiences. That defect is established, but neither the fixture nor public keys identify the user's failed token. Fixed typed rejection categories distinguish format, key selection/parameters, signature, claims and identity/time bindings without printing sensitive inputs. Final user retry must identify the remaining live category if rejection persists; it is not needed to implement the demonstrated local correction.

The [identity validator](../../crates/arany-cli/src/accounts/chatgpt/identity.rs)
and signed-RSA/JWKS/code-redemption corpora own singleton-audience normalization,
extra/malformed audience rejection and authorized-party binding. The
[ChatGPT contract](../specs/chatgpt.md#authorization-transaction) owns signature,
issuer/client/nonce/time/subject binding and safe diagnostics; the
[credential contract](../specs/credentials.md) owns storage and same-user limits.
These synthetic cases establish local admission, not the cause of the user's
live rejection, cryptographic clearance or native OS-store behavior.

[NEXT_STEPS](../../NEXT_STEPS.md#account-recovery-and-native-guard-follow-ups)
owns the credential-free user retry. Selected-backend lifecycle, live model/effort
checks and real Runs remain owner-run checks there.
