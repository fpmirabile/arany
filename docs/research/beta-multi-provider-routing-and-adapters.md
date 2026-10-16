# Provider capability and routing evidence

**Source baseline:** 2026-09-29; documentation consolidation is not a fresh API or live-account check.

[Provider spec](../specs/provider.md) own admission, supported profiles and
strict outcomes; [Provider spec](../specs/provider.md) own
transport validation. This report retains the source comparison behind exact
profile admission. [The subscription review](./subscription-provider-routes-2026-10.md)
owns the later plan-access investigation. Recheck changing service capabilities
before proposing an additional route.

## Similar wire formats do not establish equal guarantees

The reviewed sources separated four questions: valid JSON, schema adherence,
forced Tool selection and the actual upstream that executes a request.

| Reviewed surface | Evidence at the source baseline | Decision-relevant limitation |
|---|---|---|
| OpenAI Responses | [Structured Outputs](https://developers.openai.com/api/docs/guides/structured-outputs) and [function calling](https://developers.openai.com/api/docs/guides/function-calling) document strict schemas and forced selection. | Capabilities still depend on the exact model and request encoding. |
| Anthropic Messages | [Structured outputs](https://platform.claude.com/docs/en/build-with-claude/structured-outputs) and [Tool choice](https://platform.claude.com/docs/en/agents-and-tools/tool-use/define-tools) document native guarantees and model/thinking restrictions. | The [OpenAI compatibility layer](https://platform.claude.com/docs/en/api/openai-sdk) does not preserve every native parameter or guarantee. |
| OpenRouter | [Structured outputs](https://openrouter.ai/docs/guides/features/structured-outputs) depend on both model and upstream support. | A model slug alone cannot identify the executing route or prove required parameters are honored. |
| Z.AI | The reviewed [structured-output guide](https://docs.z.ai/guides/capabilities/struct-output) described JSON mode and client validation; [function calling](https://docs.z.ai/guides/capabilities/function-calling) described automatic selection. | That evidence did not establish the strict forced outcome Arany required. It is an evidence gap at that date, not a permanent claim about the service. |

**Inference for Arany:** a familiar endpoint shape or a successful model listing
cannot replace exact capability evidence. The semantic Provider seam preserves
Engine policy while adapters own their actual wire contracts. A locally repaired
JSON result is not evidence that the remote route enforced the schema.

## A broker adds a separate boundary

OpenRouter's reviewed [routing documentation](https://openrouter.ai/docs/guides/routing/provider-selection)
allows upstream fallback and parameter-dependent routing. Requiring supported
parameters, selecting an exact upstream and restricting collection/ZDR are
separate controls. A broker's classification does not replace the upstream's
policy, and an unavailable qualifying route cannot justify a weaker fallback.

The reviewed [response-caching documentation](https://openrouter.ai/docs/guides/features/response-caching)
describes replay of stored successful responses, with different inference and
billing provenance from a fresh upstream generation. That explains why route,
cache and privacy evidence cannot collapse into one compatibility flag.
Current broker admission remains in [Provider spec](../specs/provider.md).

## Usage and privacy claims have different owners

A request's storage option, an account's retention agreement and a broker's ZDR
route label answer different questions. The reviewed [OpenAI data controls](https://developers.openai.com/api/docs/guides/your-data),
[Anthropic retention guidance](https://platform.claude.com/docs/en/manage-claude/api-and-data-retention)
and [OpenRouter privacy policy](https://openrouter.ai/privacy) describe different
service boundaries. None makes endpoint authorization equivalent to permission
to disclose arbitrary Workspace data.

Provider-reported usage is evidence only for returned fields. Missing usage is
not zero cost; cancellation or a lost response cannot prove that remote work
never occurred. [Provider spec](../specs/provider.md#usage-provenance-and-reliability)
own current recording and failure behavior.

## Scope of this evidence

The source investigation supports reasons for exact profile conformance, explicit
billing and separate broker provenance. It is not runtime conformance, native
platform evidence or permission to expose a new adapter. Current gaps belong to
[next steps](../../NEXT_STEPS.md), and accepted rationale to
[D-04](./next-step-decision-register.md#d-04--provider-seam-and-profiles).
