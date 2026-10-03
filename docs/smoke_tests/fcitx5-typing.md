# fcitx5 text insertion (fcitx5 driver)

Verifies that the `fcitx5` driver inserts text through fcitx5's input method,
falls through to the next driver when there is no text field to commit to, and
reports a missing addon as a setup step rather than as a typing failure.

Needs fcitx5 running with the `fcitx5-commit` addon (`yay -S fcitx5-commit-git`,
then `fcitx5 -r -d`; fcitx5 loads addons only at startup).

## 1. The addon answers

```bash
busctl --user introspect org.fcitx.Fcitx5 /commit
# Expected: interface io.github.vendetta1871.Commit1 with CommitString

busctl --user call org.fcitx.Fcitx5 /commit \
    io.github.vendetta1871.Commit1 CommitString s ""

# b true  - a focused text field can receive text
# b false - nothing focused; the driver treats this as "try the next driver"
# a D-Bus error naming the addon/object means the addon is not loaded
```

## 2. Text arrives in one piece, whatever the layout

Focus a text field in an application that is an fcitx5 client (a GTK entry with
`fcitx5-gtk` installed, a Qt field with `QT_IM_MODULE=fcitx`, an Xlib app such as
xterm with `XMODIFIERS=@im=fcitx`, or a Wayland client), then:

```bash
busctl --user call org.fcitx.Fcitx5 /commit \
    io.github.vendetta1871.Commit1 CommitString s "今天天气很好，hello"
```

**Expected:** the whole string appears at once. Nothing is dropped, no keyboard
layout is involved, and the application's layout stays whatever it was.

The same through the voxtype driver, which is the call dictation makes:

```bash
cargo run --example fcitx5_commit_probe -- "今天天气很好，hello"
# available: true
# committed 15 chars
```

`available: false` means no focused input context (or the addon is missing; the
daemon log says which). The driver reports `commit failed: fcitx5 has no focused
input field to commit to.` in that case, and the chain moves to the next driver.

The full daemon path, with a sandbox daemon whose transcription is replaced
by `post_process` (see the x11 smoke test for the setup), and:

```toml
[output]
mode = "type"
driver_order = ["fcitx5", "x11"]
auto_submit = false
```

**Expected:** the daemon log shows `Text committed via fcitx5` and no keymap
change happens at all (`x11_keymap_settle_ms` is never waited out because the
x11 driver is never reached).

## 3. No input context falls through, it does not lose text

Focus the desktop rather than a text field and dictate.

**Expected:** the log shows `fcitx5 not available` (the addon's `false` answer),
then the next driver in `driver_order` types the text. With `driver_order =
["fcitx5", "x11"]` the transcription arrives through the x11 driver.

## 4. A missing addon is a setup step, not a typing failure

Stop fcitx5 (`fcitx5-ctl stop` or kill it) or uninstall the addon, then dictate.

**Expected:** the log names the addon and the install command:

```text
The fcitx5-commit addon is not loaded, so voxtype cannot insert text through fcitx5.
  yay -S fcitx5-commit-git   # Arch / Manjaro
  fcitx5 -r -d               # fcitx5 loads addons only at startup
```

and the chain continues to the next driver, so dictation still works.
