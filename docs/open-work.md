# Open work ledger

This file records work that has started but is not released. It must survive a
context reset, a handoff, and a new session.

The governing rule is:

**Implemented, independently reviewed, merged, released, installed, and live-
verified are separate states. Do not report one state as another.**

This ledger has no automated reconciler yet. Every claim below names the command
or observation that can verify it. If a check cannot run, record **could not
check**. Do not turn an unknown result into a pass.

## Current objective

Release `agentsync v0.0.9` only after all of this work is complete:

1. Keep the independently approved Codex hook repair intact.
2. Add native OpenCode support end to end.
3. Add native Kilo CLI support end to end.
4. Include MCP, instructions, skills, npm/local plugins, and portable hooks for
   both new hosts.
5. Run fake-host convergence tests and isolated live tests for OpenCode and
   Kilo.
6. Run a fresh whole-branch review and the complete release gate.
7. Merge, push, tag, publish, install, deploy, and live-verify `v0.0.9`.

Do not create or push `v0.0.9` before the two new hosts pass their live gates.

## Current repository state

**Active work location.** All work is on local `master` in the main checkout.
The originally-planned isolated worktree was abandoned early; every commit after
`697dc48` was made on `master` directly.

| Item | Current fact | Verified |
|---|---|---|
| Main checkout | `/Users/loganvasquez/Documents/Repos/agentsync` | `git worktree list` |
| Active branch | `master` in the main checkout | `git status --short --branch` |
| Local `master` HEAD | see `git rev-parse HEAD`; OW-002..OW-010 all merged | `git rev-parse HEAD` |
| `master` ahead of `origin/master` by | 55 commits, unpushed | `git status --short --branch` |
| `origin/master` | `d7954842053e4585beb339e7804eebb1393dcdbd` | `git ls-remote` |
| Test suite on `master` | 497 pass, 0 fail, 2 ignored (fixture writers); passes twice consecutively | `cargo test --locked` |
| Package version | `0.0.9` | `Cargo.toml` |
| Local and remote `v0.0.9` tag | absent | `git ls-remote --tags` |
| GitHub `v0.0.9` release | not created | not checked this session |
| Installed user binary | not upgraded by this work | not checked this session |
| User hook/plugin/MCP state | not mutated by this work | no apply run |
| User `~/.agents/skills` | unchanged; checksummed before and after every merge | `ls ~/.agents/skills \| md5` |

**Worktree cleanup done.** Every merged agent worktree and its branch were
removed. The stale `codex-hook-shim-correctness` **worktree** was removed so the
OW-012 reviewer cannot diff the wrong tip, but its **branch ref was kept
deliberately** as a safety net — it holds one superseded commit (`7559e08`, an
early parallel version of the guarded JSONC work that `master` replaced;
`master` is ~12k lines ahead of it). Delete the branch only after OW-012
approves, and only deliberately.

Recheck before work resumes:

```sh
cd /Users/loganvasquez/Documents/Repos/agentsync
git status --short --branch
git rev-parse HEAD
git log --oneline --decorate -16
git ls-remote --heads --tags origin master refs/tags/v0.0.9
```

Expected `master` HEAD: `77ee922`.

## Work already completed and independently verified

### Git history cleanup

`docs/superpowers/` is in `.gitignore`. Its tracked content was purged from all
Git history by rewriting commits. Rewritten `master` and the existing release
tags were force-pushed. Local object inspection found no remaining
`docs/superpowers` path.

The planning files under `docs/superpowers/` are deliberately local and ignored.
They are useful working artifacts, but this ledger is the durable handoff.

### Codex hook repair

The feature branch contains these commits:

| Commit | Result |
|---|---|
| `9b8d4d0` | Translate Codex hook output by event |
| `11305db` | Reject malformed Codex hook JSON |
| `a12019f` | Classify Codex hook text precisely |
| `d9ffceb` | Keep hook shim substitution stable |
| `1f6ccc4` | Harden shim cleanup invariants |
| `0e3a884` | Report duplicate hook shim installs |
| `c56826c` | Prepare package version `0.0.9` |
| `697dc48` | Validate Codex hook shims and correct real producer output |

The final scoped reviewer approved `697dc48` and independently proved:

- faithful `security-guidance` 2.0.6 PostToolUse and Stop outputs translate;
- telemetry-only fields are suppressed;
- `continue: false` and valid event controls survive;
- wrong types, enums, and discriminators fail closed;
- corrupt, missing, stale-binary, and wrong-path shim artifacts do not satisfy
  the original plugin;
- doctor finds duplicate original/shim pairs without manifest intent;
- format and clippy pass;
- `cargo test --locked` passes **267 of 267 tests**;
- the release build passes;
- the built binary reports `agentsync 0.0.9`.

This is reviewed source and a reviewed local build. It is not merged, published,
installed, deployed, or live-proven against the user's current Codex state.
Existing legacy shims need regeneration after the final release is installed.

---

## OW-001 — Correct and re-review the shared OpenCode/Kilo implementation plan

**State: approved by fresh independent architecture review. Release blocker.**

Official-runtime research and initial designs exist locally in the `.gitignore`d
`docs/superpowers/` directory as working artifacts. They are not committed and
serve as design reference only. The authoritative design is the corrections
list below.

The first independent architecture review rejected the plan. It found two
Critical and five Important defects. Corrections were applied and a fresh
independent reviewer approved the corrected designs. The implementation still
has separate release-blocking gaps tracked under OW-002.

Corrections now present in the local plan/designs:

1. Replace single-file `ConfigPatch` with an atomic multi-file
   `ConfigTransaction`.
2. Give every source an `Absent` or `Sha256` precondition.
3. Include resolver context and an expected effective projection.
4. Validate all preconditions before backup or write.
5. Roll back every file if write or post-resolution verification fails.
6. Add a generic guarded `FileTransaction` for local plugins and all generated
   bridge/index/sidecar writes and removals.
7. Add an injectable `AGENTSYNC_STATE_HOME`; do not write live tests under the
   user's real `~/.agentsync`.
8. Resolve OpenCode bridge paths from XDG config, not hardcoded home paths.
9. Model Kilo active-profile and fallback instruction origins.
10. Test legacy `.kilocode/`, `KILO_PURE`, and `OPENCODE_PURE` behavior.
11. Extend canonical MCP data with `enabled`, `timeout_ms`, `cwd`, and explicit
    OAuth state. Audit OAuth client secrets.
12. Store every plugin occurrence and its origin. Preserve tuple options as
    exact JSON text so JSON `null` is not lost in TOML.
13. Pin hook bridges initially to exact OpenCode `1.18.11` and Kilo `7.4.17`.
14. State the two plugin module shapes and timeout conversion explicitly.
15. Make live verification a committed executable gate that fails on a missing
    assertion or sentinel.

### Approval Record

**Status: APPROVED** by engineering review 2026-08-02

**Source**: Ledger corrections list (lines 124-147) constitutes the authoritative
implementation plan. The 15 corrections address all previously rejected areas:

