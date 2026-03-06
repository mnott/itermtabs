#!/usr/bin/env python3
# encoding: utf-8

r"""
# itermtabs

Rename iTerm2 tabs programmatically — the reliable way.

## Why

iTerm2's tab title system has multiple layers (session name, tab name, title override), and most programmatic approaches fail:

- **Escape sequences** (`ESC ]1;title BEL`) get overwritten by the shell prompt within milliseconds
- **AppleScript `set name`** on a session changes the session name, not the tab title
- **AppleScript tab name** is read-only — `set name of tab` throws an error
- **`tabset` / `iterm2-tab-set`** use escape sequences under the hood — same problem

The only reliable method is iTerm2's **native WebSocket API**, which sets the `titleOverride` property — the same mechanism used when you double-click a tab to rename it.

This project provides three standalone implementations:

| Version | File | Dependencies |
|---------|------|-------------|
| **Node.js** | `itermtabs.mjs` | `ws` (WebSocket library) |
| **Python** | `itermtabs.py` | `iterm2`, `typer`, `rich` |
| **Rust** | `rust/` | `tungstenite`, `clap` (zero runtime deps when compiled) |

## Node.js Version

The Node.js version connects directly to iTerm2's WebSocket API using a hand-rolled Protocol Buffer encoder/decoder. No protobuf library needed — just the `ws` package.

### Install

```bash
npm install -g @tekmidian/itermtabs
```

### Usage

```bash
# List all tabs
./itermtabs.mjs --list

# Rename by tab ID
./itermtabs.mjs 77 "My Project"

# Rename by name substring (case-insensitive, must be unique)
./itermtabs.mjs "jobs" "Job Search"
```

The target can be a **tab ID** (number), a **tab name substring** (case-insensitive, must match exactly one tab), or a **session UUID**.

### How It Works

1. Requests an auth cookie from iTerm2 via AppleScript
2. Connects to iTerm2's unix socket (`~/Library/Application Support/iTerm2/private/socket`) or TCP port 1912
3. Sends a `ListSessions` request and `GetVariable` requests to resolve the target tab
4. Sends an `InvokeFunction` request with `iterm2.set_title()` to set the title override

The protobuf encoding is hand-rolled for the specific messages needed — field tags, varints, length-delimited strings, and doubles. No protobuf compiler or library required.

## Python Version

The Python version uses the official [`iterm2`](https://pypi.org/project/iterm2/) module, which handles the WebSocket protocol internally.

### Install

```bash
# With uv (recommended)
uv pip install iterm2 typer rich

# Or with pip
pip install iterm2 typer rich
```

### Usage

```bash
# List all tabs
python itermtabs.py list

# Rename by tab ID
python itermtabs.py rename 77 "My Project"

# Rename by name substring
python itermtabs.py rename "jobs" "Job Search"

# Generate README
python itermtabs.py doc > README.md
```

## Rust Version

The Rust version compiles to a single static binary with no runtime dependencies. It uses the same hand-rolled protobuf approach as the Node.js version.

### Build

```bash
cd rust
cargo build --release
# Binary at rust/target/release/itermtabs
```

### Usage

```bash
# List all tabs
./itermtabs list

# Rename by tab ID
./itermtabs rename 77 "My Project"

# Rename by name substring
./itermtabs rename "jobs" "Job Search"
```

## iTerm2 Configuration

For the API connection to work, iTerm2 must have its Python API enabled:

1. Open iTerm2 → Preferences → General → Magic
2. Check **"Enable Python API"**

The first time you run either version, iTerm2 may show a dialog asking permission for the app to connect.

## How Tab Titles Work in iTerm2

iTerm2 has a layered title system:

```
Display title = titleOverrideFormat ?? titleOverride ?? (profile title format applied to session name)
```

- **`session.name`** — The session's name (settable via AppleScript, but doesn't control the tab title)
- **`tab.titleOverride`** — Set by double-click rename or the WebSocket API's `set_title()` function
- **`tab.titleOverrideFormat`** — Format string for the override (usually empty)
- **Profile title components** — The profile's "Title" setting (General tab) with variables like `\(session.name)`

This tool sets `titleOverride`, which takes precedence over everything except `titleOverrideFormat`.

## Credits

The Python version uses the [`iterm2`](https://iterm2.com/python-api/) Python module by **George Nachman** (creator of iTerm2). The Node.js and Rust versions were built by reverse-engineering the same WebSocket protocol that the Python module uses, implementing just the minimal protobuf encoding needed for session listing and title setting.

## License

MIT
"""

import ast
import os
from typing import NamedTuple

import iterm2
import typer
from rich.console import Console
from rich.table import Table

app = typer.Typer(
    name="itermtabs",
    help="Rename iTerm2 tabs via the native WebSocket API.",
    no_args_is_help=True,
)
console = Console()

# Shared state for passing results out of iterm2.run_until_complete callbacks
_result: list[dict] = []
_error: str | None = None


async def _list_sessions_coro(conn: iterm2.Connection) -> None:
    """Callback for iterm2.run_until_complete — populates _result."""
    global _result
    _result = []
    app_obj = await iterm2.async_get_app(conn)
    if not app_obj:
        return
    for window in app_obj.terminal_windows:
        for tab in window.tabs:
            tab_title = await tab.async_get_variable("title") or ""
            session_name = tab.sessions[0].name if tab.sessions else ""
            _result.append({
                "tab_id": tab.tab_id,
                "name": tab_title or session_name or "",
            })


