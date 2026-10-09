# lintel

## Author's note

This project was born from the need to let an agent control my Linux
box. That is why it runs on Arch and sway: it is cut to measure for my
own setup, not a general-purpose bar. It has an MCP server for that
agent, Jarvis: a customised Claude Code session that lives in
`~/Agents/Jarvis`, with its own context files and log, and that starts
with lintel's MCP server to see and drive the desktop.

Keeping everything in one program also saves on prompts and tokens.
The bar, the notifications, the windows and the outputs are already in
one place, so one tool call returns the whole state, compact and
structured, instead of the agent piecing it together from a string of
shell commands, each to be written, run and read back.

## What it is

An event-driven bar for sway. One process, written in Rust on GTK 4 and
gtk4-layer-shell, that is:

- the **bar**, one per output, with a card that opens under each module
  on hover: its information, its switches and buttons, its mouse actions;
- the **tray**, a StatusNotifier host with the items' menus;
- the **launcher** and **window switcher**, and a dmenu for scripts;
- the **notification daemon**, drawing its own cards under the bar;
- a **pomodoro** timer and any number of named **countdowns**;
- an **MCP server**, so an assistant can see and drive the desktop.

Nothing polls. Every value is read when the system says it changed (a
kernel uevent, a D-Bus signal, a netlink message, a sway event), when you
ask for it, or when you open its card. The clock and the timers sleep
until the instant their text changes. GTK 4 renders at each output's own
scale, fractional ones included.

## Install

Needs gtk4, gtk4-layer-shell and libpulse (PipeWire's pulse server is
fine), and a Nerd Font for the icons of the example config.

```sh
make install     # cargo build --release, the binary to ~/.local/bin/lintel
make config      # config/lintel.toml and style.css to ~/.config/lintel (if none there)
make service     # the user unit, to start from sway
```