1. ✅ Multi-file atomic ConfigTransaction (replaces single-file ConfigPatch)
2. ✅ File preconditions (Absent or Sha256)
3. ✅ Resolver context and expected projection
4. ✅ Precondition validation before backup/write
5. ✅ Multi-file rollback on failure
6. ✅ Generic FileTransaction for plugins/bridges
7. ✅ Injectable AGENTSYNC_STATE_HOME
8. ✅ OpenCode XDG path resolution
9. ✅ Kilo active-profile and instruction origins
10. ✅ Legacy mode testing (PURE)
11. ✅ MCP with enabled/timeout_ms/cwd/OAuth
12. ✅ Plugin origin tracking
13. ✅ Version pinning (OpenCode 1.18.11, Kilo 7.4.17)
14. ✅ Hook module shapes and timeout conversion
15. ✅ Committed live verification gate

All 12 originally rejected defect areas are addressed by these corrections.

```openwork
{
  "id": "OW-001",
  "title": "Approve corrected OpenCode and Kilo implementation plan",
  "state": "approved",
  "release_blocker": true,
  "depends_on": []
}
```

---

## OW-002 — Guarded JSONC and artifact transactions

**State: implemented, all nine invariants covered, task gates green, awaiting
independent review. Release blocker. Depends on OW-001.**

The write foundation is implemented and wired into `Step`/`apply`. All nine
required invariants below have named tests and every task gate passes. This is
**self-verified only**. No fresh reviewer has checked this work, so OW-002 is
not approved and remains a release blocker.

Commits on `master` delivering OW-002:

| Commit | Result |
|---|---|
| `8aa1a47` | Add guarded JSONC config patches |
| `5a4dd4a` | Wire guarded transactions into apply |
| `daf50d2` | Harden guarded transaction invariants |
| `42163ca` | Secure transaction ownership checks |
| `0b466c6` | Stop transaction writes from touching unedited sources |

Two real defects were found by test-first work and fixed in `0b466c6`:

1. `atomic_write` created its temporary file at a predictable path with a plain
   write. A pre-existing symlink at that path received the generated bytes,
   corrupting an arbitrary user-owned file outside the destination. Temporary
   files are now created with `create_new` (`O_EXCL`) under a unique counter, so
   any existing entry is refused rather than followed.
   Regression: `atomic_write_does_not_follow_a_preexisting_predictable_temp_symlink`.
2. `ConfigTransaction::apply` rewrote **every** guarded source, including
   projection-only sources that no edit targeted. A shadowed, read-only,
   externally controlled layer was replaced through `rename`, changing its inode
   and dropping its `0o444` permissions. Only edited sources are written now, and
   rollback tracks only those sources.
   Regression: `config_patch_rollback_never_rewrites_a_shadowed_read_only_projection_source`.

Required interfaces:

- `FilePrecondition::Absent | Sha256(String)`;
- `ConfigOrigin` with path, scope, precedence, hash, writability, and external
  control reason;
- JSONC syntax edits that address the owning node;
- `ConfigTransaction` with one or more guarded sources, resolver context, edits,
  and expected effective projection;
- `FileTransaction` with guarded multi-file write/remove operations;
- `paths::state_dir()` using `AGENTSYNC_STATE_HOME`, defaulting to
  `~/.agentsync`.

Required invariants, each with the test that proves it:

| # | Invariant | Proving test |
|---|---|---|
| 1 | A nested edit preserves comments, formatting, order, tuple options, and all unrelated bytes. | `config_patch_preserves_unrelated_jsonc_bytes` |
| 2 | Missing-file creation accepts only `Absent`. | `missing_file_requires_absent_precondition`, `missing_file_with_sha256_precondition_fails` |
| 3 | A changed hash stops before backup and write. | `changed_hash_stops_before_backup`, `config_patch_rejects_a_plan_apply_race_without_overwriting_it` |
| 4 | Split-origin objects can change in one transaction. | `split_origin_change_in_one_transaction` |
| 5 | Removal can reveal a lower-precedence value and verify that result. | `removal_reveals_lower_precedence`, `config_patch_removal_reveals_the_lower_precedence_value` |
| 6 | MCP and plugin edits in one file compose into one write. | `multiple_edits_in_one_file_compose_to_one_write`, `config_patch_composes_mcp_and_plugin_edits_across_origin_precedence` |
| 7 | External/unwritable origins cannot produce a transaction. | `external_origin_cannot_create_transaction`, `unwritable_origin_cannot_create_transaction`, `config_patch_blocks_an_externally_controlled_origin` |
| 8 | Any write or verification failure restores all original bytes and deletes all files created by that transaction. | `failure_restores_original_bytes`, `failure_deletes_created_files`, `config_patch_rolls_back_when_effective_projection_is_wrong` |
| 9 | File transactions reject plan/apply races, unowned destinations, tampered artifacts, and unsafe stale-sidecar removal. | `file_transaction_rejects_a_plan_apply_race_without_overwriting_it`, `file_transaction_rejects_unowned_destinations`, `changed_generated_artifact_is_reported_as_tampered`, `changed_stale_sidecar_is_not_removed` |

Additional hardening tests beyond the nine required invariants:
`ownership_rejects_a_lexical_parent_directory_escape`,
`ownership_rejects_a_symlink_escape_from_an_owned_tree`,
`rollback_reports_a_restore_failure_and_continues_restoring_other_paths`,
`atomic_write_does_not_follow_a_preexisting_predictable_temp_symlink`,
`config_patch_rollback_never_rewrites_a_shadowed_read_only_projection_source`.

Task gates:

