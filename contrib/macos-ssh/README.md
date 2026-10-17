# Paste copied files into a UU-mapped SSH terminal on macOS

This optional Mac companion uploads files copied in Finder when you paste them
into Ghostty, then inserts shell-quoted **remote** paths into the terminal.
For example, copy an image in Finder, connect with `uu-ssh-paste`, and press
Command-V at a remote shell prompt or in a terminal application's text field.
The file is available on the server before its path is inserted. No Enter is sent.

The connection is Ghostty → OpenSSH → your existing UU TCP mapping → the Linux
host's SSH server. Both terminal traffic and uploads use the same SSH alias.
The companion does not create a mapping or fall back to a public network route.
The official Mac UU application's built-in terminal is unaffected.

## Install and connect

On the controlling Mac, install Python 3 and Apple's command-line developer
tools (for `swiftc`), then run from this repository:

```sh
sh contrib/macos-ssh/install.sh
```

Enable port mapping on your Linux UU host (`UU_PORT_MAPPING=1`); in the official
Mac UU app, map a **local loopback** port to `127.0.0.1:22` on that host. Keep
the mapping connected. Configure an SSH alias for that local port, using your
existing key and the server's verified host key. For example:

```sshconfig
Host uu-server
    HostName 127.0.0.1
    Port 22243
    User your-linux-user
    IdentityFile ~/.ssh/id_ed25519
    IdentitiesOnly yes
    HostKeyAlias your-existing-server-host-key-name
    StrictHostKeyChecking yes
    ProxyCommand none
    ProxyJump none
```

In Ghostty, connect using that alias:

```sh
uu-ssh-paste uu-server
# Or use a separate SSH config:
uu-ssh-paste -F ~/.config/your-uu-ssh.conf uu-server
```

Copy one or several files in Finder with Command-C, switch to the connected
terminal, and press Command-V. Each paste creates a unique mode-0700 directory
under `~/.local/share/uuremote/uploads/` on the server. Files remain there until
you remove them. Filenames with spaces, Unicode and shell metacharacters are
supported. Existing files are never overwritten by another paste batch.

Text paste, SSH exit statuses, terminal resizing and mouse reports pass through.
The remote application still controls whether it receives bracketed paste and
mouse movement. Ctrl-C during a transfer cancels that paste. Input typed while
the upload is running is queued, so Enter cannot overtake the uploaded path.

Ghostty's title bar shows the connection phase, upload percentage, transferred
size, transfer speed and elapsed time. Its progress indicator tracks the upload
too. Progress is derived from OpenSSH's transfer meter, aggregated across copied
files; it is not a simulated timer. The original terminal title is restored when
the upload finishes, fails or is cancelled, and no terminal cells are overwritten.
Filename and error output from SCP stays private; only numeric progress is shown.

## Scope and failure behavior

- Only file-URL clipboard entries copied in Finder are uploaded, and only when
  they match the complete pasted path list. Pasting a textual local path or a
  command does not upload anything. The clipboard is never modified or polled
  in the background; file URLs are read only during a potential file paste.
- Folder paste, drag-and-drop and raw screenshot/image clipboard data are not
  supported. Save an image as a file and copy that file in Finder first.
- The SSH server must support SFTP. Upload authentication is noninteractive
  (`BatchMode=yes`); the alias must already work with a key or SSH agent.
- Missing files, duplicate basenames in a batch, control characters in filenames,
  failed uploads and clipboard-reader failures do not insert a misleading path.
  Failed transfer directories are removed on a best-effort basis. No file names,
  clipboard contents, file contents, terminal input or SSH error text are logged.
- A paste is limited to 256 files and 8 MiB of pasted path/text data. Transfers
  time out after five minutes. File sizes themselves are not limited to 8 MiB.
- The companion keeps local bracketed-paste framing enabled while the session
  runs and removes framing for remote applications that have not requested it.
  No global Ghostty configuration, remote desktop or Wine component is needed.

## Tests

```sh
python3 -m unittest discover -s contrib/macos-ssh -p 'test_*.py' -v
swiftc -O contrib/macos-ssh/clipboard-files.swift -o /tmp/clipboard-files
/tmp/clipboard-files --self-test
# Optional real SSH/SFTP acceptance test over an already connected UU mapping:
UU_PASTE_TEST_HOST=uu-server UU_PASTE_TEST_CONFIG="$HOME/.config/your-uu-ssh.conf" \
    python3 -m unittest discover -s contrib/macos-ssh -p 'test_*.py' -v
```

The Swift test uses a private pasteboard and never touches the user's clipboard.
Python tests exercise fragmented paste framing, shell quoting, plain text,
mouse reports, failed uploads, queued Enter, PTY restoration and exit status.
The optional live tests transfer synthetic binary files and verify hashes,
including a throttled upload that must report intermediate byte progress.
Physical Command-C/Command-V behavior in Finder/Ghostty remains a manual check.
