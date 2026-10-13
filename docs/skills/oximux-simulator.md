---
name: oximux-simulator
description: |
  Drive the iOS simulator, Android emulator or phone attached to your
  worktree in the OxiMux desktop app's Mobile Emulator panel: build, install and launch the app, take
  screenshots, read the accessibility tree, tap, type and swipe, then check the
  result. Activate when the task involves an iOS or Android app, a simulator or
  emulator, `xcodebuild` for a simulator, or `oximux sim`.
---

# The Mobile Emulator panel

The OxiMux desktop app streams an iOS simulator or Android device in its right sidebar, one
device per worktree. Its `sim` verbs let you see and drive that device: build
your app, install it, launch it, look at the screen, act, and look again.

Read `oximux-cli` first for the exit codes and `--json`; this guide adds the
simulator verbs.

## Before you start

- These verbs are served by the **desktop app** on an Apple silicon Mac with
  Xcode. A headless `oximux serve` has no simulator (`unsupported`).
- The desktop's local CLI access must be on (Settings › Remote). Exit 3 means
  the app is not reachable.
- Every verb works on the worktree you run it from; `--worktree DIR` names
  another. Run `oximux sim status` to see what you have:

```sh
oximux sim status
oximux sim status --json
```

No device yet? Attach one (it boots if needed). A name or udid picks one; none
picks automatically. OxiMux shows the Mobile Emulator panel by itself when you
attach, run a verb, or build for a simulator — you do not need to ask the user
to open it:

```sh
oximux sim devices
oximux sim attach
oximux sim attach "iPhone 17 Pro"
```

## The user decides first

The first verb that looks at or touches the device (a screenshot, the AX tree,
a tap, an install…) asks the user in OxiMux's Mobile Emulator panel and **exits 7 at
once** — it does not wait. Then:

1. Tell the user OxiMux is asking them to allow agents on the simulator.
2. Wait for the answer, then retry the verb:

```sh
oximux sim wait-consent
oximux sim wait-consent --max-wait 60
```

`wait-consent` exits 0 once allowed, **5 if the user said no** (stop: do not
ask again until the cooldown in the message has passed, and tell the user what
you needed), and 4 if nobody answered in time. The answer covers that device
from then on. Exit 5 with `agent-control-off` means the user turned agent
control off in Settings — do not work around it.

Consent covers these verbs. Do not reach for `xcrun simctl` to look at or drive
the screen instead: that goes around the user's decision.

## The loop: build → install → launch → look → act → check

```sh
UDID=$(oximux sim status --json | jq -r .data.device.udid)
xcodebuild -scheme MyApp -destination "platform=iOS Simulator,id=$UDID" -derivedDataPath build build
oximux sim install build/Build/Products/Debug-iphonesimulator/MyApp.app
oximux sim launch com.example.MyApp --relaunch --settle
oximux sim ax -i
oximux sim tap @e4 --settle
oximux sim screenshot
```

- `install` takes a built `.app` inside your worktree or Xcode's DerivedData
  (`~/Library/Developer/Xcode/DerivedData`); anything else is refused (exit 5).
- `screenshot` saves a PNG under `$TMPDIR/oximux-sim/` and prints its path;
  open it to look. Keep that default: OxiMux hides those files from a phone
  mirroring your chat, but not a copy saved elsewhere with `--out PATH`.
- A bare verb returns once the input is sent, before the app has reacted.
  Add `--settle` (below) instead of `sleep`: it waits for the screen to go
  idle and prints what changed.

## Act and see what changed: `--settle`, refs, `wait`, `assert`

`--settle` on `tap`, `swipe`, `type`, `button`, `rotate`, `launch` and
`open-url` waits for the screen to stop changing, then prints the elements
that appeared (`+`) and went (`-`), each with an `@eN` ref:

```sh
oximux sim tap --label "Sign In" --settle
# ok tapped “Sign In” (201,612) settled 640ms
# + @e31 TextField “Email”
# + @e32 SecureTextField “Password”
# = 12 unchanged
```