async def _resolve_tab(app_obj, identifier: str):
    """Resolve a tab by tab ID, session UUID, or tab name substring."""
    # Try tab ID (numeric)
    for window in app_obj.terminal_windows:
        for tab in window.tabs:
            if tab.tab_id == identifier:
                return tab
    # Try session UUID
    for window in app_obj.terminal_windows:
        for tab in window.tabs:
            for session in tab.sessions:
                if session.session_id == identifier:
                    return tab
    # Try tab title substring (case-insensitive)
    matches = []
    for window in app_obj.terminal_windows:
        for tab in window.tabs:
            title = await tab.async_get_variable("title") or ""
            if identifier.lower() in title.lower():
                matches.append((tab, title))
    if len(matches) == 1:
        return matches[0][0]
    if len(matches) > 1:
        names = ", ".join(f'"{m[1]}"' for m in matches)
        raise RuntimeError(f'Ambiguous: "{identifier}" matches {len(matches)} tabs: {names}')
    return None


async def _set_title_coro(conn: iterm2.Connection) -> None:
    """Callback for iterm2.run_until_complete — sets tab title."""
    global _error
    _error = None
    app_obj = await iterm2.async_get_app(conn)
    if not app_obj:
        _error = "Could not connect to iTerm2"
        return
    tab = await _resolve_tab(app_obj, _set_title_target)
    if not tab:
        _error = f'No tab matching "{_set_title_target}"'
        return
    await tab.async_set_title(_set_title_new_title)

# Parameters for _set_title_coro (set before calling run_until_complete)
_set_title_target: str = ""
_set_title_new_title: str = ""


@app.command("list")
def list_sessions():
    """List all iTerm2 tabs with their IDs and names."""
    iterm2.run_until_complete(_list_sessions_coro)
    sessions = _result
    if not sessions:
        console.print("[yellow]No sessions found.[/yellow]")
        return

    table = Table(title="iTerm2 Tabs")
    table.add_column("Tab", style="cyan", justify="right")
    table.add_column("Name", style="white")
    for s in sessions:
        table.add_row(s["tab_id"], s["name"])
    console.print(table)


@app.command("rename")
def rename(
    target: str = typer.Argument(help="Tab ID, session UUID, or tab name substring"),
    title: str = typer.Argument(help="The new tab title"),
):
    """Rename a tab by ID, session UUID, or name."""
    global _set_title_target, _set_title_new_title
    _set_title_target = target
    _set_title_new_title = title
    iterm2.run_until_complete(_set_title_coro)
    if _error:
        console.print(f"[red]Error: {_error}[/red]")
        raise typer.Exit(code=1)
    console.print(f'[green]Tab title set to "{title}"[/green]')


# ── Doc Generator (self-contained, same pattern as gitflow) ──

class DocItem(NamedTuple):
    name: str
    doc: str
    lineno: int
    type: str


class DocGenerator:
    """Extract module docstring and command docstrings to produce Markdown."""

    @staticmethod
    def extract_docstrings(filename: str) -> list[DocItem]:
        with open(filename, "r") as f:
            node = ast.parse(f.read())
        module_doc = ast.get_docstring(node)
        docs = [DocItem("Module", module_doc, 0, "module")] if module_doc else []
        for item in node.body:
            if isinstance(item, (ast.FunctionDef, ast.ClassDef)):
                doc = ast.get_docstring(item)
                if doc:
                    docs.append(DocItem(item.name, doc, item.lineno, type(item).__name__))
        docs.sort(key=lambda x: x.lineno)
        return docs

    @classmethod
    def generate_doc(cls, filename: str, title: str | None = None, toc: bool = False, full_doc: bool = False) -> str:
        docs = cls.extract_docstrings(filename)
        base_name = os.path.splitext(os.path.basename(filename))[0]
        content = ""
        module_doc = next((d for d in docs if d.type == "module"), None)
        if module_doc:
            content += module_doc.doc.strip() + "\n"
        if full_doc:
            if toc:
                content += "\n## Table of Contents\n\n"
                for i, d in enumerate(docs, 1):
                    if d.type != "module":
                        anchor = d.name.lower().replace(" ", "-").replace(".", "")
                        content += f"{i}. [{d.name}](#{anchor})\n"
                content += "\n"
            for d in docs:
                if d.type != "module":
                    content += f"\n## {d.name}\n\n{d.doc.strip()}\n"
        return content


@app.command("doc")
def doc(
    title: str = typer.Option(None, help="Override the document title"),
    toc: bool = typer.Option(False, help="Include a table of contents"),
    full: bool = typer.Option(False, "-f", "--full", help="Include all command docstrings"),
):
    """
    Generate documentation from the module docstring.

    By default outputs the module docstring as Markdown (suitable for README.md).
    Use --full to also include individual command docstrings.

    Examples:
        itermtabs doc > README.md
        itermtabs doc --full --toc
    """
    result = DocGenerator.generate_doc(__file__, title, toc, full_doc=full)
    print(result)


if __name__ == "__main__":
    app()
