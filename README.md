# agentsync

Keep your MCP servers, skills, and plugins in sync across agentic coding CLIs.

If you use more than one of Claude Code, Codex, and friends, your configuration
drifts. You add an MCP server in one and forget the other. `claude mcp add`
defaults to `--scope local`, so half your servers end up pinned to whichever repo
you happened to be in. A skill exists in one tool's directory and not the other.
Tokens end up sitting in plaintext in a JSON file.

`agentsync` shows you a to-do list of those differences and lets you resolve them
one keypress at a time.

```
agentsync     12 to review   ·   4 accepted
hosts: claude ●  codex ●
 MCP SERVERS
 ▸ ✓ atlassian_rovo        only in codex                  adopt + add to the others
   ✓ kicad                 only in claude                 adopt + add to the others
     pulumi                only in claude, 1 repo          adopt + make global
   ! tradingview           defined at 2 scopes on claude   adopt + make global (drops the duplicates)
   ! upskillai-knowledge   credential in the clear         adopt with the token moved to $UPSKILLAI_KNOWLEDGE_TOKEN
 SKILLS
   ✓ sentry-workflow       only in codex                   adopt + link into the others
 PLUGINS
     superpowers           only in claude                  adopt + install in the others
     everything-evenhub    no provider on codex            —
─────────────────────────────────────────────────────────────────────────────────────
 atlassian_rovo  —  http · https://mcp.atlassian.com/v1/mcp
 e cycles to: adopt only, don't push  ·  keep codex-only  ·  delete everywhere
 space accept   e change   A accept section   d delete   v show synced   r rescan   ⏎ run
```

## Install

```sh
cargo install --path .
```

## Use

```sh
agentsync              # the review TUI
agentsync plan         # print the differences and the plan the defaults would produce
agentsync apply --yes  # accept every default and run it
agentsync doctor       # problems that aren't differences: secrets, unset vars, dead paths
agentsync hosts        # what agentsync knows about each CLI
```

Rows that are already in sync are hidden — press `v` to see them. Nothing is
modified until you press `⏎`, review the exact commands, and confirm with `y`.
`c` on that screen writes the plan out as a shell script if you would rather run
it yourself.

## How it works

**Read from config files, write through each CLI.** Host config files are parsed
directly, because that is complete and instant and reveals per-repo scopes the
CLIs hide behind the working directory. But every *change* is made by invoking the
host's own CLI, because those files hold state that is none of our business —
Codex's project trust levels, notice flags, model preferences — and a tool that
takes ownership of the whole file destroys it.

**A canonical manifest, adopted from either side.** `~/.config/agentsync/manifest.toml`
records what you decided should exist. Symlink it into your dotfiles to version
it. You can adopt *into* it from any host, which matches how this actually goes:
you install something ad-hoc, then decide to keep it.

**Divergence is recorded, not nagged about.** Some things genuinely belong to one
tool. `hosts = ["codex"]` on an entry says so, and it stops being reported. That
is why the list converges to empty instead of training you to ignore it.

**Capabilities are enforced, not assumed.** `codex mcp add` has no `--header`, so
a server carrying custom HTTP headers is reported as *blocked* for Codex rather
than pushed with the headers silently dropped. Every skipped host prints why.

**Secrets are names, never values.** The manifest holds `bearer_token_env`,
`env_from`, and `${VAR}` references. A value that looks like a live credential is
rejected on save — a hard gate, not a lint, because the failure mode is a token in
git history. When agentsync finds a literal token in a host's config it offers to
move it to an environment variable rather than copying it.

**The run reports itself as it goes.** Executing a plan streams progress: a
spinner on the step in flight, a running count, and completed steps marked as they
land. Some steps are genuinely slow — a plugin install clones a repository — and a
UI that only repaints when everything is finished is indistinguishable from a
hang. Keystrokes during a run are discarded rather than queued, so nothing you
typed while waiting fires the moment the review screen returns.

**Failures don't abort the run.** Steps run in order; a failed step is recorded
and the run continues, then the summary says what was done, what failed with the
host's own stderr, and what was skipped. Stopping halfway would leave you unable
to tell which half landed. The manifest is written once at the end, and only if
every manifest edit succeeded.

**Nothing destructive happens without a backup.** Replacing a real directory with
a symlink, moving host-owned skill content into canonical storage, and deleting
canonical content all copy to `~/.config/agentsync/backups/` first. An "unlink"
action refuses to delete a real directory.

## Scopes

MCP servers exist at more than one scope, and the same name at two scopes is a
bug — one silently wins.

| Scope | Claude Code | Codex |
|---|---|---|
| `user` | `~/.claude.json` → `mcpServers` | `~/.codex/config.toml` → `[mcp_servers]` |
| `local` — this machine, one repo | `~/.claude.json` → `projects["<path>"].mcpServers` | — |
| `project` — committed, shared | `<repo>/.mcp.json` | `<repo>/.mcp.json` |

