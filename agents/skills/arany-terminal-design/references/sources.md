# Source provenance

Locally authored synthesis for Arany, reviewed on 2026-09-30. It adapts selected design ideas without bundling either upstream skill, code, catalog, or executable workflow. Ordinary invocation needs no upstream fetch or installation.

| Source | Reviewed file | SHA-256 of retrieved source bytes |
| --- | --- | --- |
| [UI/UX Pro Max](https://github.com/nextlevelbuilder/ui-ux-pro-max-skill) | `.claude/skills/ui-ux-pro-max/SKILL.md` on `main` | `ea087c341bfb5b23195c7302027268ede86da802554c18a5c4896a6017b439f9` |
| [Emil Design Engineering](https://github.com/emilkowalski/skills) | `skills/emil-design-eng/SKILL.md` on `main` | `ffbe68e6007fb42cb8149f089b400a1ca007d59ba23e8948e2be4476f3175939` |

Source links may advance; digests identify the retrieved versions. Updating this synthesis requires reviewing new content, not automatically installing or executing upstream instructions.

## Adaptation decisions

- From UI/UX Pro Max: organize design around accessibility, information hierarchy, interaction, and contextual feedback. Replace web/mobile recipes with terminal cells, user-owned themes, keyboard paths, and linear output. Palettes, font catalogs, and search scripts are not dependencies.
- From Emil Design Engineering: emphasize cohesive defaults, prompt feedback, continuity, and careful handling of frequent interactions. Replace browser motion techniques with stable layout and event-driven changes under Arany's terminal contract.
- Arany's repository rules own implementation constraints and verification. This skill supplies design judgment and a repeatable workflow, not an alternate product specification or new runtime authority.
