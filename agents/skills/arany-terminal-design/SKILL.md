---
name: arany-terminal-design
description: "Design, refine, or review Arany's Rust/Ratatui terminal interface: layout, composer, pickers, status, onboarding, keyboard interaction, and accessible output. Use for CLI visual polish and interaction work, not backend-only changes or HTML reports."
---

# Arany Terminal Design

Improve the user's ability to read, type, choose, and understand ongoing work in Arany. Combine visual hierarchy and accessibility with precise, predictable interaction. Design in terminal cells and semantic states, then implement through the existing Rust presentation and terminal modules.

This skill is self-contained. Its synthesis of `ui-ux-pro-max` and `emil-design-eng` is adapted to Arany; neither upstream skill, its scripts, nor its packages need installation. Read [source provenance](references/sources.md) only when reviewing or updating the synthesis.

## Establish the contract

Resolve repository paths below from this skill's directory, independently of the shell working directory.

- Read [terminal rules](../../../agents/terminal.md) before every design or review. They own modes, keys, layout constraints, drawing, restoration, and accessibility behavior.
- Read [testing rules](../../../agents/testing.md) before implementation or verification claims. Use the existing owner for each claim.
- Follow [AGENTS.md](../../../AGENTS.md) routing for security, CLI, Session, credentials, and architecture when the requested change reaches them.

Inspect the current implementation and affected journey. Existing behavior may have advanced beyond an earlier screenshot or conversation. Identify the actual friction: lost context, unclear selection, excessive chrome, poor scan order, ambiguous status, or difficult recovery.

Separate presentation from behavior changes. A legible locked control is presentation; making it editable during a Run changes the contract. A new command, queue, permission, provider call, or persistence rule needs its own authorized scope. Review requests produce findings; implementation requests proceed within scope.

## Workflow

1. **Observe.** Inspect the frame, input handling, semantic projection, and existing evidence. When useful and available, exercise the journey in a real terminal with isolated state and synthetic data. Never trigger live inference, account setup, or credential changes merely for a screenshot. Label source-only findings accordingly.
2. **Specify.** Name the information the user needs, the next action, affected states, and what retains its place when work completes or a panel closes. A small adjustment needs only a short description. Compare competing layouts with cell-based sketches at the same dimensions and state; label sketches as proposals.
3. **Implement.** Reuse current style and input owners. Keep behavior distinctions explicit. Follow repository planning requirements for changes spanning modules or public interfaces.
4. **Verify.** Check the successful action and the meaningful failure or interruption it could mishandle. Inspect applicable widths and linear mode using the evidence guidance below.

Finish when the requested friction is addressed, relevant contracts are preserved or intentionally updated, and the evidence supports the result. A mockup alone does not demonstrate a working interaction.

## Visual hierarchy in a cell grid

Treat font, palette, background, and zoom as user-owned. Establish hierarchy through spacing, alignment, labels, supported text emphasis, and restrained semantic accents. Evaluate contrast against the actual terminal theme; a hardcoded palette is not accessibility evidence.

Keep conversation and editable input as the primary reading path. Status explains what is happening and the next available action. Disclose competing metadata through existing detail surfaces. Borders and blank rows consume transcript space: use them where they clarify grouping.

Choose information priority before column widths:

| Surface | Keep legible first | Reduce or disclose later |
| --- | --- | --- |
| Composer | Entered text, caret, validation | Argument hints and supporting metadata |
| Status | Current state and required action | Long identifiers and optional metrics |
| Picker | Distinguishable label, selection, eligibility | Descriptions and secondary fields |
| Agent activity | Identity, disposition, attention needed | Verbose objectives and result previews |
| Setup | Current choice, account/provider, recovery | Explanations unrelated to this step |

At smaller sizes, omit lower-priority fields deliberately instead of arbitrary right-edge clipping. Preserve distinguishing portions of similar paths or identifiers, with full detail reachable through an existing supported path.

