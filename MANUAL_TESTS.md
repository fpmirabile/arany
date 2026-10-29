# Manual tests for feat/coding-loop

Local checklist, not committed. Each step adds the manual checks it needs; delete items as you verify them.

Build once per step: `cargo build --release -p arany-cli` (release, so timings match daily use).

## Step 6: discovery and search

- [x] (Claude, debug build: 27 results, none under `.claude/worktrees` (15,653 files skipped), 760 ms cold / ~200 ms warm, was ~1.6 s) Ask "search the whole repo for `fn main`": no `.claude/worktrees/...` paths in the result, and the search finishes fast. Check timing:
  ```sh
  sqlite3 -readonly "$J" "select json_extract(s.payload,'$.intent.call.operation'), f.created_at_ms - s.created_at_ms from events f join events s on s.kind='ToolStarted' and json_extract(s.payload,'$.intent.id')=json_extract(f.payload,'$.intent.id') where f.kind='ToolFinished' order by f.sequence desc limit 10"
  ```
  `search` should be ~0.3 s or less (was ~1.6 s).
- [x] (Claude: 199 ms with include vs 221 ms whole repo, both warm; both fast. Bonus: luna sent both searches in ONE step, `requested_tools = 2`, so parallel calls work on luna) A search with `include: "crates/arany-cli/src/terminal/**"` is clearly faster than one over the whole repo.
- [x] (Claude: no `Limit`, `may_be_truncated: false`) An unfiltered search over the repo (2,300+ files) returns results instead of `Limit`; if it hits a bound it shows `may_be_truncated` with a `next_offset`.
- [ ] (Claude: Write does create parents, but the model still did 3 mkdir + 1 write because the write description doesn't say so; fixed in step 10b item 14, re-test after) Ask it to create `scratch/a/b/new.txt`: one Write call, no Mkdir.

## Step 9: Cargo in the default class, lean AGENTS.md

- [ ] In a trusted Rust folder with a toolchain selected, ask it to run `cargo check` (no resource class): no "failed to determine the amount of parallelism … Operation not permitted" error.
- [ ] Fresh session in this repo, ask for a small UI tweak: it reads code first and does not read `AGENTS.md`, the Ponytail skill or whole specs up front.

## Step 10a/10b: fixes

Pending from step 9 (needs your Keychain approval for a new binary): the two step 9 checks above.

- [ ] `/settings` (fullscreen) row 16 "Context window": Enter opens a text field, type `500k`, Enter, Enter → saved. ←/→ cycle presets `100% · 75% · 50% · 1m · 500k · 256k · 128k`.
- [ ] `/settings` rows read "Provider calls per task", "Tokens per task", "Active time per task".
- [ ] `explain @crates/arany-cli/src/terminal.rs` (212 KB): Run starts with a notice "… attached partially; the model reads the rest" (no "exceeds its byte limit").
- [ ] `arany exec "hola"` with no flags uses your saved model/effort.
- [ ] Trust screen in a new folder lists runtime profiles (Rust, Node…) with your normal (duplicated) PATH.
- [ ] Re-test step 6: "create `scratch/a/b/new.txt`" → one Write, no Mkdir.
- [ ] An edit task on ChatGPT ends without a refused `answer_invalid` call: `sqlite3 -readonly "$J" "select json_extract(payload,'$.refusal') from events where kind='ProviderCallRecorded' and json_extract(payload,'$.refusal') is not null order by sequence desc limit 5"` shows nothing new.
- [ ] Re-link skills: `npx --yes skills@1.7.0 add ./agents/skills --agent codex claude-code --yes`, then remove old `arany-terminal-design` entries in `.claude/skills/` and `.agents/skills/`.