- A simulator says `settled`; Android and a real iPhone say `settled (frame)`
  (their accessibility reads are slow, so one read follows the screen going
  quiet).
- The wait is capped (5 s on a simulator, 6 s on Android or an iPhone, 8 s
  for `launch` and `open-url`; `--settle-timeout MS` changes it). When the
  cap runs out the verb still exits 0 and says why:
  `settle: timeout (frame-busy)` (the screen kept moving: a spinner, a video)
  or `settle: timeout (ax-unstable)`, followed by the diff so far.

**Refs.** `ax -i` lists only what can be acted on, each with a ref; a ref also
appears on every `+` line of a settle. Use it instead of coordinates:

```sh
oximux sim ax -i
oximux sim tap @e31
oximux sim type @e31 "hello@example.com"
oximux sim type @e32 "secret" --settle
```

- A ref keeps its number while its element is on screen, across settles and
  across the device sleeping and waking. New elements get new numbers; a
  number is never reused for another element. When look-alikes (same role
  and name, e.g. a list's "Delete" buttons) come or go, all of them get new
  numbers: the old ones exit 1 with `stale-ref` rather than tap a twin.
- A tap by ref finds the element where it is now. If it moved more than 8
  points you get a `moved` warning (exit 0).
- An element that is gone, or can no longer be told apart from a look-alike,
  exits 1 with `stale-ref`: run `oximux sim ax -i` again.
- Refs are your own when you run inside an OxiMux agent session; other agents
  on the same worktree number their own.

**Wait and check, no sleeping:**

```sh
oximux sim wait --label "Welcome" --timeout-ms 8000
oximux sim wait @e7 --gone
oximux sim assert --id title --text "Home"
oximux sim assert @e12 --enabled
oximux sim assert --label "Error" --gone
```

`wait` exits 0 once the element appears (or, `--gone`, goes away) and 4
(`timeout`) when time runs out (default 10000 ms, at most 60000). `assert`
checks once: exit 0, or 1 (`assert-failed`) with what it found. `--text`
compares a field's value, else the element's label, exactly (surrounding
spaces aside).

## Several steps at once: `run`, flows, export

A file of steps runs in order with one command, each exactly as if typed:

```sh
cat > login.flow <<'FLOW'
# oximux-flow v1
launch com.example.app --relaunch --settle
tap --label "Sign in" --exact --settle
type "ada@example.com"
assert --label "Welcome" --exact
FLOW
oximux sim run login.flow           # stops at the first step that fails
oximux sim run login.flow --from 3  # resume there (e.g. after exit 7)
oximux sim run login.flow --settle  # settle after every action
```

- One step per line, without the leading oximux sim; `#` lines are comments; `-` as
  the file reads the steps from stdin. Every line
  is checked before step 1 runs, so a typo never leaves a half-driven app.
- A failing step stops the run with that step's exit code and says
  `step N failed: <line>`. `run`, `flow` and `wait-consent` cannot be steps.
- Steps that change device state (`privacy`, `install`, `erase`, `push`,
  `location`) are shown to the user once, as a list, before step 1; if they
  say no, nothing runs (exit 5). `erase`, `push` and a real device's settings
  still ask when their turn comes.
- `${SECRET_1}`, `${SECRET_2}`, … in a step are read from the environment
  (a recorded password is `type "${SECRET_1}"`): set them before the run. No
  other variable is read; `$${` writes a literal `${`.

**Exact selectors.** `--label` also matches part of a label ("Settings" finds
"Return to Settings"). `--exact` matches the label or id exactly; `--role R`
and `--nth N` (from 0, in `ax` order) pick among look-alikes:

```sh
oximux sim tap --label "Sign in" --exact
oximux sim tap --label "Delete" --exact --nth 1
oximux sim assert --role Image --nth 0
```

**Record your steps.** `oximux sim flow start` records the verbs you send on
this worktree's device; `flow status` shows the steps so far; `flow stop`
saves them in OxiMux's data and prints the path (`--out FILE` writes them
where you say instead). Refs become exact selectors in the file, never
`@eN`. The user can record their own taps and typing from the panel (Device
menu › Record flow); you cannot start, read or stop that recording.

