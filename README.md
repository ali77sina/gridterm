# gridterm

A fast, GPU-rendered grid terminal for macOS. Split the window into any NxM
grid of real shells, with a built-in AI assistant that can see and drive the
panes.

## Features

- GPU rendering with wgpu and glyphon for low-latency, low-RAM output
- Dynamic grids: split into any layout (2x2, 3x2, 4x4) on the fly
- Full terminal emulation via alacritty_terminal and a real PTY per pane
- 24-bit color, text selection, copy and paste, scrollback
- macOS keybindings (word and line delete, word and line motion)
- A built-in chat assistant that can run commands in panes, read their output,
  search them, manage the grid, and use a browser to test web apps
- Per-pane cost and token tracking for coding agents that emit OpenTelemetry
- Crash and freeze logging to help diagnose issues

## Build

Requires a recent Rust toolchain.

```bash
cargo build --release
./target/release/gridterm
```

## AI assistant (optional)

The built-in assistant uses Azure OpenAI. Copy the example config and fill in
your own values:

```bash
cp .env.example .env
# edit .env with your endpoint, deployment, and key
```

The `.env` file is gitignored and never committed. If no config is present,
gridterm runs as a plain terminal and the assistant stays disabled.

## Keybindings

- `Cmd+1` to `Cmd+9`: square grid presets
- `Cmd+G` then `NxM` and Enter: custom grid
- `Cmd+Shift+Arrow`: add or remove a column or row (keeps existing panes)
- `Ctrl+]`: cycle the active pane
- `Cmd+J`: toggle the chat assistant
- `Cmd+C` and `Cmd+V`: copy and paste
- `Option+Backspace`: delete word, `Cmd+Backspace`: delete line

## License

MIT. See [LICENSE](LICENSE).
