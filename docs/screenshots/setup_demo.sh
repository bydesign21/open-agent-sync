#!/bin/bash
# Build a self-contained demo world for the README screenshots.
#
# These are real runs of the real TUI: it reads real config files and invokes real
# CLIs. Only the *contents* are fixtures, so the screenshots are stable and cover
# the interesting states instead of whatever happens to be on one machine.
set -euo pipefail

# A fake $HOME, so every path in the UI contracts to ~/... like it would
# on a real machine instead of showing a temp directory.
D=/tmp/agentsync-demo
rm -rf "$D"
mkdir -p "$D"/{bin,.config/agentsync/hosts,.claude/skills,.agents/skills,.claude/marketplaces/official/.claude-plugin,.claude/marketplaces/adhd/.claude-plugin,repos/infra,repos/webapp}
mkdir -p "$D/.claude" "$D/.codex"

PY=$(command -v python3)

# ---- fake host CLIs: log, mutate their own config, and take a beat so the
# ---- progress screen is observable
for host in claude codex; do
  cat > "$D/bin/$host" <<EOF
#!$PY
import json, os, re, sys, time
CFG = "$D/.${host}/config.toml"
LOG = "$D/calls.log"
argv = sys.argv[1:]
open(LOG, "a").write("$host " + " ".join(argv) + "\n")
time.sleep(1.1)

def load():
    try:
        return open(CFG).read()
    except FileNotFoundError:
        return ""

def without(name, text):
    return re.sub(r"\[mcp_servers\." + re.escape(name) + r"\][^\[]*", "", text)

if argv[:2] == ["mcp", "add"] or argv[:2] == ["mcp", "add-json"]:
    name = argv[2]
    lines = ["[mcp_servers." + name + "]"]
    if argv[1] == "add-json":
        d = json.loads(argv[3])
        if d.get("url"):
            lines.append('url = "' + d["url"] + '"')
        else:
            lines.append('command = "' + d.get("command", "x") + '"')
    else:
        rest = argv[3:]
        if "--url" in rest:
            lines.append('url = "' + rest[rest.index("--url") + 1] + '"')
        if "--" in rest:
            lines.append('command = "' + rest[rest.index("--") + 1] + '"')
    text = without(name, load()) + "\n".join(lines) + "\n\n"
    open(CFG, "w").write(text)
elif argv[:2] == ["mcp", "remove"]:
    text = without(argv[2], load())
    open(CFG, "w").write(text)
sys.exit(0)
EOF
  chmod +x "$D/bin/$host"
done

# ---- host config: claude has more, codex has less
cat > "$D/.claude/config.toml" <<'EOF'
[mcp_servers.kicad]
command = "node"
args = ["/Users/you/repos/kicad-mcp/dist/index.js"]

[mcp_servers.tradingview]
command = "bun"
args = ["/Users/you/repos/tradingview-mcp/src/server.js"]

[mcp_servers.sentry]
url = "https://mcp.sentry.dev/mcp"

[mcp_servers.upskillai-knowledge]
url = "https://api.example.com/platform/knowledge/mcp"

[mcp_servers.upskillai-knowledge.http_headers]
Authorization = "Bearer f0a6ab6e38d15c1541fe6ed1e67ba64c78ccbe02505d358d58c30020aa5340ae"

[plugins."superpowers@claude-plugins-official"]
enabled = true

[plugins."hookify@claude-plugins-official"]
enabled = true

[plugins."i-have-adhd@i-have-adhd"]
enabled = true

[marketplaces.claude-plugins-official]
source_type = "git"
source = "https://github.com/anthropics/claude-plugins-official.git"

[marketplaces.i-have-adhd]
source_type = "local"
source = "MARKETS/adhd"
EOF
sed -i '' "s|MARKETS|~/.claude/marketplaces|" "$D/.claude/config.toml"

cat > "$D/.codex/config.toml" <<'EOF'
[mcp_servers.atlassian_rovo]
url = "https://mcp.atlassian.com/v1/mcp"

[mcp_servers.unityMCP]
url = "http://127.0.0.1:8080/mcp"

[mcp_servers.sentry]
url = "https://mcp.sentry.dev/mcp"

[plugins."atlassian-rovo@openai-curated"]
enabled = true
EOF