Navigation selection is not execution status: a highlighted model row becomes the selected model only after activation succeeds. Use accents for interaction roles rather than a different color per module. Essential instructions must remain readable without dim text or color. Pair statuses with words and avoid dependence on special fonts. Include long labels, combining characters, and wide characters in design examples.

## Interaction craft

Polish means that ordinary actions preserve the user's place and produce the expected result. Prefer a coherent default over another configuration option for each style choice.

- **Composer:** distinguish draft, hint, validation, and submitted content. Tie the caret to editable text. Updates elsewhere must not swallow input or move the editing position.
- **Completion:** previews predict actions; they are not executable input. Explain ambiguity and unavailable choices without network work on keystrokes. Use the current registry and parser.
- **Pickers:** expose the focused row and activation rule. Preserve selection by stable identity when data changes where the contract permits. If an item disappears, use and communicate a deterministic fallback.
- **Panel return:** restore the appropriate draft, focus, and reading position. Check completion of the underlying Run while a panel is open. Key hints describe the focused mode.
- **Feedback:** place outcomes near their controls. Distinguish unavailable, pending, failed, and completed. An unavailable choice needs a reason and supported next step; recoverable errors preserve valid input.
- **Busy work:** show progress supported by committed facts. Local selection responds immediately; durable success waits for acknowledgement. Elapsed time is not a completion percentage.

Use established dismissal and cancellation rules. A visual refresh does not authorize rebinding keys or changing whether Enter submits, retains, or activates a choice.

## Translate motion into continuity

The current terminal contract uses event-driven redraws without animation. Translate motion-oriented advice into stable positions, prompt feedback, and clear state transitions. Keep input anchored as activity changes, agent ordering stable, and the user's location intact after closing details.

Do not port CSS transforms, opacity transitions, pixel spacing, hover dependencies, or animation loops. Responsiveness comes from prompt input handling and bounded work before drawing. Check idle behavior too: a quiet interface should not generate unnecessary frames or repeated announcements.

## Rust placement

Follow existing module boundaries rather than adding a UI framework:

- Semantic labels and safe display projections belong with the pure presentation owner.
- Cell layout, Ratatui styles, cursor placement, and viewport behavior belong with the terminal owner.
- Input decoding, composer editing, and command admission stay with their current owners. Reuse state rather than reconstructing it from rendered strings.
- Engine and Session expose facts; terminal geometry and focus stay out of their interfaces and canonical history.

Use existing sanitization and Unicode-width helpers before layout. Derive inner rectangles from available area with guarded arithmetic, including border cells and zero-size regions. Clip on grapheme and cell boundaries, including the cursor window. Draw bounded visible data without reopening files, requesting credentials, calling providers, or replaying full history.

A private style helper earns its place through repeated semantic roles. A single view adjustment rarely needs a new public abstraction, dependency, theme engine, or configuration format. Consult the installed Ratatui version and official documentation when an API detail is uncertain.

## Evidence and review

Select checks that own the changed claim. Authoritative widths, modes, commands, and release gates remain in the linked repository rules.

| Changed claim | Useful evidence |
| --- | --- |
| Layout, truncation, selection | Existing projection/TestBackend cases and visual inspection |
| Submission, completion, locked controls | Existing composer/registry cases; process evidence when routing changes |
| Focus, picker lifetime, input during completion | Existing native PTY journey through action, interruption, and return |
| Linear accessibility or output channels | Existing exact-output fixtures; assistive-technology review for usability claims |
| Terminal ownership or restoration | Existing native lifecycle evidence; screenshots cannot prove cleanup |

Exercise affected states with realistic long labels and Unicode. Check monochrome readability and the smallest supported presentation. Keep screenshots or recordings with synthetic content under `.lavish/`. Browser previews can explain proposals but cannot establish terminal geometry, keyboard behavior, or correctness.

For reviews, use **Observed behavior | Proposed change | User benefit | Evidence**, with a file or observed state per finding. Separate defects from aesthetic preferences. For implementation reports, describe changes and checks actually run; source inspection does not establish live, cross-platform, or screen-reader behavior.
