---
name: screenshot
description: How to take a screenshot of the screen, a window or a region with the platform's own CLI through bash (grim and slurp, niri, sway, Hyprland, KDE, GNOME, X11, macOS) and look at it with the read tool. Load it before the first screenshot when asked to look at something on the screen, check how an app or window looks, or see what the user sees.
---

A screenshot is a file you take with the platform's capture command through bash, then `read`: the read tool shows you the image, scaled to fit 1920 pixels on its longest side. For a web page in the browser you drive, use the browser skill's `agent-browser screenshot` instead.

## Mind what you capture

Everything in the image goes to the model's provider. Capture the smallest thing that answers: the one window, or a region of it, rather than the whole desktop, which may show the user's mail, chats or other private things. Take the whole screen only when the user asks for it. Never capture the screen on your own initiative to see what the user is doing.

## Find the capture tool

`!` commands in a skill do not run when you load it, so look first, in one bash call:

```sh
echo "$XDG_CURRENT_DESKTOP $XDG_SESSION_TYPE"; command -v niri grim slurp swaymsg hyprctl jq spectacle gnome-screenshot maim xdotool import screencapture
```

None there: tell the user which one to install for their desktop (below), and do not install it yourself unasked.

## Write it here

`${TMPDIR:-/tmp}/nth-screenshot-$(date +%s).png`, then `read` that path. The read keeps its own copy, so the file may go afterwards.

## By desktop

- **niri**: `niri msg --json windows` lists every window's `id`, `app_id` and `title`; pick by those. `niri msg action screenshot-window --id <id> --path <file> -d true` captures that window, focused or not, and `niri msg action screenshot-screen --path <file> -d true` the focused output. Both also put the image on the user's clipboard, replacing what was there; say so when you use them. For a region, use grim.
- **wlroots compositors** (sway, Hyprland, river, niri too): `grim <file>` is every output, `grim -o <output> <file>` one (`swaymsg -t get_outputs`, `hyprctl monitors`), `grim -g "<x>,<y> <w>x<h>" <file>` a region. `grim -g "$(slurp)" <file>` lets the user drag the region: ask first, as they have to act.
- **sway**: the focused window's region is `swaymsg -t get_tree | jq -r '.. | select(.focused?) | .rect | "\(.x),\(.y) \(.width)x\(.height)"'`, for `grim -g`.
- **Hyprland**: `hyprctl -j activewindow | jq -r '"\(.at[0]),\(.at[1]) \(.size[0])x\(.size[1])"'`, for `grim -g`; `hyprctl -j clients` lists the others.
- **KDE**: `spectacle -b -n -a -o <file>` the active window, `-f` instead of `-a` the whole screen, `-r` a region the user draws.
- **GNOME** (Wayland): `gnome-screenshot -w -f <file>` the focused window, without `-w` the screen. Newer GNOME may lack it and only offer the interactive screenshot portal: then ask the user to take the screenshot and give you the path.
- **X11**: `maim <file>` the screen, `maim -i "$(xdotool getactivewindow)" <file>` the focused window, `maim -g <w>x<h>+<x>+<y> <file>` a region; `import -window root <file>` where only ImageMagick is there.
- **macOS**: `screencapture -x <file>` the screen without the shutter sound, `-R<x>,<y>,<w>,<h>` a region, `-l <window id>` a window. The terminal needs the Screen Recording permission; an all-black image means it lacks it.

## Reading what you took

Text that came out too small to read after scaling down is not a reason to capture the whole screen again: capture a region around it, which is shown at full size. Describe what the image shows, and say when something is too small or cut off rather than guessing.