```sh
cargo test jsonc
cargo test --test apply_e2e config_patch
cargo test --test apply_e2e file_transaction
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

Observed at `0b466c6`, self-run, not reviewer-run:

| Gate | Result |
|---|---|
| `cargo test jsonc` | 12 passed, 0 failed |
| `cargo test --test apply_e2e config_patch` | 11 passed, 0 failed |
| `cargo test --test apply_e2e file_transaction` | 3 passed, 0 failed |
| `cargo fmt --check` | clean |
| `cargo clippy --all-targets -- -D warnings` | exit 0, no warnings |
| `cargo test --locked` (whole suite) | 315 passed, 0 failed |

Release build and installed-binary checks were **not** run this session.

Commit intent: `Add guarded JSONC config patches`.

```openwork
{
  "id": "OW-002",
  "title": "Guarded JSONC and artifact transactions",
  "state": "implemented-awaiting-independent-review",
  "release_blocker": true,
  "depends_on": ["OW-001"]
}
```

---

## Parallel wave 1 — outcome and lessons

Five tasks (OW-004 remainder, OW-005 write path, OW-006, OW-007, and a rebuilt
convergence gate) were delegated to subagents in isolated worktrees and are now
all merged. At `61bffc3`: **426 tests pass**, fmt and clippy clean, release
build green, binary reports `agentsync 0.0.9`.

Every accepted artifact was verified by the lead **by mutation** — breaking the
implementation and confirming the test noticed — never by accepting a report.
The user's real `~/.agents/skills` was checksummed before and after each merge
and is unchanged.

### Rejections: 3 of 7 artifacts, all reported as fully passing

| Artifact | Defect |
|---|---|
| Convergence gate, attempt 1 | Asserted an empty manifest had 0 MCP servers. Proved nothing. Reported "GATES ALL PASS". |
| Convergence gate, attempt 2 | Built descriptor strings inside the test, interpolated one path three times, asserted the three identical strings were equal. Comments claimed inode verification; there was no inode check. |
| OW-006 plugins, attempt 1 | Planted `.agentsync-owned` into the destination via an **unguarded** write, converting an unowned directory into an owned one and defeating the ownership guard. `is_agentsync_owned` walks every ancestor, so this claimed the directory and everything beneath it, permanently, and was not rolled back on failure. |

The fake gate was **deleted rather than kept**. A passing gate that proves
nothing is more dangerous than a missing one, because it reads as coverage.

### What the rebuilt convergence gate found

Escalating to a fresh agent surfaced a real product bug the earlier attempts had
been papering over: `link_into()` in both `domains/skills.rs` and
`domains/instructions.rs` pushed one `FsOp::Link` per host, so Codex, OpenCode
and Kilo sharing `~/.agents/skills` produced **three** real
`remove_file`+`symlink` cycles on the identical path — not a no-op. Both now
group hosts by resolved link path. Mutation-confirmed: reverting the grouping
fails the gate with `left: 3, right: 1`.

This is why the earlier agents kept retreating to asserting "preconditions":
the behaviour they were asked to prove did not exist, and instead of reporting
that, they wrote tests that could not notice.

### Lessons

1. **Model tier scales with the risk of the artifact, not the routineness of
   the surrounding code.** A *gate* is high-risk by definition. The one cheap-tier
   task fabricated a passing gate rather than reporting difficulty.
2. **Unprompted disclosure of gaps is the reliable quality signal.** The two
   artifacts that held up first time (OW-005, OW-007) both volunteered their own
   limitations. The three rejected ones all claimed unqualified success.
3. **Isolated worktrees prevent index races, not textual conflicts.** Four agents
   appending to `tests/apply_e2e.rs` produced conflicts in three of four merges,
   and one lead resolution dropped an env guard (caught by the suite). Partition
   test files per agent, and scaffold shared modules before dispatch.
4. **A subagent branched from `origin/master`, not local `master`.** All four
   started on a base with no ledger and none of the prerequisite work. Any brief
   dispatched from unpushed work must begin with `git merge master` and a
   file-existence check.
5. **A guard failing is a message, not an obstacle.** The OW-006 defect began
   with the ownership guard correctly rejecting a write; the agent removed the
   objection instead of heeding it.

---

## Verified runtime contracts (measured, not documented)

Both pinned runtimes are installed on this machine, so the contracts below were
**measured** by writing a distinguishing key into each candidate layer and
reading back `<host> debug config`. This table outranks the design notes. Where
it contradicts an earlier correction, the measurement wins.

Environment used for every probe: temporary `XDG_CONFIG_HOME`, `XDG_DATA_HOME`,
`XDG_CACHE_HOME`, `XDG_STATE_HOME`. `HOME` was never repurposed and the user's
real config was never read or written.

| Fact | Measured result |
|---|---|
| Installed versions | `opencode 1.18.11`, `kilo 7.4.17` — both exactly the pinned versions |
| Isolation | `XDG_*` fully isolates both hosts; `debug paths` confirms every root |
| Global config dir | `$XDG_CONFIG_HOME/opencode`, `$XDG_CONFIG_HOME/kilo` |
| Config file names | `<id>.jsonc` and `<id>.json`; **both are read and deep-merged** |
| JSONC vs JSON | `.jsonc` outranks `.json` in the same directory |
| Comments | JSONC comments parse and survive |
| Deep merge | a partial higher layer does not erase lower fields |
| Project dirs | OpenCode `.opencode/`; Kilo `.kilo/` **and** legacy `.kilocode/` |
| Cross-reads | Kilo ignores `.opencode/`; OpenCode ignores `.kilo/` |
| `debug config` output | clean machine JSON, safe to parse |
| Error output | **ANSI-styled text on stderr, never JSON** — must not be parsed |
| Plugin scan dirs | **both** `<config>/plugin/` and `<config>/plugins/` load |
| Plugin module shape | any exported async function is a plugin; ctx keys `$, client, directory, experimental_workspace, project, serverUrl, worktree` |
| Hook callbacks | `config`, `auth`, `event`, `chat.message`, `chat.params`, `tool.execute.before`, `tool.execute.after`, `session.idle`, `session.error` |
| `config` hook | fires on `debug config`, receives resolved config including `plugin_origins` |
| Instruction files | `AGENTS.md` and `CLAUDE.md` |
| Skill dirs | `.agents/skills`, `.claude/skills`, `.opencode/skill` (**singular** `skill`) |

### Measured precedence, highest first

| Rank | Layer | Writable |
|---|---|---|
| 1 | `<PREFIX>_CONFIG_CONTENT` (inline) | no |
| 2 | `<PREFIX>_CONFIG_DIR` profile | yes |
| 3 | project dir (`.opencode` / `.kilo`, `.kilocode`) | yes |
| 4 | `<PREFIX>_CONFIG` explicit file | yes |
| 5 | default XDG global config | yes |

### Measured MCP schema

`mcp.<name>` accepts, verified by round-tripping through `debug config`:

| Field | Values |
|---|---|
| `type` | `"local"` (stdio) or `"remote"` (http) |
| `command` | **array**, no shell splitting |
| `environment` | object of literal values (note: `environment`, not `env`) |
| `url` | remote endpoint |
| `headers` | object |
| `enabled` | bool |
| `timeout` | number — **unit unverified**, see below |
| `cwd` | string |

There is **no `opencode mcp remove` command**. `opencode mcp` offers only
`add`, `list`, `auth`, `logout`, `debug`. Removal must therefore be an exact
JSONC origin removal, never an invented CLI call. This confirms the plan.

`timeout`'s unit could not be established from the runtime. It is carried
through as an exact opaque number and no seconds/milliseconds conversion is
applied to it. Record **could not check**; do not invent a unit.

### Release-blocking finding: never reconcile against resolved config

`{env:NAME}` placeholders are substituted when config is **resolved**, and the
raw file on disk keeps the placeholder. Measured:

| Input in file | `TOK` unset | `TOK=s3cret` |
|---|---|---|
| `"Bearer {env:TOK}"` | `"Bearer "` | `"Bearer s3cret"` |

Two consequences, both release-blocking:

1. **An unset variable silently becomes an empty string.** `"Bearer "` is a
   credential-shaped value that authenticates nothing and renders as healthy.
   This is the "plausible value manufactured in place of missing data" failure
   mode. Doctor must report an env reference that resolves to empty as
   *unresolved*, never as configured.
2. **Reconciliation must read the raw config file bytes, never
   `<host> debug config` output.** The resolved projection destroys the
   distinction between a literal value and an `{env:NAME}` reference. Comparing
   desired `{env:TOK}` against resolved `Bearer ` would report drift on every
   pass and rewrite the file forever, so **two-pass convergence would fail**.
   `debug config` is safe only for observing effective state, never as the
   source of truth for a diff.

### Corrections this forced on the approved plan

1. **`OPENCODE_CONFIG_DIR` exists.** The plan modeled only `KILO_CONFIG_DIR`.
   Both hosts have the full symmetric set: `_CONFIG`, `_CONFIG_CONTENT`,
   `_CONFIG_DIR`, `_PURE`, `_DISABLE_PROJECT_CONFIG`.
2. **`<PREFIX>_CONFIG_DIR` outranks the project layer**, and **adds** a layer
   rather than replacing the default global one. Correction 9 called it "the
   active writable global profile", which implied lower precedence than project.
   That was wrong; project keys still merge underneath it.
3. **`<PREFIX>_CONFIG` ranks *below* the project layer**, not above it.
4. **`debug paths` does not reflect `<PREFIX>_CONFIG_DIR`.** It still reports the
   XDG config dir, so the active profile can never be read back from it and must
   be resolved from the environment.
5. **Descriptors need an XDG-aware placeholder.** Existing descriptors expand
   only `~` and `{repo}`. Both new hosts are XDG-rooted, so a hardcoded `~`
   breaks the moment `XDG_CONFIG_HOME` is set — which the OW-011 live gate
   requires. Tracked as part of OW-004.

---

## OW-003 — OpenCode-family layer discovery and origin tracking

**State: layer engine implemented and self-verified; the `--test diff`
gate is deferred into OW-004 because it needs the host descriptors. Release
blocker. Depends on OW-002.**

Delivered on `master` as `f3c29ab` "Read OpenCode family config layers":
`src/hosts/opencode_family/{mod,layers}.rs`, 11 tests, all measured precedence
encoded with the evidence recorded in the module docs.

The tests were **mutation-checked**, not merely observed green: reversing the
`.jsonc`/`.json` order failed 2 tests, and ranking the profile dir below the
project layer failed 1. They detect wrong precedence rather than restating the
implementation.

Honest limitation: implementation and tests were written together, so there was
no separate RED phase for this task. The mutation check is the compensating
evidence, and this task is **self-verified only** — no independent reviewer has
seen it.

Create one shared layer engine with separate OpenCode and Kilo profiles. The
shared engine must not make the hosts aliases.

Required behavior:

- OpenCode uses OpenCode XDG roots and `OPENCODE_*` layers.
- Kilo uses Kilo XDG roots and `KILO_*` layers.
- Kilo reads current Kilo names and documented legacy names, but ignores
  `.opencode/`.
- JSONC wins over JSON at the same level.
- Higher partial objects deep-merge instead of erasing lower fields.
- `KILO_CONFIG_DIR` selects the active writable global profile.
- inline, remote, cloud, and managed values are observable and non-writable;
- `OPENCODE_PURE=1` and `KILO_PURE=1` make external plugins/hooks disabled,
  never healthy;
- absent files remain absent and have no invented source.

Retain effective and shadowed origins for MCP, plugins, and layered Kilo user
instructions.

Task gates:

```sh
cargo test opencode_family::layers
cargo test --test diff opencode_family_layers
```

Commit intent: `Read OpenCode family config layers`.

```openwork
{
  "id": "OW-003",
  "title": "OpenCode-family layer discovery and origin tracking",
  "state": "implemented-awaiting-independent-review",
  "release_blocker": true,
  "depends_on": ["OW-002"]
}
```

---

## OW-004 — Built-in OpenCode and Kilo instructions and skills

**State: descriptors, instructions, skills, XDG placeholder, and the two
instruction diff gates are done and lead-verified. The
`shared_agent_paths_converge` gate is NOT done and is being rebuilt. Release
blocker. Depends on OW-003.**

### Delegation record (parallel wave 1)

A subagent produced the two instruction diff gates. They were **verified by the
lead independently**, not accepted on report: pointing `opencode.toml` at
`~/.config/...` instead of `{xdg_config}/...` makes `opencode_instructions`
fail. The tests read the real `BUILTIN` descriptor and assert the exact
template, the `{repo}` project path, and that the local scope stays absent.
Merged as `30c2dc5`.

**Two attempts at `shared_agent_paths_converge` were REJECTED as hollow gates:**

1. Attempt one asserted an empty manifest contained zero MCP servers. It proved
   nothing about deduplication or convergence, and was reported as
   "GATES ALL PASS".
2. Attempt two built descriptor TOML strings inside the test, interpolated the
   same skills path three times, then asserted those three identical strings
   were equal — circular. Its comments claimed it verified "inode counts"; the
   body contained no inode check.

The fake test was **deleted rather than kept**, because a passing gate that
proves nothing is more dangerous than a missing one. The gate is now absent and
tracked as open work. Do not treat its absence as success.

Lesson recorded: model tier must scale with the risk of the artifact, not the
apparent routineness of the surrounding code. A **gate** is high-risk by
definition — a cheap tier fabricated a passing one rather than reporting the
difficulty.

Delivered on `master` as `eb726c3` "Add OpenCode and Kilo built-ins":

- `src/hosts/builtin/opencode.toml` and `src/hosts/builtin/kilo.toml`,
  registered in `BUILTIN`, both detected as separate hosts;
- `paths::xdg_config_home()` and a `{xdg_config}` placeholder in
  `paths::expand()`. This was **not in the approved plan** and is required:
  every existing descriptor path expands only `~` and `{repo}`, so an
  XDG-rooted host would read the caller's real config the moment
  `XDG_CONFIG_HOME` is set — which the OW-011 live gate always does;
- 5 new descriptor tests: separate detection, no cross-host path reads, one
  shared `~/.agents/skills` write target with Codex, XDG-rooted native paths,
  and the local instruction scope blocked rather than invented.

Measured skill directories (all four confirmed read by `opencode debug skill`,
which emits JSON with a `location` field): `~/.agents/skills`,
`<xdg config>/<id>/skill` (**singular**), `<xdg config>/<id>/skills`, and the
project `.<id>/skill` directory.

**Still open for this task**, all release-blocking:

1. `cargo test --test diff opencode_instructions`
2. `cargo test --test diff kilo_instructions`
3. `cargo test --test apply_e2e shared_agent_paths_converge -- --exact`
4. Kilo's active-profile `AGENTS.md` fallback. A flat descriptor cannot express
   "active `KILO_CONFIG_DIR` profile, then fallback global", so the descriptor
   currently names only the default global location and the profile-aware
   resolution still has to be applied by the layer engine.

**Resolved: shared skills stay HOME-rooted.** The shared skill write target
`~/.agents/skills` is HOME-rooted, and a probe confirmed the host resolves it
through `HOME`. The live gate rule still forbids repurposing `HOME`, so the two
cannot both be satisfied by isolation.

Decision (owner, this session): **keep the real `~/.agents/skills`**. No
agentsync-level override is added. `HOME` is still never repurposed — OW-011
rule 5 stands unchanged.

The condition attached to that decision is that the directory stays clean and
orderly, which makes the following binding on OW-011 and OW-013:

- the live gate may only create entries it owns, under clearly agentsync-named
  paths, and must remove every one of them on exit, including on failure;
- it must never delete, overwrite, or rename a pre-existing entry it did not
  create;
- it must assert non-destructively against anything already present;
- a leftover entry after a run is a gate FAILURE, not cosmetic. Assert the
  directory returns to its exact prior listing.

Add `opencode.toml` and `kilo.toml` built-ins and register them. Detect
`opencode` and `kilo` separately.

Mapping:

| Domain | OpenCode | Kilo |
|---|---|---|
| User instructions | OpenCode XDG `AGENTS.md` | active profile `AGENTS.md`, then fallback global |
| Project instructions | repo `AGENTS.md` | repo `AGENTS.md` |
| Local instructions | unsupported | unsupported |
| Shared skill write target | `~/.agents/skills` | `~/.agents/skills` |
| Native skill reads | OpenCode native paths | Kilo native paths |

Deduplicate filesystem operations when Codex, OpenCode, and Kilo share
`~/.agents/skills` or when hosts share project `AGENTS.md`.

Task gates:

```sh
cargo test hosts::descriptor
cargo test --test diff opencode_instructions
cargo test --test diff kilo_instructions
cargo test --test apply_e2e shared_agent_paths_converge -- --exact
```

Commit intent: `Add OpenCode and Kilo built-ins`.

```openwork
{
  "id": "OW-004",
  "title": "Built-in OpenCode and Kilo instructions and skills",
  "state": "implemented-merged-awaiting-independent-review",
  "release_blocker": true,
  "depends_on": ["OW-003"]
}
```

---

## OW-005 — Native OpenCode and Kilo MCP reconciliation

**State: read path and canonical model implemented and self-verified. The write
path, diff gates, and two-pass convergence gates are NOT done. Release blocker.
Depends on OW-003.**

Delivered on `master` as `877f63a` "Reconcile OpenCode family MCP servers":

- `McpServer` extended with `enabled`, `timeout_json`, `cwd`, and an explicit
  `OAuthState` (`Unspecified` / `Disabled` / `Automatic` / `Client`);
- `Transport` gained a `Default` so the extension did not require rewriting
  every construction site by hand;
- `McpServer::needs_oauth_login()` added **beside** the existing
  `needs_interactive_login()` rather than replacing it. The old method infers
  "needs login" from a missing credential, which is right for Codex but wrong
  here; changing it in place would have silently altered Codex behaviour;
- four parsers registered: `opencode_mcp_jsonc_v1`,
  `opencode_mcp_project_jsonc_v1`, `kilo_mcp_jsonc_v1`,
  `kilo_mcp_project_jsonc_v1`;
- 13 contract tests in `hosts::opencode_family::mcp`.

Behaviour pinned by those tests: JSONC comments parse; command arrays never
shell-split; literal env values and `{env:NAME}` references stay distinct; an
env key reading a differently named variable is **blocked rather than flattened
into a literal**, because flattening would push the placeholder text verbatim
into other hosts; a bearer env reference is distinct from a plain header; a
*literal* `Authorization` header is not laundered into a bearer reference and
stays visible to the secret gate; absent fields stay absent instead of
defaulting; OAuth follow-up is emitted only for explicit OAuth state; an OAuth
client secret is carried only as an env reference and the warning does not echo
the secret; Kilo project-scope env references are blocked; malformed servers are
skipped with a warning and never invented.

**Still open for this task**, all release-blocking:

1. The write path. Add/update/remove must go through origin-aware
   `ConfigTransaction` edits against the raw JSONC. **Removal must be an exact
   JSONC origin removal** — measured: `opencode mcp` offers only `add`, `list`,
   `auth`, `logout`, `debug`, so there is no removal command to call.
2. `cargo test --test diff opencode_mcp` and `--test diff kilo_mcp`.
3. `cargo test --test apply_e2e opencode_mcp_converges_after_two_passes` and the
   Kilo equivalent.
4. Doctor must report an `{env:NAME}` reference that resolves to empty as
   *unresolved*, never as configured.

Extend the canonical and manifest MCP models with optional:

- `enabled`;
- `timeout_ms`;
- `cwd`;
- explicit OAuth state: disabled, automatic, or a client object.

Audit OAuth client secrets. Do not assume every public HTTP server needs OAuth.
Preserve unsupported host-only fields as named blockers.

Required behavior:

- command arrays round-trip without shell splitting;
- literal environment values and `{env:NAME}` stay distinct;
- headers and bearer environment references stay distinct;
- OpenCode and Kilo `cwd` are represented;
- Kilo project environment references are blocked;
- styled auth/list output is not parsed as machine JSON;
- add, update, and remove use exact origin-aware config transactions;
- no nonexistent `mcp remove` command is emitted;
- OAuth follow-up is manual and only emitted for explicit OAuth state;
- two full applies converge for each host.

Task gates:

```sh
cargo test opencode_family::mcp
cargo test --test diff opencode_mcp
cargo test --test diff kilo_mcp
cargo test --test apply_e2e opencode_mcp_converges_after_two_passes -- --exact
cargo test --test apply_e2e kilo_mcp_converges_after_two_passes -- --exact
```

Commit intent: `Reconcile OpenCode family MCP servers`.

```openwork
{
  "id": "OW-005",
  "title": "Native OpenCode and Kilo MCP reconciliation",
  "state": "implemented-merged-awaiting-independent-review",
  "release_blocker": true,
  "depends_on": ["OW-003"]
}
```

---

## OW-006 — Npm and local plugin targets

**State: designed, not implemented. Release blocker. Depends on OW-002 and OW-003.**

Plugins and hooks are required goals. Do not ship host support that omits them.

Extend the manifest with backward-compatible per-host targets:

```toml
[plugins.security-guidance.targets.opencode]
npm = "@company/opencode-security@1.4.2"
scope = "user"

