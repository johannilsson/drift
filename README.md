# Drift


A terminal TUI for monitoring git repository changes in real time. Designed to run alongside an editor or AI coding tool as a passive change monitor.

## Install

```sh
cargo install --path .
```

## Usage

Run from within any git repository:

```sh
drift
```

## Keys

### List view

| Key | Action |
|-----|--------|
| `j` / `↓` | move down |
| `k` / `↑` | move up |
| `Enter` | open diff for selected file |
| `e` | open file in `$EDITOR` |
| `q` / `Ctrl+c` | quit |

### Diff view

| Key | Action |
|-----|--------|
| `j` / `↓` | line down |
| `k` / `↑` | line up |
| `Ctrl+d` / `Ctrl+u` | half page down/up |
| `Ctrl+f` / `Ctrl+b` | full page down/up |
| `PageDown` / `PageUp` | full page down/up |
| `g` | top |
| `G` | bottom |
| `e` | open file in `$EDITOR` |
| `Esc` / `q` | back to list |

## Editor

Opens `$EDITOR`, falling back to `nvim` → `vim` → `vi`.
