#!/usr/bin/env python3
"""Replay a captured escape stream onto a cell grid and emit an SVG.

Handles the subset ratatui/crossterm actually emits: absolute cursor moves,
erase-display, erase-line, and SGR colour/attribute runs.

Usage: ansi2svg.py IN.raw ROWS COLS OUT.svg [TITLE]
"""
import html
import re
import sys

IN, ROWS, COLS, OUT = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), sys.argv[4]
TITLE = sys.argv[5] if len(sys.argv) > 5 else ""

# A dark palette close to a default terminal theme.
BG = "#12141a"
FG = "#d6dae2"
PALETTE = {
    30: "#3b4048", 31: "#e06c75", 32: "#98c379", 33: "#e5c07b",
    34: "#61afef", 35: "#c678dd", 36: "#56b6c2", 37: "#abb2bf",
    90: "#5c6370", 91: "#e06c75", 92: "#98c379", 93: "#e5c07b",
    94: "#61afef", 95: "#c678dd", 96: "#56b6c2", 97: "#ffffff",
}


def indexed(idx):
    """The xterm 256-colour palette: 16 base, a 6x6x6 cube, then a grey ramp."""
    if idx < 8:
        return PALETTE[30 + idx]
    if idx < 16:
        return PALETTE[90 + (idx - 8)]
    if idx < 232:
        idx -= 16
        levels = (0, 95, 135, 175, 215, 255)
        return "#%02x%02x%02x" % (
            levels[idx // 36],
            levels[(idx % 36) // 6],
            levels[idx % 6],
        )
    grey = 8 + (idx - 232) * 10
    return "#%02x%02x%02x" % (grey, grey, grey)


class Cell:
    __slots__ = ("ch", "fg", "bg", "bold", "dim", "rev")

    def __init__(self):
        self.ch = " "
        self.fg = None
        self.bg = None
        self.bold = False
        self.dim = False
        self.rev = False

    def key(self):
        return (self.fg, self.bg, self.bold, self.dim, self.rev)


def blank():
    return [[Cell() for _ in range(COLS)] for _ in range(ROWS)]


grid = blank()
cy = cx = 0
cur = Cell()  # current pen

text = open(IN, "rb").read().decode("utf-8", "replace")
i = 0
while i < len(text):
    ch = text[i]
    if ch == "\x1b":
        m = re.match(r"\x1b\[([0-9;?]*)([A-Za-z])", text[i:])
        if not m:
            i += 1
            continue
        params, cmd = m.group(1), m.group(2)
        nums = [int(p) for p in params.split(";") if p.isdigit()]
        if cmd == "H":
            cy = (nums[0] - 1) if nums else 0
            cx = (nums[1] - 1) if len(nums) > 1 else 0
        elif cmd == "J":
            grid = blank()
        elif cmd == "K":
            for x in range(cx, COLS):
                grid[cy][x] = Cell()
        elif cmd == "m":
            if not nums:
                nums = [0]
            n = 0
            while n < len(nums):
                v = nums[n]
                if v == 0:
                    cur = Cell()
                elif v == 1:
                    cur.bold = True
                elif v == 2:
                    cur.dim = True
                elif v == 7:
                    cur.rev = True
                elif v == 22:
                    cur.bold = cur.dim = False
                elif v == 27:
                    cur.rev = False
                elif v == 39:
                    cur.fg = None
                elif v == 49:
                    cur.bg = None
                elif 30 <= v <= 37 or 90 <= v <= 97:
                    cur.fg = PALETTE.get(v)
                elif 40 <= v <= 47:
                    cur.bg = PALETTE.get(v - 10)
                elif 100 <= v <= 107:
                    cur.bg = PALETTE.get(v - 10)
                elif v in (38, 48) and n + 1 < len(nums):
                    # 38;5;N or 38;2;R;G;B
                    if nums[n + 1] == 5 and n + 2 < len(nums):
                        col = indexed(nums[n + 2])
                        if v == 38:
                            cur.fg = col
                        else:
                            cur.bg = col
                        n += 2
                    elif nums[n + 1] == 2 and n + 4 < len(nums):
                        col = "#%02x%02x%02x" % (nums[n + 2], nums[n + 3], nums[n + 4])
                        if v == 38:
                            cur.fg = col
                        else:
                            cur.bg = col
                        n += 4
                n += 1
        i += m.end()
        continue

    if ch == "\r":
        cx = 0
    elif ch == "\n":
        cy += 1
    elif ch == "\b":
        cx = max(0, cx - 1)
    elif ch >= " ":
        if 0 <= cy < ROWS and 0 <= cx < COLS:
            cell = grid[cy][cx]
            cell.ch = ch
            cell.fg, cell.bg = cur.fg, cur.bg
            cell.bold, cell.dim, cell.rev = cur.bold, cur.dim, cur.rev
        cx += 1
    i += 1

# ---- emit SVG ----
CW, LH = 8.0, 17.0          # cell width, line height
PAD_X, PAD_Y = 16.0, 14.0
TOP = PAD_Y + (26.0 if TITLE else 0.0)
W = COLS * CW + PAD_X * 2
H = ROWS * LH + TOP + PAD_Y

parts = [
    f'<svg xmlns="http://www.w3.org/2000/svg" width="{W:.0f}" height="{H:.0f}" '
    f'viewBox="0 0 {W:.0f} {H:.0f}" font-family="ui-monospace,SFMono-Regular,'
    f'Menlo,Consolas,monospace" font-size="12.5">',
    f'<rect width="{W:.0f}" height="{H:.0f}" rx="8" fill="{BG}"/>',
]
if TITLE:
    for n, colour in enumerate(("#ff5f57", "#febc2e", "#28c840")):
        parts.append(f'<circle cx="{PAD_X + 6 + n * 15:.0f}" cy="17" r="5.5" fill="{colour}"/>')
    parts.append(
        f'<text x="{W / 2:.0f}" y="21" fill="#6b7280" text-anchor="middle" '
        f'font-size="11">{html.escape(TITLE)}</text>'
    )


def resolve(cell):
    fg = cell.fg or FG
    bg = cell.bg
    if cell.rev:
        fg, bg = bg or BG, cell.fg or FG
    return fg, bg


# Background rectangles first, merged into runs.
for y in range(ROWS):
    x = 0
    while x < COLS:
        _, bg = resolve(grid[y][x])
        if bg is None:
            x += 1
            continue
        run = x
        while run < COLS and resolve(grid[y][run])[1] == bg:
            run += 1
        parts.append(
            f'<rect x="{PAD_X + x * CW:.1f}" y="{TOP + y * LH:.1f}" '
            f'width="{(run - x) * CW:.1f}" height="{LH:.1f}" fill="{bg}"/>'
        )
        x = run

# Then text, merged into runs of identical style.
for y in range(ROWS):
    baseline = TOP + y * LH + LH - 4.5
    x = 0
    while x < COLS:
        cell = grid[y][x]
        if cell.ch == " " and not cell.rev:
            x += 1
            continue
        run, key = x, cell.key()
        chars = []
        while run < COLS and grid[y][run].key() == key:
            chars.append(grid[y][run].ch)
            run += 1
        body = "".join(chars).rstrip()
        if body:
            fg, _ = resolve(cell)
            attrs = f'fill="{fg}"'
            if cell.bold:
                attrs += ' font-weight="600"'
            if cell.dim:
                attrs += ' opacity="0.62"'
            parts.append(
                f'<text x="{PAD_X + x * CW:.1f}" y="{baseline:.1f}" {attrs} '
                f'xml:space="preserve">{html.escape(body)}</text>'
            )
        x = run

parts.append("</svg>")
open(OUT, "w").write("\n".join(parts))
print(f"{OUT}: {ROWS}x{COLS}", file=sys.stderr)
