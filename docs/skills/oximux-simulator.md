---
name: oximux-simulator
description: |
  Drive the iOS Simulator attached to your worktree in the OxiMux desktop app:
  build, install and launch the app, take screenshots, read the accessibility
  tree, tap, type and swipe, then check the result. Activate when the task
  involves an iOS app, the iOS Simulator, `xcodebuild` for a simulator, or
  `oximux sim`.
---

# The iOS Simulator

The OxiMux desktop app streams an iOS Simulator in its right sidebar, one
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
picks automatically. OxiMux shows the Simulator panel by itself when you
attach, run a verb, or build for a simulator — you do not need to ask the user
to open it:

```sh
oximux sim devices
oximux sim attach
oximux sim attach "iPhone 17 Pro"
```

## The user decides first

The first verb that looks at or touches the device (a screenshot, the AX tree,
a tap, an install…) asks the user in OxiMux's Simulator panel and **exits 7 at
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
oximux sim launch com.example.MyApp --relaunch
oximux sim screenshot
oximux sim ax
oximux sim tap --label "Sign In"
sleep 1
oximux sim screenshot
```

- `install` takes a built `.app` inside your worktree or Xcode's DerivedData
  (`~/Library/Developer/Xcode/DerivedData`); anything else is refused (exit 5).
- `screenshot` saves a PNG under `$TMPDIR/oximux-sim/` and prints its path;
  open it to look. Keep that default: OxiMux hides those files from a phone
  mirroring your chat, but not a copy saved elsewhere with `--out PATH`.
- Verbs return once the input is sent. The app then animates: wait a moment
  (`sleep 1`) before the screenshot that checks the result.

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

## When something fails

| exit | code | what to do |
|---|---|---|
| 7 | `consent-pending` | tell the user, `oximux sim wait-consent`, retry |
| 5 | `consent-denied` | stop; tell the user what you needed |
| 5 | `agent-control-off` | the user turned it off; do not work around it |
| 5 | `path-outside-worktree` | build into the worktree or DerivedData |
| 5 | `refused` | not without the user asking (e.g. shutting down a device they booted, or booting one they shut down) |
| 1 | `unavailable` | no Xcode, the device is still booting, or the user turned the iOS Simulator off in Settings; the message says which |
| 1 | `no-device` | `oximux sim attach` |
| 1 | `not-streaming` | the device is starting; retry in a few seconds |
| 1 | `not-found` | `oximux sim ax` to see what is on screen |
| 2 | `bad-input` | the message says what was wrong |
| 3 | `unreachable` | the desktop app is not running, or local CLI access is off |

## Limits

- No volume buttons, no Touch ID / Face ID matching, no hardware keyboard
  shortcuts beyond typing text.
- Xcode 26 is required; Xcode 27 is best-effort.
- Screenshots and the AX tree go to your model provider. The user was told when
  they allowed it; do not sign in to real accounts on the simulator.
- `oximux sim shutdown` refuses a device the user booted themselves. Do not add
  `--force` unless the user asked you to shut it down.
- A device the user shut down from the panel stays down: verbs exit 5
  (`refused`). Ask the user before running `oximux sim attach` to boot it again.