**Export.** `oximux sim flow export login.flow --maestro --out flow.yaml`
writes Maestro YAML (to stdout without `--out`; `--app-id ID` when the file
has no `# app` line and no launch). Steps with no counterpart (rotation,
installs, text checks) stay in the file as `# unsupported: <step>` and are
counted on stderr; app data is never cleared. Point steps need the file's
`# screen WxH` line (a recording writes it). Check the YAML with the user's
own Maestro version.

## Coordinates are points

`tap`, `swipe` and the `ax` frames all use **points**, from the top-left of the
screen in its current orientation. A default screenshot has **one pixel per
point**, so a position you read off the image is the coordinate to tap:

```sh
oximux sim tap 201 437
oximux sim swipe 201 700 201 200 --duration 400
```

`screenshot --full` gives the device's full resolution instead and prints its
scale (pixels per point): divide by it before tapping.

Prefer elements over coordinates when they have a label or an identifier — it
survives layout changes:

```sh
oximux sim ax --flat
oximux sim tap --label "Continue"
oximux sim tap --id login-button
```

A label matches exactly first, then as a case-insensitive substring.

## Typing, buttons, rotation, URLs

Tap a field first, then type. ASCII is typed key by key; anything else (and
`--paste`) goes through the device's clipboard:

```sh
oximux sim tap --label "Email"
oximux sim type "hello@example.com"
oximux sim type "Xin chào" --paste
oximux sim button home
oximux sim button lock
oximux sim button app-switcher
oximux sim rotate landscape-left
oximux sim rotate portrait
oximux sim open-url "https://example.com"
oximux sim open-url "myapp://settings"
```

`home` is the swipe-up gesture on Face ID devices and the button on the rest.
`open-url` takes `http(s)` and app schemes, never `file:`.

## Device state: location, appearance, privacy, push, status bar, erase

Change the device instead of shelling out to `simctl` or `adb`:

```sh
oximux sim location set 10.77 106.70        # latitude, then longitude
oximux sim location set -33.86 151.21       # south and west are negative
oximux sim location clear                   # a simulator only
oximux sim appearance dark
oximux sim privacy grant photos --app com.example.app
oximux sim privacy reset location --app com.example.app
oximux sim push --app com.example.app notification.json
oximux sim status-bar clean                 # 9:41, full battery: for screenshots
oximux sim status-bar clear
oximux sim erase                            # wipe and boot again
```

- `privacy` always names one app (`--app`); nothing changes for every app at
  once. The app may restart. A grant can hide a missing usage description in
  the app's Info.plist: check the real prompt before shipping. A simulator
  cannot grant `camera` or `notifications`: tap the system alert instead.
- `push` takes a JSON file: an object with `aps`, at most 4096 bytes.
- `erase` deletes every app and its data. It is asked in the panel every
  time, as is `push`; a device the user booted is refused without `--force`.
- What each device takes: a simulator, everything; an Android emulator,
  everything but `push`; an Android phone, `appearance`, `privacy` and
  `status-bar`, each asked in the panel every time; a real iPhone, none. A
  gap exits 5 (`refused`) and says why; nothing is ever skipped silently.
- An Android device's demo-mode status bar goes back to normal when the last
  worktree attached to it detaches, and when OxiMux quits.

## Logs and crashes

Read what the app said, or why it died, instead of running `log stream` or
`adb logcat` yourself. Each is one bounded read, never a stream:

```sh
oximux sim logs                                    # the last 30s, 200 lines
oximux sim logs --app com.example.app --level error --since 5m
oximux sim logs --grep "checkout" --lines 500
oximux sim crashes                                 # the last 10m
oximux sim crashes --app com.example.app --since 1h
```

- `--since` takes `30s`, `5m` or `1h` (at most `1h`); `--lines` at most 2000,
  and the output is cut to 64 KiB, newest kept. A slow read (over 30 s) fails:
  narrow `--since` or add `--app`. A simulator's whole log (no `--app`, no
  `--grep`) is read at most `10m` back (`5m` with `info` or `debug`).