agentsync shows every scope in one list rather than making you switch between
them, collapses a name that appears in several repos into one row, and defaults a
shadowed entry to a single global definition. `>` promote / demote is expressed as
the `adopt + make global` action, which removes the per-repo copies before the
global one lands so the name is never at two scopes at once.

## Adding another CLI

A host is a TOML descriptor. Drop one in `~/.config/agentsync/hosts/` — no
recompile, and it overrides a built-in of the same name.

```toml
name = "opencode"
display = "OpenCode"
detect = { bin = "opencode" }

[mcp]
scopes = ["user"]
caps = ["stdio", "http", "env", "bearer_env"]

[[mcp.read]]
file = "~/.config/opencode/config.json"
parser = "claude_json_v1"        # reuse a compiled parser

[mcp.add]
style = "flags"
argv_stdio = ["mcp", "add", "{name}", "{env_flags...}", "--", "{command}", "{args...}"]
argv_http  = ["mcp", "add", "{name}", "--url", "{url}", "{bearer_flags...}"]
env_flag = "--env"
env_format = "{key}={value}"
bearer_env_flag = "--bearer-token-env-var"

[mcp.remove]
argv = ["mcp", "remove", "{name}"]

[skills]
dirs = ["~/.config/opencode/skills"]   # dirs[0] is the link target
```

Config *parsing* stays compiled — `parser = "..."` names one of a handful of
shapes, listed by `agentsync hosts --parsers`. Config formats are too irregular
to describe in data, but a new host usually reuses an existing parser. Everything
else — paths, argv, flag spellings, scopes, capabilities — lives in the file.

`caps` is what makes this safe: declare only what the CLI can actually express,
and agentsync will refuse to push anything it can't, instead of dropping the part
that doesn't fit.

Template vocabulary: `{key}` substitutes a scalar anywhere in an argument (an
unknown key is an error, never an empty string), and an argument that is exactly
`{key...}` splices a list and may expand to nothing.

## Manifest

```toml
[mcp.kicad]
transport = "stdio"
command = "node"                     # bare, so it resolves on another machine
args = ["~/repos/kicad-mcp/dist/index.js"]
env = { LOG_LEVEL = "info" }
env_from = ["KICAD_PYTHON"]          # forwarded by name; the value is never stored

[mcp.knowledge]
transport = "http"
url = "https://api.example.com/mcp"
bearer_token_env = "KNOWLEDGE_TOKEN"

[mcp.pulumi]
transport = "stdio"
command = "pulumi"
args = ["mcp", "start"]
scope = "project"
repos = ["/Users/me/repos/infra"]

[mcp.unityMCP]
transport = "http"
url = "http://127.0.0.1:8080/mcp"
hosts = ["codex"]                    # intentional divergence, recorded

[skills.code-review]
source = "skills/code-review"        # relative to the manifest

[marketplaces.claude-plugins-official]
github = "anthropics/claude-plugins-official"
hosts = ["claude"]

[plugins.superpowers]                # no marketplace: resolved per host
```

Plugin ids are derived rather than declared, because the curated registries
genuinely differ between hosts — `superpowers` is in `claude-plugins-official` on
Claude Code and three different marketplaces on Codex, and plenty of plugins are
in only one. Neither CLI resolves a bare id (`codex plugin add superpowers` exits
1 with "requires --marketplace unless passed as `<plugin>@<marketplace>`"), so
agentsync reads each host's marketplace manifests and resolves the id per host.
A name no marketplace offers is *not available*, not drift. A name several
marketplaces offer asks you to pin one rather than guessing.

A per-repo `.agentsync.toml` is merged in for that repo, forced to project scope.
It never shadows the user manifest silently — a name collision is reported.

## Layout

```
tui/        review + run screens
core/       model, differ, planner, applier — no host knowledge
domains/    mcp, skills, plugins
hosts/      descriptor loader, parser registry, CLI runner
manifest/   canonical file + secret gate
```

`core` takes a manifest and a list of host snapshots and emits rows and a plan; it
has never heard of Claude Code or Codex. That is what keeps hosts pluggable and
the differ testable without a machine to test against.

## Development

```sh
cargo test                  # unit + integration
cargo clippy --all-targets -- -D warnings
cargo fmt
```

`tests/diff.rs` builds synthetic worlds and asserts on row wording and planner
output. `tests/apply_e2e.rs` stands up a fake host CLI on a temporary `PATH` — it
records the argv it receives and maintains a real config file — so the write path
is verified without touching the machine running the tests, including that a
second pass converges.

## Status

Early. Verified against Claude Code 2.1.220 and Codex CLI 0.146.0 on macOS.

Two things are worth knowing:

- Codex's project-scoped `.mcp.json` support is inferred from its loader, not
  confirmed; its descriptor declares `scopes = ["user"]` until it is. Claude
  Code's three scopes are confirmed against `claude mcp add --help`.
- `agentsync doctor --fix` for rewriting a literal secret out of a host's own
  config file is not implemented. Today, moving a token to an environment
  variable rewrites the manifest and re-pushes the corrected definition; the old
  literal remains in the backup.

## License

MIT