# ---- marketplace catalogs
cat > "$D/.claude/marketplaces/official/.claude-plugin/marketplace.json" <<'EOF'
{ "name": "claude-plugins-official",
  "plugins": [ {"name":"superpowers"}, {"name":"hookify"}, {"name":"commit-commands"} ] }
EOF
cat > "$D/.claude/marketplaces/adhd/.claude-plugin/marketplace.json" <<'EOF'
{ "name": "i-have-adhd", "plugins": [ {"name":"i-have-adhd"} ] }
EOF

# ---- skills: codex owns two real dirs, claude links one of them elsewhere
for s in code-review fix-ci; do
  mkdir -p "$D/.agents/skills/$s"
  printf -- "---\nname: %s\ndescription: demo\n---\n" "$s" > "$D/.agents/skills/$s/SKILL.md"
done
mkdir -p "$D/.claude/skills"
ln -sfn "$D/.agents/skills/code-review" "$D/.claude/skills/code-review"
mkdir -p "$D/.claude/skills/release-notes"
printf -- "---\nname: release-notes\ndescription: demo\n---\n" > "$D/.claude/skills/release-notes/SKILL.md"

# ---- a repo with its own project-scoped server
cat > "$D/repos/infra/.mcp.json" <<'EOF'
{ "mcpServers": { "pulumi": { "type": "http", "url": "https://mcp.ai.pulumi.com/mcp" } } }
EOF

cat > "$D/repos/webapp/.mcp.json" <<'EOF'
{ "mcpServers": { "playwright": { "command": "npx", "args": ["-y", "@playwright/mcp"] } } }
EOF

# ---- host descriptors overriding the built-ins, pointed at the fixtures
write_descriptor() {
  local name=$1 skills=$2 markets=$3
  cat > "$D/.config/agentsync/hosts/$name.toml" <<EOF
name = "$name"
display = "$name"
detect = { bin = "$name" }

[mcp]
scopes = ["user", "project"]
caps = ["stdio", "http", "env", "bearer_env"$4]

[[mcp.read]]
file = "~/.$name/config.toml"
parser = "codex_toml_v1"

[[mcp.read]]
file = "{repo}/.mcp.json"
parser = "mcp_json_v1"
scope = "project"

[mcp.add]
style = "flags"
argv_stdio = ["mcp", "add", "{name}", "--", "{command}", "{args...}"]
argv_http = ["mcp", "add", "{name}", "--url", "{url}", "{bearer_flags...}"]
bearer_env_flag = "--bearer-token-env-var"
env_flag = "--env"
env_format = "{key}={value}"
header_flag = "--header"
header_format = "{key}: {value}"

[mcp.remove]
argv = ["mcp", "remove", "{name}"]

[skills]
dirs = ["$skills"]

[plugins]
implicit_marketplaces = ["openai-curated"]

[[plugins.read]]
file = "~/.$name/config.toml"
parser = "codex_plugins_toml_v1"

[[plugins.catalog]]
glob = "$markets"
parser = "marketplace_manifest_v1"

[plugins.install]
argv = ["plugin", "add", "{id}"]

[plugins.remove]
argv = ["plugin", "remove", "{id}"]

[plugins.marketplace_add]
argv = ["plugin", "marketplace", "add", "{source}"]

[plugins.marketplace_remove]
argv = ["plugin", "marketplace", "remove", "{name}"]

$5
EOF
}
# claude can express headers; codex cannot — the real capability gap.
# The same split shows up in hooks: claude can express `if` and the rewake
# fields, codex cannot. Only claude gets a hooks.read source, because only
# claude's fixture tree has a hooks.json to find.
CLAUDE_HOOKS=$(cat <<'EOF'
[hooks]
events = ["SessionStart", "UserPromptSubmit", "PreToolUse", "PostToolUse",
          "Stop", "SubagentStop", "PreCompact", "Notification", "SessionEnd"]
caps   = ["matcher", "if", "timeout", "async_rewake",
          "rewake_message", "rewake_summary"]
output = ["hook_specific_output", "system_message", "additional_context",
          "suppress_output", "rewake_message", "rewake_summary"]

[[hooks.read]]
glob   = "~/.claude/plugins/cache/*/*/*/hooks/hooks.json"
parser = "claude_hooks_json_v1"

[[hooks.read]]
file   = "~/.claude/settings.json"
parser = "claude_settings_hooks_v1"

[[hooks.read]]
file   = "~/.claude/settings.local.json"
parser = "claude_settings_hooks_v1"

[hooks.shim]
marketplace = "~/.agentsync/shims/claude"
EOF
)
CODEX_HOOKS=$(cat <<'EOF'
[hooks]
events = ["SessionStart", "UserPromptSubmit", "PreToolUse", "PostToolUse",
          "Stop", "SessionEnd"]
caps   = ["matcher", "timeout", "async_rewake"]
output = ["hook_specific_output", "system_message", "additional_context",
          "suppress_output"]

[hooks.shim]
marketplace = "~/.agentsync/shims/codex"
EOF
)
write_descriptor claude "~/.claude/skills" "~/.claude/marketplaces/*/.claude-plugin/marketplace.json" ', "headers"' "$CLAUDE_HOOKS"
write_descriptor codex  "~/.agents/skills" "~/.claude/marketplaces/official/.claude-plugin/marketplace.json" '' "$CODEX_HOOKS"

