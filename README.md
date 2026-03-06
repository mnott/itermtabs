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

