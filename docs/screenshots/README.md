# Regenerating the screenshots

The images in the main README are real runs of the real TUI, captured from a pty
and rendered to SVG. Only the *config being read* is a fixture, so the screenshots
stay stable and cover the interesting states instead of whatever happens to be on
one machine.

```sh
cargo build --release
bash docs/screenshots/shots.sh          # writes docs/screenshots/shots/*.svg
cp docs/screenshots/shots/*.svg docs/screens/
```

| File | What it does |
|---|---|
| `setup_demo.sh` | Builds `/tmp/agentsync-demo`: a fake `$HOME`, two fake host CLIs on a private `PATH`, host configs, marketplace catalogs, skills, two repos, and a manifest. Every path in the UI then contracts to `~/...`. |
| `capture.py` | Runs the binary in a pty of a fixed size, sends keystrokes, dumps the raw escape stream. |
| `ansi2svg.py` | Replays that stream onto a cell grid — cursor moves, erases, SGR colour and attribute runs, including the xterm 256-colour palette — and emits an SVG. |
| `shots.sh` | One capture per view, each against a freshly built demo world. |

The fake host CLIs are not stubs that always succeed: they record their argv and
actually maintain their own config file, so the run screen shows real work and a
second pass converges. They also sleep briefly, which is what makes the streaming
progress screen observable.

Two things worth knowing if you touch this:

- `capture.py` takes the binary path from `AS_BIN` rather than expanding `~`,
  because `HOME` is overridden for the demo world and `~` would resolve into the
  fixture.
- The fixtures' `PATH` is deliberately narrow, so anything the fake CLIs shell out
  to needs an absolute path.