# ---- manifest: enough to produce missing / differs / synced / divergent rows
cat > "$D/.config/agentsync/manifest.toml" <<EOF
[mcp.sentry]
transport = "http"
url = "https://mcp.sentry.dev/mcp"

[mcp.kicad]
transport = "stdio"
command = "node"
args = ["/Users/you/repos/kicad-mcp/dist/index.js"]

[mcp.linear]
transport = "http"
url = "https://mcp.linear.app/mcp"

[mcp.unityMCP]
transport = "http"
url = "http://127.0.0.1:8080/mcp"
hosts = ["codex"]

[mcp.pulumi]
transport = "http"
url = "https://mcp.ai.pulumi.com/sse"
scope = "project"
repos = ["$D/repos/infra"]

[plugins.superpowers]

[marketplaces.claude-plugins-official]
github = "anthropics/claude-plugins-official"
EOF

# ---- hooks: a plugin manifest for hookify, at the real cache layout claude
# ---- reads. It shows the two gaps this domain exists to report:
# ---- - three PostToolUse handlers with a byte-identical command, told apart
# ----   only by `if`. Codex hashes the command alone, so it cannot tell them
# ----   apart. Codex can host a shim, so this renders as a shimmed gap.
# ---- - a PreCompact handler. Codex has no PreCompact event, so this renders
# ----   as blocked.
HOOKS_DIR="$D/.claude/plugins/cache/claude-plugins-official/hookify/1.0.0/hooks"
mkdir -p "$HOOKS_DIR"
cat > "$HOOKS_DIR/hooks.json" <<'EOF'
{
  "description": "hookify — turns conversation mistakes into hooks that prevent them",
  "hooks": {
    "PostToolUse": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "bash \"${CLAUDE_PLUGIN_ROOT}/hooks/check-rule.sh\"",
            "if": "Bash(git commit:*)",
            "asyncRewake": true,
            "rewakeMessage": "A hookify rule flagged this commit. Address or acknowledge the findings below, then continue.",
            "rewakeSummary": "Commit rule check found issues"
          },
          {
            "type": "command",
            "command": "bash \"${CLAUDE_PLUGIN_ROOT}/hooks/check-rule.sh\"",
            "if": "Bash(git push:*)",
            "asyncRewake": true,
            "rewakeMessage": "A hookify rule flagged this push. Address or acknowledge the findings below, then continue.",
            "rewakeSummary": "Push rule check found issues"
          },
          {
            "type": "command",
            "command": "bash \"${CLAUDE_PLUGIN_ROOT}/hooks/check-rule.sh\"",
            "if": "Bash(gt submit:*)",
            "asyncRewake": true,
            "rewakeMessage": "A hookify rule flagged this submit. Address or acknowledge the findings below, then continue.",
            "rewakeSummary": "Submit rule check found issues"
          }
        ],
        "matcher": "Bash"
      }
    ],
    "PreCompact": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "bash \"${CLAUDE_PLUGIN_ROOT}/hooks/save-rules-context.sh\""
          }
        ]
      }
    ]
  }
}
EOF

echo "$D"
