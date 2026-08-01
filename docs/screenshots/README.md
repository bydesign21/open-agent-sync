# Regenerating the screenshots

The images in the main README are real runs of the real TUI, captured from a pty
and rendered to SVG. Only the *config being read* is a fixture. This keeps the
screenshots stable, and lets them cover interesting states instead of whatever
is on one machine.

```sh
cargo build --release
bash docs/screenshots/shots.sh          # writes docs/screenshots/shots/*.svg
cp docs/screenshots/shots/*.svg docs/screens/
```

| File | What it does |
|---|---|
| `setup_demo.sh` | Builds `/tmp/agentsync-demo`. This includes a fake `$HOME`, two fake host CLIs on a private `PATH`, host configs, marketplace catalogs, skills, two repos, and a manifest. Every path in the UI then contracts to `~/...`. |
| `capture.py` | Runs the binary in a pty of a fixed size, sends keystrokes, dumps the raw escape stream. |
| `ansi2svg.py` | Replays that stream onto a cell grid — cursor moves, erases, SGR color and attribute runs, including the xterm 256-color palette — and emits an SVG. |
| `shots.sh` | One capture per view, each against a freshly built demo world. |

The fake host CLIs are not stubs that always succeed. They record their argv,
and they maintain their own config file. As a result, the run screen shows real
work, and a second pass converges. They also sleep briefly. This is what makes
the streaming progress screen observable.

Two things worth knowing if you touch this:

- `capture.py` takes the binary path from `AS_BIN` instead of expanding `~`.
  `HOME` is overridden for the demo world, so `~` resolves into the fixture
  instead.
- The fixtures' `PATH` is deliberately narrow, so anything the fake CLIs shell
  out to needs an absolute path.
