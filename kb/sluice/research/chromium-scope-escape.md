---
title: "Chromium moves itself into a systemd scope (research for ADR-0028)"
tags: [sluice, research, containment, cgroup, chromium, playwright]
---

# Chromium moving itself into `app-org.chromium.Chromium-<pid>.scope`

Research date 2026-11-06. Read-only: sources fetched over HTTP, plus `strings`-style
`grep -a` on Playwright binaries already on disk (no browser was launched; the only
execution was `chrome-headless-shell --version`, which prints
`Google Chrome for Testing 156.0.8078.4` and exits).

Playwright chromium-headless-shell r1248 = Chromium **156.0.8078.4**
(https://raw.githubusercontent.com/microsoft/playwright/a47fcf59c8/packages/playwright-core/browsers.json).
r1254 (current main of Playwright) = 158.0.8127.18.

## 1. Which code does it, and why

Files (Chromium `components/dbus/xdg/`):

- `systemd.cc` / `systemd.h`: `dbus_xdg::internal::SetSystemdScopeUnitNameForXdgPortal(dbus::Bus*, ..., SystemdUnitCallback)`
  https://chromium.googlesource.com/chromium/src/+/refs/tags/156.0.8078.4/components/dbus/xdg/systemd.cc
  (main: https://chromium.googlesource.com/chromium/src/+/main/components/dbus/xdg/systemd.cc)
- `portal.cc`: `PortalRegistrar::Request()` calls it; public entry point is
  `dbus_xdg::RequestXdgDesktopPortal(bus, [interface_name,] callback)`.
  https://chromium.googlesource.com/chromium/src/+/refs/tags/156.0.8078.4/components/dbus/xdg/portal.cc
- Unit name: `kUnitNameFormat[] = "app-$1-$2.scope"` with `$1` = `EscapeSystemdUnitName(version_info::nix::GetAppName(env))`,
  `$2` = `getpid()`. `GetAppName` (`base/version_info/nix/version_extra_utils.cc`) returns
  `org.chromium.Chromium` (non-branded) or `com.google.Chrome` (branded), plus `.beta/.unstable/.canary`
  suffix from env `CHROME_VERSION_EXTRA`. So Playwright's build gives `app-org.chromium.Chromium-<pid>.scope`.

Introducing commit: `76320b4e3c` "Add dbus_xdg::SetSystemdScopeUnitNameForXdgPortal()", 2024-11-26,
Cr-Commit-Position main@{#1388436}, review https://chromium-review.googlesource.com/c/chromium/src/+/6049902,
bugs 40759171 and 40086962 (author Thomas Anderson). Stated reason (commit message):

> xdg-desktop-portal obtains the application name from the systemd unit name (except in Flatpak or Snap environments).
> The name is passed as an ID to backends like Secret and GlobalShortcuts, so it's effectively required to set a systemd
> unit name for those, otherwise it will use an empty string as the ID ... Also, if using a desktop environment like GNOME
> or KDE, a systemd scope gets created when spawning via the application launcher. Child processes also inherit the scope,
> so if the app launcher creates a terminal which the user launches the browser with, then the browser will incorrectly get
> the terminal name.

So yes: purpose is the xdg-desktop-portal app id. (Commit via GitHub mirror API: `gh api repos/chromium/chromium/commits/76320b4e3c`.)

Later history (same file): `1116db1d40` cache results (2024-12), `39b45f2608` check systemd service via NameHasOwner
first (2024-12), `2667c345f7` wait for unit activation (2025-01), `ad6db6f3e0` "[XDG] Add portal initialization helper"
= `RequestXdgDesktopPortal()` (2025-11-26, main@{#1550159}), `7becfc78c4` escape unit name (2026-07),
`5b7b7af1c5` shared Flatpak/Snap detection helper `base::nix::GetAppSandbox()` (2026-10-14, main@{#1718239}, bug 537480522),
and **`436f4a77eb` "[Linux] Move child processes into the XDG portal systemd scope"** (2026-11-05, main@{#1736615},
bugs 580525223 fixed, 528177806, 537480522). That last commit message:

> SetSystemdScopeUnitNameForXdgPortal() creates app-<name>-<pid>.scope with only the browser PID. It runs after the zygotes,
> the GPU process, renderers and utility processes have been spawned, so those stay in the scope of whatever launched the
> browser ... Create the scope with Delegate=true and, once it is active, scan /proc ... for descendants that are still in a
> different cgroup and attach them with AttachProcessesToUnit(), one PID at a time ... (at most 3 passes) ... If
> AttachProcessesToUnit() is unavailable (systemd older than v238), the behavior is unchanged. Tested by launching chrome with
> `systemd-run --user --scope --unit=launcher.scope` and stopping that scope ...

Consequence: Chromium 156 (Playwright r1248) moves ONLY the browser PID, exactly as the probe saw
("For now, only add this process to the new scope. It's possible to add all PIDs in the process tree, but there's currently
not a benefit." in 156's systemd.cc). From the 2026-11-05 commit onward (M16x, not yet in 156/158 stable as far as can be
told, UNCERTAIN) Chromium moves the *whole existing process tree* too, and the scope is `Delegate=true`. That is worse for
a job runner: even renderers/zygotes forked earlier leave the job cgroup.

Who triggers it at runtime (156 and main): anything that calls `RequestXdgDesktopPortal` on the shared session bus. The one
that runs unconditionally in every content-based browser process incl. headless shell is the power monitor:
`PowerMonitorDeviceSourceLinux` ctor (`components/power_monitor/power_monitor_device_source_linux.cc`) does
`dbus_xdg::RequestXdgDesktopPortal(session_bus_.get(), ...)`; it is created from
`MakePowerMonitorDeviceSource()` in `content/app/content_main_runner_impl.cc` ("PowerMonitor is needed in reduced mode") and
`content/browser/browser_main_loop.cc`. Other callers on main: PortalSettingsLinux (`ui/linux`), select-file portal,
secret portal key provider, print dialog portal, geolocation portal, notifications, proxy resolver, eye dropper, etc.
Binary check: both Playwright chromium_headless_shell-1228 and -1248 contain the string `StartTransientUnit` and
`org.freedesktop.systemd1`; 1248 contains no `AttachProcessesToUnit` (consistent with browser-PID-only).

## 2. Conditions under which it does NOT move

There is no `base::Feature` and no command-line switch gating it (no `--disable-features=` knob; none found in
`systemd.cc`, `portal.cc`, `power_monitor_device_source_linux.cc`). It is also not gated on headless mode, `--no-sandbox`, or on
the current cgroup (it never checks whether it is already in an `app-*.scope`; it always creates a new one,
`mode = "replace"`). The only skips:

a) **No session bus**. `content/app/content_main.cc` (156 tag):
```
// ... When this variable is not set (test environments like xvfb-run), the dbus client library will fall back to
// auto-launch mode. ... This one line disables the dbus auto-launch, by clobbering the DBUS_SESSION_BUS_ADDRESS env
// variable if not already set. The old auto-launch behavior, if needed, can be restored by setting
// DBUS_SESSION_BUS_ADDRESS="autolaunch:" before launching chrome.
const int kNoOverrideIfAlreadySet = 0;
setenv("DBUS_SESSION_BUS_ADDRESS", "disabled:", kNoOverrideIfAlreadySet);
```
   `dbus::Bus::Connect()` (`dbus/bus.cc`) uses `dbus_bus_get_private(DBUS_BUS_SESSION)`, i.e. libdbus honours that variable.
   So with `DBUS_SESSION_BUS_ADDRESS` **unset** (or `disabled:` / any unusable address) Chromium cannot reach the user
   manager, `NameHasOwner(org.freedesktop.systemd1)` fails and the callback gets `kNoSystemdService`: no scope is created.
   With it set (the normal case inside a systemd user session: `unix:path=/run/user/<uid>/bus`) it connects.
   (Strings `disabled:` present in the 1248 binary.) This is the cleanest, version-independent switch.
   Side effects: no portals, no notifications, no D-Bus power/settings integration; errors "Failed to connect to the bus"
   logged. Harmless for headless test runs.
b) **Flatpak/Snap detection** (callback `kUnitNotNecessary`):
   - 156 (`systemd.cc`): `if (env->HasVar("FLATPAK_SANDBOX_DIR") || env->HasVar("SNAP")) {...}` (env var presence only, even empty).
   - main since 2026-10-14 (`base/nix/xdg_util.cc` `DetectAppSandbox`): `/.flatpak-info` exists, or `FLATPAK_ID` non-empty ->
     kFlatpak; `SNAP` and `SNAP_NAME` both non-empty -> kSnap. `FLATPAK_SANDBOX_DIR` NO LONGER counts. The commit rationale:
     "Environment variables alone are not relied on, since they may be inherited by host processes launched from a sandboxed app."
     So the `FLATPAK_SANDBOX_DIR` trick is version-fragile (works on 156, likely not on main/new majors; exact first major
     containing 5b7b7af1c5 not determined). It may also alter other Chromium Flatpak-aware behaviour (UNCERTAIN).
c) `getpid() <= 1` -> `kInvalidPid` (irrelevant; a PID-namespaced browser as init).
d) Failure paths: `StartTransientUnit` error reply (e.g. user manager refuses, "Permission denied" seen with snaps in
   https://bugs.launchpad.net/ubuntu/+source/snapd/+bug/1961898) -> `kFailedToStart`, process stays where it is.
e) systemd < 238 only affects the new descendant migration (`AttachProcessesToUnit`).
Env `CHROME_VERSION_EXTRA` only changes the scope name; it does not disable.

## 3. How, and is it synchronous

1. On the dbus thread (`dbus_thread_linux::GetSharedSessionBus()`, a PRIVATE session-bus connection): `NameHasOwner("org.freedesktop.systemd1")`.
2. `org.freedesktop.systemd1.Manager.StartTransientUnit(name="app-org.chromium.Chromium-<pid>.scope", mode="replace",
   properties=[("PIDs", au [getpid()])] (+ ("Delegate", b true) on main), aux=[])` on object `/org/freedesktop/systemd1`
   of the **session bus** (= the per-user systemd manager). systemd moves the PID into the new cgroup
   `.../app.slice/app-org.chromium.Chromium-<pid>.scope`; the slice is the default `app.slice` for user scopes.
3. Reply -> `Manager.GetUnit(name)` -> watch `org.freedesktop.systemd1.Unit` `ActiveState` until active.
4. main only: scan `/proc` on a thread pool for descendants whose `/proc/<pid>/cgroup` differs from the browser's, and call
   `Manager.AttachProcessesToUnit(unit, "", [pid])` per PID, parents first, up to 3 passes.

Not synchronous with respect to browser startup: all of it is asynchronous D-Bus callbacks. The browser keeps running
and spawns zygote/GPU/utility/renderers meanwhile (they were forked from the old cgroup in 156; that is the "renderers forked
before the move stayed behind" the probe saw). Anything forked after the move inherits the new scope. The window is tens to
hundreds of ms; not deterministic.

## 4. Firefox and WebKit

- **Firefox (Playwright firefox-1532 on disk)**: `grep -a` of `libxul.so` and `firefox` finds no `StartTransientUnit`,
  `AttachProcessesToUnit` or `org.freedesktop.systemd1` strings. Mozilla bugs found (1982733 Taskbar Tabs, 1793303,
  1746771) do not describe a Firefox-created systemd scope. Not able to run searchfox or GitHub code search (JS-only /
  rate limited), so the source-level claim is from the binary check only: Firefox does not call systemd itself (UNCERTAIN for
  newer builds than r1532; Playwright r1573 is Firefox 159).
  Evidence that Firefox gets scope names from the *launcher*, not itself: portal log "Assigning app ID "" to pid ... which has
  unit "app-firefox_firefox-<hash>.scope"" (https://bugzilla.mozilla.org/show_bug.cgi?id=1793303), Red Hat
  https://bugzilla.redhat.com/show_bug.cgi?id=2176759 (scope named by GNOME).
- **WebKit (Playwright webkit-2311, GTK and WPE MiniBrowser)**: `libwebkitgtk-6.0.so`, `libWPEWebKit-2.0.so`, `MiniBrowser`
  contain no `StartTransientUnit`/`systemd1`. WebKit spawns web/network processes via its own launcher (bwrap when sandboxed),
  not via scopes. (Source not read; binary evidence only.)
- **GLib `GDesktopAppInfo`** does move *launched* apps into transient scopes: commit f50e0b20 "gdesktopappinfo: Move launched
  applications into transient scope" (B. Berg, 2022-03, scope `app-glib-<appid>-<pid>.scope`, `StartTransientUnit` over the
  session bus). Only when an app is launched via `g_app_info_launch*`; neither Playwright browser does that. Relevant only if
  something launches through GIO, e.g. `xdg-open`/`gio open`.
  https://mail.gnome.org/archives/commits-list/2022-March/msg08094.html and
  https://fuchsia.googlesource.com/third_party/glib/+/refs/heads/upstream/benzea/systemd-transient-scope%5E%21/

## 5. Reports of this breaking containment

- https://github.com/haowjy/meridian-dev-workflow/pull/9 "agent-browser: run Chrome in a memory-capped systemd scope":
  "Chromium moves its *browser* process into its own `app-org.chromium.Chromium-<pid>.scope` (XDG portal path)" which leaves
  it uncapped; workaround "sets `FLATPAK_SANDBOX_DIR` to skip that move, so browser, GPU and renderers share the capped scope";
  author notes "This depends on Chromium internals" and verifies with `/proc/<pid>/cgroup`. Closest independent confirmation
  found (English only).
- Red Hat https://bugzilla.redhat.com/show_bug.cgi?id=1931938 (RFE for systemd-oomd): states all Chromium processes live in
  one scope; not about escaping.
- Snap/Launchpad https://bugs.launchpad.net/ubuntu/+source/snapd/+bug/1961898: "Failed to add PIDs to scope's control group:
  Permission denied" when the transient scope cannot be created (the Chromium side of the same mechanism).
- Chromium's own motivation for 436f4a77eb: processes left in the launcher's scope get killed when it stops
  (crbug.com/528177806, 580525223, 537480522; tracker pages were not fetchable, JS-only; only referenced from the commit).
- Searched Chinese (CSDN/Zhihu-style terms), Russian, Japanese, plus Bazel/GitHub Actions/Kubernetes/Flatpak terms: no
  report found about Chromium leaving a job cgroup in those languages or ecosystems. Only hits were generic systemd-run /
  MemoryMax how-tos and the Red Hat RFE. Absence of reports is not proof; the search engine was general-purpose (US-only
  web search), not site-specific (Zhihu/Habr/Qiita/Zenn were not directly queryable).
- Kubernetes/Docker: inside a container there is normally no session bus and no `DBUS_SESSION_BUS_ADDRESS`, so it silently
  falls into case 2a. Only machines with a user systemd session (desktop, `loginctl enable-linger` CI, self-hosted runners as
  a systemd user) are exposed.

## Implications for a job runner that must keep browsers inside its cgroup

1. Best lever: remove the session bus from the job environment. Unset `DBUS_SESSION_BUS_ADDRESS` (Chromium then sets
   `disabled:` itself) or set it to `disabled:`. Also do not leave `XDG_RUNTIME_DIR/bus` reachable through an inherited
   address. Works for any Chromium version including the new descendant-migration (the code path is never entered); does not
   depend on Chromium internals except the `content_main.cc` clobbering (present since 2017, crbug.com/715658). Caveat: a
   test that needs portals/notifications will lose them. Apply to the whole job env (sluice's cgroup leaf already runs with
   the user-manager env, so it inherits the address by default).
2. Defence in depth, not a replacement: `FLATPAK_SANDBOX_DIR=1` (works on 156, not on main after 2026-10-14) or `/.flatpak-info`
   (cannot be faked). `SNAP` env alone is not enough on main (needs `SNAP_NAME`) and may change other behaviour. Not
   recommended as primary.
3. Detection/containment fallback: since the move is asynchronous, poll or use a netlink/`cgroup.events`-style check is not
   possible across cgroups; instead, track the job by something other than the cgroup: e.g. at job end scan
   `/proc/*/cgroup` for `app.slice/app-*-<pid>.scope` where `<pid>` is a descendant of the job's root PID (use
   ppid chain / `PR_SET_CHILD_SUBREAPER` on the daemon so orphans re-parent to it, or put the job in its own PID namespace)
   and kill them (`systemctl --user stop` that scope or SIGKILL the tree). The scope's PIDs remain real children of the job,
   so a process-tree (ppid / subreaper) kill covers them even though `cgroup.kill` does not.
4. memory.max on the leaf will not see the browser once moved; accounting for the whole job must either avoid the move (1)
   or add the escaped scopes' `memory.current` (cgroup v2 `memory.stat` of `app-*.scope`) to the job's total.
   Note: newer Chromium creates the scope with `Delegate=true`, so its subtree is writable by the browser user.
5. Do not rely on any Chromium feature/flag: none exists. Pin the strategy in a test: start a harmless Chromium under the
   daemon with and without `DBUS_SESSION_BUS_ADDRESS` and assert `/proc/<pid>/cgroup` (the probe already does the first half).
6. Firefox and WebKit builds from Playwright show no systemd calls, so they stay in the job cgroup; only Chromium-family
   (Chrome, Edge-like, Electron apps built on content/ if they initialise the power monitor on Linux) are affected. Electron
   is UNVERIFIED.

### Uncertain / unverified
- Which stable Chromium major first contains 436f4a77eb (descendant migration + Delegate=true) and 5b7b7af1c5 (FLATPAK_SANDBOX_DIR
  no longer honoured). Dates only: 2026-11-05 and 2026-10-14; 158.0.8127.18 likely predates the first.
- Whether `unset DBUS_SESSION_BUS_ADDRESS` alone suffices when `XDG_RUNTIME_DIR/bus` exists: the `content_main.cc` clobbering
  sets `disabled:` so libdbus does not fall back to that socket (code-read, not tested).
- Firefox/WebKit conclusions rely on `grep -a` of shipped binaries and not on source reads.
- crbug.com tracker entries were not retrievable; only the commit messages quote them.