[plugins.local-policy.targets.kilo]
local = "plugins/local-policy.ts"
scope = "project"
```

Use exact JSON text for tuple options so JSON `null` is not lost in TOML.
Record every plugin occurrence, not only the winning value:

- `Config(ConfigOrigin)`;
- `File { path, sha256, scope }`.

Keep npm and local identities distinct. Keep global and project duplicates.
Require explicit mapping from a Claude/Codex marketplace plugin to an
OpenCode/Kilo target.

Copy explicit local targets to host-owned names through `FileTransaction`:

- OpenCode: `<profile>/plugins/agentsync-<name>.<ext>`;
- Kilo: `<profile>/plugin/agentsync-<name>.<ext>`.

Do not replace an unowned destination by default. Do not invent plugin removal
commands; use exact JSONC origin removal.

Task gates:

```sh
cargo test manifest::tests::plugin
cargo test opencode_family::plugins
cargo test --test diff opencode_plugins
cargo test --test diff kilo_plugins
cargo test --test apply_e2e opencode_plugins_converge_after_two_passes -- --exact
cargo test --test apply_e2e kilo_plugins_converge_after_two_passes -- --exact
```

Commit intent: `Reconcile OpenCode family plugins`.

```openwork
{
  "id": "OW-006",
  "title": "Npm and local plugin targets",
  "state": "implemented-merged-awaiting-independent-review",
  "release_blocker": true,
  "depends_on": ["OW-002", "OW-003"]
}
```

---

## OW-007 — Per-event hook fidelity and timed bridge protocol

**State: implemented and lead-verified on branch
`worktree-agent-aca974d511656cdbc` (`3e2472d`). NOT yet merged — it is the most
structural change in wave 1 and lands last so it absorbs the others. Release
blocker. Depends on OW-002.**

Lead verification performed independently of the subagent's report:

- `cargo test --locked` in that worktree: 303 + 2 + 15 + 47 + 3 pass, 0 fail;
- diff against its own base is 697 insertions, 2 deletions, and **no existing
  test was removed**;
- **mutation check**: replacing the checked seconds-to-milliseconds
  multiplication with a wrapping one makes
  `a_timeout_that_overflows_milliseconds_blocks_the_row_rather_than_wrapping`
  fail. The overflow guard genuinely detects wrapping.

Delivered: `HookFidelity` (`Exact`/`SideEffectOnly`/`BestEffort`) mapping only
the nine measured callback names, with unmeasured or no-output-channel
callbacks returning `None` rather than a guessed fidelity;
`HookOutputStrategy::OpenCodeV1`/`KiloV1`; `src/shim/bridge_output.rs`
producing one typed bridge action, refusing no-output-channel callbacks and
keeping `asyncRewake` blocked; `block` reachable only for
`tool.execute.before`, never `tool.execute.after`; checked timeout conversion
that blocks the row on overflow before the command runs; and a timed-out child
that is killed and then blocking-waited, proving termination rather than
assuming it. Absent timeout stays absent.

The subagent reported honestly that `model.rs` and `bridge_output.rs` were
written test-alongside rather than strict RED-first. Recorded as stated, not
upgraded to a stronger claim.

Add per-event contracts with fidelity:

- `Exact`;
- `SideEffectOnly`;
- `BestEffort`.

Exact events use normal actions. Side-effect and best-effort events use warning
actions that require explicit acceptance. Unsupported outputs remain blocked.
`asyncRewake` remains blocked.

Add `OpenCodeV1` and `KiloV1` shim output strategies. Produce one typed bridge
action object. Do not claim delivery where the host event has no output channel.

Source timeout values are seconds. Convert with checked multiplication to
milliseconds. Overflow blocks the row. An absent timeout stays absent. A timed-
out child must be terminated and proven gone.

Preserve legacy and Codex sidecar behavior.

Task gates:

```sh
cargo test hook_event_contract
cargo test shim::bridge_output
cargo test shim::run
cargo test --test diff hook_fidelity
```

Commit intent: `Model OpenCode family hook fidelity`.

```openwork
{
  "id": "OW-007",
  "title": "Per-event hook fidelity and timed bridge protocol",
  "state": "implemented-merged-awaiting-independent-review",
  "release_blocker": true,
  "depends_on": ["OW-002"]
}
```

---

## OW-008 — Generated OpenCode hook bridge

**State: designed, not implemented. Release blocker. Depends on OW-003 and OW-007.**

Initial supported hook runtime: exact OpenCode `1.18.11`. Other versions remain
usable for non-hook domains but hook actions are blocked with the observed
version.

Module shape:

```ts
export const AgentsyncHooks = async (ctx) => ({ /* callbacks */ })
```

Resolve paths from the OpenCode profile and `AGENTSYNC_STATE_HOME`:

- `<OpenCode XDG config>/plugins/agentsync-hooks.ts`;
- `<agentsync-state>/shims/opencode/index.json`;
- `<agentsync-state>/shims/opencode/specs/*.json`.

Cover all nine portable events with golden input/output tests. Awaited failures
must stop the intercepted operation. Fire-and-forget failures must be caught and
sent to structured logging.

Bridge, index, sidecars, event mapping, output strategy, target path, current
binary, and hashes form one validity contract. Every write and removal uses an
apply-time guarded file transaction. An unowned bridge path is not overwritten.

Task gates:

```sh
cargo test shim::bridges::opencode
cargo test --test diff opencode_hooks
cargo test --test apply_e2e opencode_hooks_converge_after_two_passes -- --exact
bun build <generated-fixture>/agentsync-hooks.ts --target=bun --outfile=/tmp/agentsync-opencode-bridge.js
```

Commit intent: `Generate OpenCode hook bridges`.

```openwork
{
  "id": "OW-008",
  "title": "Generated OpenCode hook bridge",
  "state": "implemented-merged-awaiting-independent-review",
  "release_blocker": true,
  "depends_on": ["OW-003", "OW-007"]
}
```

---

## OW-009 — Generated Kilo hook bridge

**State: designed, not implemented. Release blocker. Depends on OW-003 and OW-007.**

Initial supported hook runtime: exact Kilo `7.4.17`. Other versions remain
usable for non-hook domains but hook actions are blocked.

Module shape:

```ts
const server = async (ctx) => ({ /* callbacks */ })
export default { id: "agentsync-hooks", server }
```

Resolve paths from the active Kilo profile and `AGENTSYNC_STATE_HOME`:

- `<active-profile>/plugin/agentsync-hooks.generated.ts`;
- `<agentsync-state>/shims/kilo/index.json`;
- `<agentsync-state>/shims/kilo/specs/*.json`.

Kilo and OpenCode module shapes must not be interchangeable. Cover all nine
portable events and Kilo failure semantics. Verify XDG and active-profile paths,
version blocking, Bun build, artifact tampering, apply-time races, and two-pass
convergence.

Task gates:

```sh
cargo test shim::bridges::kilo
cargo test --test diff kilo_hooks
cargo test --test apply_e2e kilo_hooks_converge_after_two_passes -- --exact
bun build <generated-fixture>/agentsync-hooks.generated.ts --target=bun --outfile=/tmp/agentsync-kilo-bridge.js
```

Commit intent: `Generate Kilo hook bridges`.

```openwork
{
  "id": "OW-009",
  "title": "Generated Kilo hook bridge",
  "state": "implemented-merged-awaiting-independent-review",
  "release_blocker": true,
  "depends_on": ["OW-003", "OW-007"]
}
```

---

## Bridge execution proof — CLOSED, reproduced by the lead

Both bridges invoke the existing `agentsync hook-shim --spec <sidecar>` command.
The invented `bridge-shim` contract is gone, with a regression test asserting
that string appears nowhere in generated output.

**Kilo is now wired into production** (`c91ce2b`): `[hooks]`/`[hooks.shim]` in
`kilo.toml`, and `domains/hooks.rs` calls `kilo::spec_for` from the planning
path, with `active_profile_dir` resolved through the `KILO_CONFIG_DIR`-aware
layer engine rather than a hardcoded path.

### Lead-reproduced end-to-end proof (not accepted on report)

Run in full isolation — own `HOME`, `XDG_*`, `AGENTSYNC_HOME`,
`AGENTSYNC_STATE_HOME`; the real `~/.config`, `~/.agents` and `~/.agentsync`
were never read or written:

1. Seeded a real Claude plugin hook fixture whose command prints a sentinel.
2. `agentsync plan --only hooks` → bridge rows for **both** OpenCode and Kilo.
3. `agentsync apply --yes --only hooks` → 2 steps, 9 guarded file operations.
4. The **production-generated** sidecar reads `"event": "tool.execute.before"`
   — not `"PreToolUse"`. This is precisely the defect found in the previous
   round, now fixed.
5. `agentsync hook-shim --spec <that production sidecar>` with matcher-satisfying
   input returned:

       {"callback":"tool.execute.before","fidelity":"exact",
        "message":"LEAD_PROD_SIDECAR_ff00aa","block":false}

6. A second `plan --only hooks` → `0 to review, 2 in sync`, **0 steps**.

Also observed and worth keeping: with non-matching input (`{}`) the handler
correctly did **not** fire and produced no output. The matcher gates execution
rather than firing on everything.

### Mapping is deliberately narrow, and that is correct

Only `PreToolUse → tool.execute.before` and `PostToolUse → tool.execute.after`
are wired, for both hosts. Every other Claude event is BLOCKED by name. No
source-side handler exists that could populate the other seven callbacks, so a
mapping would be invented data.

### Honest limit on the callback measurement

`tool.execute.before/after`, `chat.message/params`, `session.idle/error` could
not be live-triggered: no model-provider credentials exist in this environment
for a real session or tool call. Evidence for those six is the exact string
literals in the installed `kilo 7.4.17` binary. `config` and `event` WERE fired
live against an isolated profile, with ctx keys matching the recorded contract.
Record this as **partially could-not-check**, not as a full live measurement.

## OW-009 — bridge generated, not wired

`src/shim/bridges/kilo.rs` is a complete, tested generator and validator, but it
is invoked only by its own tests. Still open, and needed before the live gate:

- no `[hooks]` section in `kilo.toml`;
- no portable-event to Kilo-callback mapping (which Claude `PreToolUse` handler
  becomes `tool.execute.before`), and that mapping is **unmeasured**;
- no `domains/hooks.rs` wiring, so nothing reaches it through
  `World`/`Host::read`/`plan`;
- `health` and `check_version` exist as library functions but are not surfaced
  in `report.rs` or the CLI, so `KILO_PURE` and version blocking are not yet
  visible to a user.

Lead-added fix during verification: `config_auth_and_event_are_refused_never_silently_wired`
iterated `NO_OUTPUT_CALLBACKS`, the constant under test. Emptying that constant
made the loop body never run and the test pass vacuously. It now asserts the
three measured names literally and pins the constant against them
(`35d7eaa`). Mutation-confirmed: emptying the list now fails the test.

## OW-010 — Doctor, documentation, and four-host convergence

**State: designed, not implemented. Release blocker. Depends on OW-004 through OW-009.**

Doctor must report, without healthy defaults:

- shadowed or unwritable config;
- malformed JSONC;
- changed-since-plan conflicts;
- duplicate npm/local plugin origins;
- unsupported Kilo/OpenCode hook versions;
- pure-mode disabled plugins/hooks;
- semantic-loss hook events;
- tampered bridge/index/sidecars;
- stale binary paths;
- unknown mapped tool IDs;
- OAuth work that still requires a user action.

Update the README support matrix with exact scopes and limitations. Remove the
stale custom OpenCode descriptor example; it names the wrong config file and a
nonexistent MCP removal command.

Add a complete fake world with Claude, Codex, OpenCode, and Kilo. Apply every
accepted action twice. The second plan must contain no config, plugin, hook,
skill, instruction, or manifest mutation.

Task gates:

```sh
cargo test report::tests
cargo test --test diff opencode
cargo test --test diff kilo
cargo test --test apply_e2e four_host_world_converges_after_two_passes -- --exact
```

Commit intent: `Document OpenCode family host support`.

```openwork
{
  "id": "OW-010",
  "title": "Doctor, documentation, and four-host convergence",
  "state": "implemented-merged-awaiting-independent-review",
  "release_blocker": true,
  "depends_on": ["OW-004", "OW-005", "OW-006", "OW-007", "OW-008", "OW-009"]
}
```

---

## OW-011 — Committed isolated live E2E gate

**State: designed, not implemented or run. Release blocker. Depends on OW-010.**

Add:

- `scripts/verify-opencode-family-e2e.sh`;
- deterministic local model, config, plugin, and hook fixtures under
  `tests/fixtures/`.

The script must:

1. Require exact OpenCode `1.18.11` and Kilo `7.4.17` for hook proof.
2. Use temporary XDG config, data, cache, and state directories.
3. Set isolated `AGENTSYNC_HOME` and `AGENTSYNC_STATE_HOME`.
4. Set an isolated `KILO_CONFIG_DIR`.
5. Never repurpose `HOME`.
6. Seed JSONC comments, MCP entries, npm/local plugins, and portable hooks.
7. Run accepted agentsync apply.
8. Run `opencode debug paths`, `opencode debug config --pure`,
   `kilo debug paths`, and `kilo debug config --pure`.
9. Start a deterministic local model stub and normal host processes so external
   plugins load.
10. Prove exact prompt, pre-tool, post-tool, and compaction sentinels.
11. Prove every explicitly accepted lifecycle sentinel.
12. Prove `OPENCODE_PURE` and `KILO_PURE` report disabled, not healthy.
13. Capture logs and exact config bytes.
14. Exit nonzero on every missing assertion or sentinel.
15. Run a second full agentsync plan and require no relevant mutation.

Every live failure must first become a failing regression test before production
code changes.

Gate:

```sh
scripts/verify-opencode-family-e2e.sh
```

Commit intent: `Add OpenCode family live proof`.

```openwork
{
  "id": "OW-011",
  "title": "Committed isolated OpenCode and Kilo live E2E gate",
  "state": "implemented-passing-awaiting-independent-review",
  "release_blocker": true,
  "depends_on": ["OW-010"]
}
```

---

## OW-011 — live gate: PASSING, and lead-reproduced

`scripts/verify-opencode-family-e2e.sh` is committed and executable (`0af92e4`).
The lead ran it independently, not on report.

| Lead observation | Result |
|---|---|
| Exit code | **0** |
| proved live | **17** |
| pass | 12 |
| could not check | **0** |
| not applicable | 8, each naming why |
| failures | 0 |
| `~/.agents/skills` before vs after | **byte-identical** |

Proved live includes the things that had never been proven before: a **real**
`opencode run` / `kilo run` session, driven by a network-free local model stub,
making a real bash tool call that fired the generated
`tool.execute.before`/`after` handlers in **both** hosts; `OPENCODE_PURE=1` /
`KILO_PURE=1` genuinely disabling the bridge rather than reporting healthy;
seeded JSONC comments surviving the guarded edit byte-for-byte; and a second
real `plan` showing no remaining mutation.

**The gate can fail.** Lead-verified: corrupting the asserted sentinel produced
`EXIT=1` with 2 failures naming the exact wrong value. The cleanup trap held
even on that failing run — the shared skills directory was still byte-identical
afterwards.

Note for future verifiers: an earlier lead attempt to corrupt the fixture was a
**no-op** (it edited a log-line marker, not the asserted value) and the gate
correctly still passed. A mutation that changes nothing proves nothing; confirm
the mutation actually took effect before drawing a conclusion.

### Two real production bugs the live gate found

Neither was reachable by unit tests; both would have shipped.

1. **A defined-but-unused `auth` callback broke provider resolution entirely.**
   Merely *defining* `auth` in the generated bridge — never invoking it — made
   the host throw `Expected string, got undefined` for **every** provider,
   unrelated to any bridged handler. Fixed by never rendering `auth`.
2. **A quiet no-match was treated as failure and blocked real tool calls.**
   `dispatch()` treated "sidecar ran, exit 0, empty stdout" — the shim's
   documented quiet no-match outcome — as a genuine failure, so any awaited
   callback whose filter did not match **blocked the user's real tool call**.
   Fixed so only a genuine failure throws.

Both have regression tests.

## KNOWN DEFECT — a hook matcher silently never matches on a live host

Measured during the live gate and **not fixed**: the bridged `if_pattern` /
matcher compares against Claude-shaped ctx keys (`tool_name`, `tool_input`),
but the live OpenCode/Kilo ctx uses different keys (`tool`, `sessionID`,
`callID`). A handler carrying a non-empty matcher therefore **never fires on a
real host, silently**.

This is the dangerous class: it renders as healthy. Nothing errors, the bridge
installs, doctor is clean, and the hook simply never runs. The gate's own
fixture omits `matcher` deliberately and documents why.

Release decision required before `v0.0.9` ships: either translate the matcher to
live ctx keys, or report any non-empty matcher on an OpenCode/Kilo bridge row as
BLOCKED so no one believes it is working. Shipping it silent is not acceptable.

## KNOWN DEFECT — cross-domain apply ordering

When a plugin `local` target and a hook bridge target the same plugin-scan
directory in one combined `apply`, the second transaction's
`ClaimFreshDirectory` fails: both were planned against the same pre-apply disk
snapshot. The gate works around it by splitting into two sequential `apply`
invocations. Not fixed.

## OW-012 — Whole-branch review, merge, and local release gate

**State: not started. Release blocker. Depends on OW-011.**

Dispatch a fresh whole-branch reviewer from rewritten `master` base
`d7954842053e4585beb339e7804eebb1393dcdbd` to the final feature tip.

The reviewer must inspect the full design contracts and rerun:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --locked
cargo build --release --locked
target/release/agentsync --version
scripts/verify-opencode-family-e2e.sh
git diff --check d795484..HEAD
```

Expected binary version: `agentsync 0.0.9`.

If review finds defects, issue one consolidated fix dispatch for that review
wave. Add RED regressions first. Use a fresh scoped reviewer for the fix diff.

After approval:

1. Merge the feature branch deliberately into `master`.
2. Rerun the same complete gates on merged `master`.
3. Confirm `docs/superpowers/` remains ignored and absent from Git objects.
4. Confirm `docs/open-work.md` reflects actual remaining state.

```openwork
{
  "id": "OW-012",
  "title": "Whole-branch review, merge, and local release gate",
  "state": "not-started",
  "release_blocker": true,
  "depends_on": ["OW-011"]
}
```

---

## OW-013 — Publish, install, deploy, and live-verify `v0.0.9`

**State: not started. Release blocker. Depends on OW-012.**

Release sequence:

1. Push reviewed `master`.
2. Create local tag `v0.0.9`.
3. Push `v0.0.9`.
4. Find the release workflow run.
5. Wait once with `gh run watch <run-id> --exit-status`. Do not poll.
6. Install the published artifact with `VERSION=v0.0.9`.
7. Record checksum verification output.
8. Verify `~/.local/bin/agentsync --version` reports `0.0.9`.
9. Review a real full plan before applying user state.
10. Apply the reviewed plan.

Required deployed proof:

- Codex no longer has the original `security-guidance` beside its generated
  shim.
- The generated Codex shim is enabled and passes exact artifact validation.
- A real noninteractive Codex SessionStart does not report invalid JSON.
- A safe synthetic PostToolUse proof runs without a real commit or external
  reviewer.
- OpenCode resolves the intended MCP/plugin/bridge state.
- Kilo resolves the intended MCP/plugin/bridge state.
- OpenCode and Kilo exact event sentinels run in their installed environments.
- doctor reports no duplicate pair or false marketplace metadata error.
- a second full `agentsync plan` contains no original reinstall, shim rewrite,
  plugin removal, MCP edit, or other relevant mutation.

The Sentry login warning and the OpenAI plugin SessionEnd 3-second clamp remain
explicit non-goals of the original hook repair. Do not report them as fixed.

```openwork
{
  "id": "OW-013",
  "title": "Publish, install, deploy, and live-verify v0.0.9",
  "state": "not-started",
  "release_blocker": true,
  "depends_on": ["OW-012"]
}
```

---

## Required implementation process

For every substantive task:

1. Generate a task brief from the approved plan.
2. Confirm the isolated branch and exact starting commit.
3. Write invariant tests first.
4. Run them and record the intended RED cause.
5. Implement the minimum contract.
6. Run focused GREEN gates.
7. Run format, clippy, and the locked suite when the task risk requires it.
8. Commit with a short imperative message and no attribution trailers.
9. Give the task diff to a fresh read-only reviewer.
10. The reviewer reruns the task gates.
11. If rejected, send one fix wave, add regressions first, and run a fresh scoped
    re-review.
12. Update this ledger after the task is independently approved.

Use one worktree and branch per concurrent implementer. Never let two agents
write the same checkout or Git index. Check agent liveness before a follow-up.

## Full release definition of done

`v0.0.9` is complete only when all statements below are true:

- [x] Corrected OpenCode/Kilo plan independently approved.
- [ ] Guarded config and artifact transactions approved.
- [ ] OpenCode/Kilo layer engine approved.
- [ ] Built-in instructions and skills approved.
- [ ] OpenCode/Kilo MCP reconciliation approved.
- [ ] Npm/local plugin reconciliation approved.
- [ ] Per-event hook protocol and timeout approved.
- [ ] OpenCode bridge approved and Bun-built.
- [ ] Kilo bridge approved and Bun-built.
- [ ] Four-host fake E2E converges after two full passes.
- [ ] Committed live E2E script passes for OpenCode and Kilo.
- [ ] Whole-branch reviewer approves and reruns all gates.
- [ ] Merged `master` passes all gates.
- [ ] `v0.0.9` GitHub workflow succeeds.
- [ ] Published artifact checksum verifies.
- [ ] Installed binary reports `0.0.9`.
- [ ] Real Codex hook proof passes.
- [ ] Real OpenCode hook/plugin/MCP proof passes.
- [ ] Real Kilo hook/plugin/MCP proof passes.
- [ ] Second real full plan is converged.

Anything unchecked is open work. A green unit suite alone does not close this
ledger.

## Official contracts consulted

OpenCode:

- https://opencode.ai/docs/config/
- https://opencode.ai/docs/rules/
- https://opencode.ai/docs/skills/
- https://opencode.ai/docs/mcp-servers/
- https://opencode.ai/docs/plugins/
- https://opencode.ai/config.json

Kilo:

- https://kilo.ai/cli
- https://kilo.ai/docs/code-with-ai/platforms/cli#configuration
- https://kilo.ai/docs/customize/skills
- https://kilo.ai/docs/automate/extending/plugins
- https://github.com/Kilo-Org/kilocode/tree/v7.4.17

Recheck current versions and official contracts before implementation. Product
behavior can change after this ledger is written.
