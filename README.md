# catshell

A terminal and a file explorer that share one SSH connection.

Most SSH clients open a second connection for their file browser, which means a second
authentication and two views of the server that drift apart. catshell multiplexes the
shell channels and the SFTP channel over a single authenticated transport, so the file
explorer is looking at the same session as the prompt: `cd` in the terminal and the
explorer follows.

The interface is drawn on the GPU — the terminal grid is a single instanced draw call
against a glyph atlas — because a terminal that cannot outpace `cat` on a large file is
one that feels slow.

## Download

Prebuilt binaries are on the [releases page](https://github.com/catnoom/catshell/releases/latest).
Unpack and run `catshell`; there is nothing to install.

| Platform | File |
| --- | --- |
| Linux (x86_64, glibc 2.35+) | `catshell-*-x86_64-unknown-linux-gnu.tar.gz` |
| Windows (x86_64) | `catshell-*-x86_64-pc-windows-msvc.zip` |
| macOS (Apple Silicon) | `catshell-*-aarch64-apple-darwin.tar.gz` |

## Build from source

Needs a stable Rust toolchain. On Linux, the X11 and Wayland development packages:

```sh
sudo apt-get install pkg-config libwayland-dev libxkbcommon-dev libx11-dev
cargo build --release
```

## Keys

| | |
| --- | --- |
| `Ctrl+Shift+D` / `Ctrl+Shift+E` | Split the pane left/right, or top/bottom |
| `Ctrl+Shift+W` | Close the focused pane |
| `Ctrl+Tab` | Move to the next pane |
| `Ctrl+Shift+T` | New tab |
| `Ctrl+Shift+B` | Broadcast typing to every pane in the tab |
| `Ctrl+Shift+C` / `Ctrl+V` | Copy the selection / paste |

Plain `Ctrl` combinations are left alone: a shell needs `Ctrl+C` more than an interface
needs a shortcut.

## Configuration

Under the user config directory — `~/.config/catshell` on Linux,
`%APPDATA%\catshell` on Windows, `~/Library/Application Support/catshell` on macOS:

* `hosts.toml` — saved hosts, editable from the host list or by hand. Hosts from
  `~/.ssh/config` are read as well and shown alongside them.
* `config.toml` — font, scrollback, download directory, shell integration.

Keys come from `ssh-agent`; passwords, if you choose to save one, go to the OS keyring
(Secret Service, Credential Manager, Keychain) and never to a file.

## Licence

MIT or Apache-2.0, at your option.