- `--level error` keeps errors and faults; `info` and `debug` add more.
- `crashes` lists each crash with its exception, its reason and the top 8
  frames of the crashing thread, then `warnings:` — lines that warn of trouble
  short of a crash (an ANR, an unhandled promise rejection). A simulator's
  crash report is written a few seconds (up to ~20 s) after the app dies;
  until then `crashes` lists the crash without frames and says so — run it
  again for them. At most the newest 20 crashes come back (a note says how
  many older ones were left out).
- A real Android phone is read one app at a time: `--app` is required for
  both (exit 2 without it). A real iPhone has neither (exit 5).
- Secret-looking values (API keys, tokens, `token=…`) are replaced with
  `[redacted]` before you see them.

## Android

The same verbs drive an Android emulator (an AVD) or a phone with USB
debugging. Android devices are listed beside the simulators with ids of their
own — `avd:<name>` for an emulator, `adb:<serial>` for a phone — and a name
that exists on both platforms is narrowed with `--platform`:

```sh
oximux sim devices --platform android
oximux sim attach "Medium Phone" --platform android
oximux sim attach avd:Medium_Phone
```

What differs from iOS:

- Coordinates are **dp** (Android's points): a default screenshot has one
  pixel per dp, like iOS.
- `install` takes an `.apk` inside your worktree
  (`app/build/outputs/apk/debug/app-debug.apk`); `launch` takes the package
  name (`com.example.app`).
- `type` sends any text as text — no `--paste` needed.
- Android's own buttons:

```sh
oximux sim button back
oximux sim button app-switcher
oximux sim button volume-up
oximux sim button volume-down
```

- `shutdown` stops an emulator OxiMux booted (with `--force`, one the user
  started). A phone is never shut down (exit 5).

## When something fails

| exit | code | what to do |
|---|---|---|
| 7 | `consent-pending` | tell the user, `oximux sim wait-consent`, retry |
| 5 | `consent-denied` | stop; tell the user what you needed |
| 5 | `agent-control-off` | the user turned it off; do not work around it |
| 5 | `path-outside-worktree` | build into the worktree or DerivedData |
| 5 | `refused` | not without the user asking (e.g. shutting down a device they booted, or booting one they shut down) |
| 1 | `unavailable` | no Xcode, the device is still booting, or the user turned the Mobile Emulator off in Settings; the message says which |
| 1 | `no-device` | `oximux sim attach` |
| 1 | `not-streaming` | the device is starting; retry in a few seconds |
| 1 | `not-found` | `oximux sim ax` to see what is on screen |
| 1 | `stale-ref` | the ref's element is gone; `oximux sim ax -i` for fresh refs |
| 1 | `assert-failed` | the message says what was found instead |
| 4 | `timeout` | `wait` ran out; `oximux sim ax -i` shows what is on screen |
| 2 | `bad-input` | the message says what was wrong |
| 3 | `unreachable` | the desktop app is not running, or local CLI access is off |

## Limits

- No volume buttons, no Touch ID / Face ID matching, no hardware keyboard
  shortcuts beyond typing text.
- Verified with Xcode 26. Any other Xcode version (older or newer) is
  best-effort: it is not blocked, but the helper may fail on it. If the
  sim verbs keep failing with a helper error there, tell the user to
  select Xcode 26. The Mobile Emulator panel shows the exact command for an
  Xcode 26 it finds; the general form, with their own app path, quoted, is
  `sudo xcode-select -s '/Applications/Xcode.app/Contents/Developer'`.
- `--settle`, `ax -i`, `wait`, `assert`, a tap by ref, `run` and `flow`
  print what is on screen, like `ax`: OxiMux hides their output from a phone
  mirroring your chat.
- Screenshots and the AX tree go to your model provider. The user was told when
  they allowed it; do not sign in to real accounts on the simulator.
- `oximux sim shutdown` refuses a device the user booted themselves. Do not add
  `--force` unless the user asked you to shut it down.
- A device the user shut down from the panel stays down: verbs exit 5
  (`refused`). Ask the user before running `oximux sim attach` to boot it again.