Then in sway's config, once the session's variables reach the user
manager (sway's `50-systemd-user.conf` imports them):

```
exec_always systemctl --user start lintel
```

or run it by hand: `lintel`, or `lintel --config FILE`. A second instance
cannot bind the socket and exits.

## How it works

### Sources

A **source** produces a variable, a JSON value. Its kind says how:

| kind          | the value                                                                                  |
|---------------|--------------------------------------------------------------------------------------------|
| `listen`      | a command that runs for good; every line it prints is the new value                        |
| `command`     | a command run at start and again on each of its **triggers**; its output is the value      |
| `clock`       | the time, `format` and `info_format` (strftime); wakes exactly when the text changes, and when the system clock is set |
| `sway`        | workspaces, binding mode, focused title over sway's IPC; also fills `scratchpad`           |
| `display`     | the outputs, on sway's output events                                                       |
| `audio`       | the default output and input, from the sound server's events                               |
| `mpris`       | the media player through playerctld, its stream's volume from the sound server             |
| `battery`, `network`, `bluetooth`, `brightness`, `powerprofile`, `nightlight` | native readers (sysfs, iwd and rtnetlink, bluez, the backlight, power-profiles-daemon, gammastep's user unit), run on triggers like a `command` |
| `pomodoro`, `timer` | the pomodoro and the countdowns, driven over the socket                              |
| `static`      | a constant, until `lintel update` changes it                                               |

Output that is not JSON is taken as a string. The notification daemon
fills a `notifications` variable of its own.

### Triggers

The events that make a `command` or a native reader run again, read
natively; no process is spawned until one fires:

| trigger              | fires on                                                              |
|----------------------|-----------------------------------------------------------------------|
| `udev:SUBSYSTEM`     | a kernel uevent (`power_supply`, `backlight` ...)                      |
| `dbus:BUS:NAME:PATH` | a signal from NAME under PATH; BUS is `session` or `system`; for systemd the manager is subscribed first |
| `netlink`            | a link, address or route change                                       |
| `sway:EV[,EV]`       | sway IPC events (`output`, `tick`, `workspace` ...)                   |
| `refresh`            | `lintel refresh NAME`, from a script that just changed something      |
| `hover`              | the module's card opens: read when you look at it                     |
| `timer:SECONDS`      | a plain interval, for the rare thing that has no event                |

A burst (five volume steps, a flurry of signals) becomes one read: the
source waits `coalesce_ms` after the first event.

A few things cannot announce a change. A monitor driven over DDC/CI has
no event at all, so the `brightness` source reads it through its
`command` on your own writes, on a screen pick and when the card opens.
How often a battery reports its capacity is up to the firmware.

### Modules and cards

A **module** shows variables through templates (`{battery.text}`,
`{sway.title|trunc:60}`) and runs commands on the mouse: left, middle,
right, wheel up and down. A command goes through `swaymsg exec`, so the
program lives under sway, with the session's environment, and outlives
a restart of the bar; `$S` (the `scripts` directory) and the bar's
`PATH` go with it.

Resting the pointer on a module opens its **card** under it: the title,
the information, and what the module declares:

- `toggles`: switches, each with a name, a line under it and the
  commands for on and off. The state is a template, and the switch
  follows its variables, so a change made elsewhere shows at once.
- `buttons`: rows of buttons with a caption. With a `state` the row is
  a choice, and the button whose value matches is marked (the power
  profile in use).
- `input`: a value field, a label field and a button (a countdown's
  minutes and name). The card takes the keyboard while it is open.
- `hints`: the module's mouse actions, as a table.

A `calendar` module's card carries a month grid: `‹` `›` or the wheel
move a month, the month name returns to today, and from the keyboard
←/→ months, ↑/↓ years, `t` today, `q` or Escape closes. Month and day
names are the system locale's.

The card lives in a second, transparent, full-width layer-shell window
that stays mapped; hovering only moves a box in it, and its input region
is the card alone, so the windows beneath keep their clicks.

### The tray

The bar serves `org.kde.StatusNotifierWatcher` (or uses the one already
on the bus), registers as a host and follows every item on its signals.
Icons come from the theme or the item's pixmaps, scaled for the output.
Left click activates (or opens the menu of a menu-only item), middle
click is the secondary action, right click the menu, read from the
item's dbusmenu when needed, the wheel scrolls. `Passive` items are
hidden.

### The picker

`lintel launch` opens the launcher in the middle of the screen: the
desktop entries (name, generic name, keywords and comment, weighed in
that order, with their icons) and, under each app, its open windows. An
app runs a new instance, a window is focused; an app with
`SingleMainWindow` and its window open shows the window alone.
`lintel windows` is the windows alone, Ctrl+Tab swaps the two, and both
take text already typed (`lintel launch mail`).

Keys: Down, Tab, Ctrl+N or Ctrl+K go down; Up, Shift+Tab, Ctrl+P or
Ctrl+L go up; Page keys jump; Enter picks; Escape or Ctrl+G cancel;
Ctrl+Tab, Ctrl+J or Ctrl+; go to the other list. The mouse selects by
moving over a line, picks with a click, and the wheel scrolls the lines
past `rows`.

The order is **frecency**: how often and how recently a line was picked,
per list, with a half-life of thirty days, kept in
`~/.local/state/lintel/frecency.json`. While typing, the fuzzy score
leads and frecency pushes.

`lintel pick NAME [PROMPT]` is a dmenu: lines on stdin (with an icon as
`text\0icon\x1fNAME`), the chosen one on stdout, or its number with
`--index`.

### The notifications

lintel owns `org.freedesktop.Notifications` and draws the notifications
under the bar: icon or image, summary, body with markup, a progress bar
for the `value` hint, actions as buttons. Each urgency has its frame and
timeout; `replaces_id` and the `x-dunst-stack-tag` hint replace in
place; a history keeps the last ones; a pause sends new ones straight to
it.

A left click invokes the default action and goes to the app's window,
found by the `desktop-entry` hint or the app's name, out of the
scratchpad if needed. Right click closes, middle click closes all.

```
lintel notifications close | close-all | action | pop | history | clear | toggle | status | list
```

`history` opens the picker on the history and shows the chosen one
again; `toggle` is the pause; `list` prints everything as JSON.

### The pomodoro and the countdowns

Work, short break, a long break every few pomodori, a cheer at the end
of a session. The phases notify with Skip and Stop buttons and can run
`on_phase_end` (a bell). The state is saved in
`~/.local/state/lintel/pomodoro.json` and read back at start, so a
restart keeps the session. Phases run on monotonic time: a suspended
laptop pauses them.

`lintel timer 7m Pasta` starts a countdown; the label is its key and its
notification's title. The example shows them as a list module, and the
pomodoro's card has the fields to start one.

## Talking to it

```
lintel refresh NAME            read source NAME again
lintel update NAME VALUE       set a variable (JSON or text)
lintel get NAME                print a variable
lintel state                   every variable, as one JSON object
lintel pomodoro start|stop|pause|resume|toggle|skip|status
lintel timer 7m [LABEL]        a countdown (7m, 90s, 1h30m)
lintel timer stop [LABEL]      end one, or all; `timer status` lists them
lintel launch | windows        the launcher, the window switcher
lintel pick NAME [PROMPT]      a dmenu; --index prints the number
lintel cancel                  close the picker
lintel notifications CMD       see above
lintel mcp                     the MCP server, on stdin and stdout
lintel quit
```

The socket is `$XDG_RUNTIME_DIR/lintel.sock`, a line in and a line out,
so `printf 'pomodoro status\n' | socat - UNIX:$XDG_RUNTIME_DIR/lintel.sock`
works too.

### The MCP server

`lintel mcp` speaks the Model Context Protocol, one JSON-RPC message per
line. For Claude Code: `--mcp-config` with
`{"mcpServers": {"lintel": {"command": "lintel", "args": ["mcp"]}}}`.

| tool            | what it does                                                                 |
|-----------------|------------------------------------------------------------------------------|
| `bar_state`     | every module as on screen (text, card info, class, hidden, actions, a list's items) and the raw variables |
| `bar_command`   | a request to the bar, as `lintel ...` sends it; interactive ones are refused |
| `notifications` | on screen and the history                                                    |
| `sway_windows`  | every window with workspace, output, geometry, state and its processes       |
| `sway_get`      | any sway IPC query                                                           |
| `sway_command`  | sway commands, as swaymsg runs them                                          |
| `screenshot`    | an output, a window or a region, through grim                                |
| `logs`          | the journal: lintel's unit, another unit, a syslog tag, the kernel           |

Without the bar running, the sway, screenshot and journal tools still
answer.

## Configuration

`~/.config/lintel/lintel.toml` and `style.css` next to it;
`config/lintel.toml` is a complete, commented example.

`[bar]`: `height`, `card_width`, `scripts` (exported as `$S`),
`hover_delay_ms` (rest this long before a card opens),
`leave_delay_ms`, `coalesce_ms`.

`[picker]`: `width`, `rows`, `icon_size`, `terminal` (prefixed to the
entries that want one), `scratchpad` (a command that brings a scratchpad
window up, `{id}` its con id; by default sway's own focus).

`[notifications]`: `enabled`, `position` (top-left, top-center,
top-right), `offset`, `width`, `gap`, `icon_size`, `max_visible`,
`history`, `timeout_low`, `timeout_normal`, `timeout_critical` (seconds,
0 until closed).

`[pomodoro]`: `work_minutes`, `short_break_minutes`,
`long_break_minutes`, `pomodori_until_long`, `pomodori_per_session`,
`display` (`minutes`, or `seconds` for mm:ss), `on_phase_end`, `icons`
(work, short_break, long_break, paused, stopped, countdown).

`[[source]]`: `name`, `kind`, `command`, `initial` (the value before the
first read), `triggers`, and for the clock `format` and `info_format`.

`[[module]]`: `name`, `side` (left or right), `kind` (`module`, `label`
without a card, `list`, `tray`, `calendar`), `title`, `text`, `info`,
`class` (templates), `markup` (pango), `max_chars` (the text's width at
most; when the bar is short of room this label shrinks first),
`hide_when`, `click`, `middle`, `right`, `scroll_up`, `scroll_down`,
`hints` (badge and text pairs), `input` (`prompt`, `default`, `label`,
`label_default`, `button`, `command` with `{value}` and `{label}`),
`[[module.toggles]]` (`label`, `detail`, `state`, `on`, `off`; the state
is off when it renders "", false, 0, null or off),
`[[module.buttons]]` (`label`, `state`, `items` with `text`, `command`
and `value`).
For `list`: `items` (the array, `sway.workspaces`), `item_text`,
`item_class`, `item_click`, `item_middle`, `item_right`, templates over
each item. For `tray`: `icon_size`, `spacing`.

Template filters: `trunc:N`, `upper`, `lower`, `or:TEXT`, `if:TEXT`
(TEXT when the value is true-ish), `esc` (pango escape).

### Styling

GTK 4 CSS. The bar window has class `lintel-bar`, the card's window
`lintel-popup`, the picker's `lintel-picker`. A module has `module`, its
name, its `class` template's output, and `open` while its card shows; a
list has `list` and its name, its buttons `item` and their class. In
the card: `.card`, `.card-title`, `.toggles` and `.toggle` (`.name`,
`.detail`, `switch`), `.buttons` and `.button-row` (buttons `.active`
when chosen), `.input`, `.hints`, `.hint`, `.key`, and the calendar's
`.calendar`, `.head`, `.nav`, `.title`, `.weekday` (`.weekend`), `.day`
(`.outside`, `.today`). The picker: `.picker`, `.head`, `.prompt`,
`entry`, `.row` (`.selected`, `.child`), `.detail`.

## Source layout

```
src/main.rs              the command line: run the bar, or talk to it
src/config.rs            lintel.toml
src/template.rs          {var.path|filter}
src/ipc.rs               the socket, client and server
src/mcp.rs               the MCP server
src/runner.rs            running commands, detached or captured
src/locale.rs            the system locale for dates
src/sources/mod.rs       the hub: starts the sources, routes requests
src/sources/command.rs   listen and command sources
src/sources/snapshot.rs  a value read on triggers, for command and the native readers
src/sources/triggers.rs  udev, dbus, netlink, sway, timer
src/sources/dbus.rs      the shared bus connections
src/sources/clock.rs     the timerfd clock
src/sources/sway.rs      workspaces, mode, title, scratchpad
src/sources/display.rs   the outputs
src/sources/pulse.rs     the sound server connection
src/sources/audio.rs     the default output and input
src/sources/mpris.rs     the media player
src/sources/battery.rs, network.rs, bluetooth.rs, brightness.rs,
  powerprofile.rs, nightlight.rs     the native readers
src/sources/pomodoro.rs  the pomodoro and the countdowns
src/sources/notify.rs    the bar's own notifications
src/sources/notifyd.rs   the notification daemon
src/sources/tray.rs      the StatusNotifier watcher and host
src/sources/picker.rs    desktop entries, windows, launching and focusing
src/ui/mod.rs            GTK, the monitors, the store
src/ui/store.rs          variables and their subscribers
src/ui/bar.rs            one output: the bar, the modules, the card
src/ui/calendar.rs       the month grid
src/ui/tray.rs           the tray icons and menus
src/ui/picker.rs         the picker window and the frecency
src/ui/notify.rs         the notification cards
config/                  the example configuration and style
systemd/                 the user unit
```

## License

MIT.
