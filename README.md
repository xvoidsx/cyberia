<div align="center">
  <h1>C Y B E R I A</h1>
</div>

<img width="665" height="692" alt="image" src="https://github.com/user-attachments/assets/42ff17a1-d90c-4e7d-9480-bb1a1d92c2e3" />

###### cyberia in 'ambient' mode, inviting you to jump back into your conversations, and showing you what you missed while you were away

An opinionated [Matrix](https://matrix.org/) TUI, dressed in nightshadeNeon.
The UI is kept as simple as possible and there's no mouse support (yet, at
least). With the assumption that usually only the last couple messages
actually matter, vertical space is not conserved at all.

Cyberia is a friendly fork of [matui](https://github.com/pkulak/matui) by
Phil Kulak, maintained by [xvoidsx](https://github.com/xvoidsx). Improvements
that fit upstream's vision are contributed back; the rest lives here.

# Who should use this client?

Anyone who wants a very simple terminal Matrix client with a neon soul.
Room administration beyond moderation (settings, power levels) is left to
other clients.

# Installation

## Releases

You can download the latest release, unpack it, and move `cyberia` to `/usr/bin`
(or anywhere else you like).

## From source

`cargo build --release` — the binary lands at `target/release/cyberia`.

# Keybindings

Modal UIs can be a bit overwhelming, but thankfully chat isn't terribly
complicated. Especially if you don't implement too many features.

| Key    | Description                                            |
|--------|--------------------------------------------------------|
| Space  | Show the room switcher.                                |
| j*     | Select one line down.                                  |
| k*     | Select one line up.                                    |
| Ctrl+d | Select half a page down.                               |
| Ctrl+u | Select half a page up.                                 |
| G      | Select latest message.                                 |
| i      | Create a new message using the external editor.        |
| I      | Create a new message directly in the external editor.  |
| Enter  | Open the selected message (images, videos, urls, etc). |
| Esc    | Leave the thread view.                                 |
| s      | Save the selected message (images and videos).         |
| c      | Edit the selected message in the external editor.      |
| dd     | Delete the selected message (with confirmation).       |
| r      | React to the selected message.                         |
| R      | Reply to the selected message.                         |
| T      | Start (or open) a thread on the selected message.      |
| U      | Open the command prompt with the sender filled in.     |
| v      | View the selected message in the external editor.      |
| V      | View the current room in the external editor.          |
| u      | Upload a file.                                         |
| p      | Paste an image from the clipboard.                     |
| m      | Mute or unmute the current room (until restart).       |
| A      | Ambient mode: katakana rain with unread summary (any key exits). |
| /      | Search the current room.                               |
| :      | Open the command prompt.                               |
| ?      | Show this helper (press again for commands).           |

\* arrow keys are fine too

# License

GPL-2.0, same as upstream matui.
