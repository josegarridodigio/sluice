---
title: "Containment spike S4: fallback matrix and detection"
tags: [sluice, research, containment, cgroups, systemd, spike]
---

# Containment spike S4: fallback matrix and detection

Spike S4 of [[sluice/plan/containment-plan.md]], feeding
[[sluice/design/adr/0017-agent-resource-containment.md]]. No AI sessions, no
product code. Machine: Ubuntu 26.04.1, kernel 7.0.0-38, systemd 259.5,
cgroup v2, 20 CPUs, user `jose` (uid 1000). Evidence is marked **measured**
(commands run here) or **documented** (source cited in section 8). Anything
neither measured nor documented is marked **unverified**.

## 1. Summary of findings

1. Detection must not parse names: `stat -fc %T /sys/fs/cgroup` prints
   `cgroup2fs` here but `UNKNOWN` in an Alpine (busybox) container whose
   mount is a perfectly good cgroup2. Use `statfs` `f_type` (0x63677270 =
   `CGROUP2_SUPER_MAGIC`) or the fstype column of `/proc/self/mountinfo`.
2. The user manager here delegates `cpu memory pids` (not `cpuset`, not `io`)
   and `user@1000.service` has `Delegate=pids memory cpu` plus
   `DelegateSubgroup=init.scope` (measured, `systemctl cat user@1000.service`).
   Our design needs only `cpu memory pids`.
3. A login `session-N.scope` process cannot move itself into the user manager's
   tree: `user-1000.slice/cgroup.procs` and `session-2.scope/cgroup.procs` are
   `root:root 0644` (measured; the write returned EACCES for both). The kernel
   rule is the cause (writer needs write access to the common ancestor's
   `cgroup.procs`, documented).
4. **But `AttachProcessesToUnit` on the user manager does move such a process**
   (measured; section 3.2). The user manager cannot write the file either; it
   falls back to asking the system manager over the system bus (documented in
   systemd's `unit_attach_pids_to_cgroup`). So the hook path works for
   session-scope agents, provided the system bus is reachable, and the
   self-move path works for everything forked after the agent was attached.
5. `nice` works unprivileged and is inherited, but only against siblings in the
   same cgroup. Between two cgroups it has no effect at all (measured: nice 0
   vs nice 19 in two sibling services got 1.50 s of CPU each in 3 s).
   `autogroup` is useless on a systemd-with-cpu-controller machine (measured:
   writing the autogroup nice is accepted and changes nothing, 50/50).
6. A delegated unit with `Delegate=memory pids` still gets a working `memory.high`
   and `pids.max` in its leaves, and its leaves have no `cpu.weight` file:
   "cpu available" is detected by file presence, not by guessing the distro.
7. Containers see a read-only cgroup2 mount and no user manager: Sluice must
   treat them as "uncontained, by design" with a clear doctor message, not as
   a failure.

## 2. Detection (what is measured here)

| Probe | Command | Result here |
|---|---|---|
| unified mode | `stat -fc %T /sys/fs/cgroup` | `cgroup2fs` |
| manager version | `systemctl --version` | `systemd 259 (259.5-0ubuntu3.4)` |
| user manager state | `systemctl --user is-system-running` | `running` |
| private socket | `ls -l $XDG_RUNTIME_DIR/systemd/private` | `srwx------ jose` (`/run/user/1000/systemd/private`) |
| session bus | `$DBUS_SESSION_BUS_ADDRESS` | `unix:path=/run/user/1000/bus,...` |
| controllers of the user manager | `.../user@1000.service/cgroup.controllers` | `cpu memory pids` |
| controllers of root | `/sys/fs/cgroup/cgroup.controllers` | `cpuset cpu io memory hugetlb pids rdma misc dmem` |
| enabled down the chain | `subtree_control` of `user.slice`, `user-1000.slice`, `user@1000.service` | `cpu memory pids` at each level |
| delegation setting | `systemctl cat user@1000.service` | `Delegate=pids memory cpu`, `DelegateSubgroup=init.scope` |
| admin drop-ins | `/etc/systemd/system/user@.service.d` | absent; vendor drop-ins: `10-login-barrier.conf`, `10-oomd-user-service-defaults.conf`, `timeout.conf` |
| caller's cgroup | `/proc/self/cgroup` | `0::/user.slice/user-1000.slice/user@1000.service/session.slice/dbus.service` (under the user manager) |
| ownership | `ls -ld` | `user@1000.service/` and its `cgroup.procs`: `jose:jose`; `user-1000.slice/cgroup.procs` and `session-2.scope/cgroup.procs`: `root:root rw-r--r--` |
| autogroup | `/proc/sys/kernel/sched_autogroup_enabled`, `/proc/self/autogroup` | `1`, `/autogroup-17892 nice 0` |
| I/O scheduler | `/sys/block/nvme0n1/queue/scheduler` | `[none] mq-deadline` (so `ionice` classes do nothing here) |
| login sessions | `loginctl list-sessions` | session 2 (gdm, seat0/tty2) and the `manager` entry |

Processes of this user inside a login scope (measured with `systemd-cgls` and
`ps -o cgroup`): `gdm-wayland-session` (5313) and `gnome-session-init-worker`
(5334) in `session-2.scope`; everything else in `user@1000.service`. No SSH
login was possible (key auth to localhost denied), so the SSH case is
reproduced with the GDM session scope, which has the same cgroup shape
(`user-1000.slice/session-N.scope`, root-owned).

### 2.1 Probing the "can I write here" question without side effects

Do not infer from ownership alone (ACLs exist: `user@1000.service` shows the `+`
flag). Cheap and safe probe: `access(path, W_OK)` on
`<own cgroup>/cgroup.procs`, and for the common-ancestor rule, `access(W_OK)` on
the `cgroup.procs` of the nearest common ancestor of the caller's cgroup and the
target scope (for a session-scope caller this is `user-1000.slice`, not
writable). Doctor reports the result without ever moving anything.

## 3. The login-scope case, in detail

### 3.1 Self-move: impossible (measured)

```
$ sleep 60 &            # lives in .../user@1000.service/session.slice/dbus.service
$ echo $! > /sys/fs/cgroup/user.slice/user-1000.slice/cgroup.procs
permiso denegado  (rc=1)
$ echo $! > /sys/fs/cgroup/user.slice/user-1000.slice/session-2.scope/cgroup.procs
permiso denegado  (rc=1)
```

A shell that lives in `session-N.scope` (SSH, TTY, GDM) cannot write into a
cgroup of the user manager: source `session-N.scope` and destination
`user@UID.service/...` have `user-UID.slice` as nearest common ancestor, whose
`cgroup.procs` is root-owned. Kernel containment rule: the writer needs write
permission on the `cgroup.procs` of the common ancestor of source and target in
addition to the target ([kernel cgroup-v2 doc](https://docs.kernel.org/admin-guide/cgroup-v2.html),
[Heo's patch discussion](https://lkml.iu.edu/hypermail/linux/kernel/1612.0/03557.html)).
The ADR-0017 spike moved a shell that was already under `user@1000.service`
(`dbus.service`), where the common ancestor is the user-owned
`user@1000.service`; that is why it worked there and fails here.

### 3.2 `AttachProcessesToUnit` on the user manager: works (measured)

```
$ systemd-run --user --unit=sluice-s4-attach -p Delegate=yes sleep 120
$ busctl --user call org.freedesktop.systemd1 /org/freedesktop/systemd1 \
    org.freedesktop.systemd1.Manager AttachProcessesToUnit ssau \
    sluice-s4-attach.service / 1 5334        # 5334 was in session-2.scope
rc=0
$ cat /proc/5334/cgroup
0::/user.slice/user-1000.slice/user@1000.service/app.slice/sluice-s4-attach.service
```

Caveats measured in the same run:

- The target unit must be delegated and active: moving into a non-delegated
  unit returned `Process migration not available on non-delegated units.`
  (documented in `bus_unit_method_attach_processes`, reproduced here).
- No password or polkit prompt: the method is `SD_BUS_VTABLE_UNPRIVILEGED`; the
  checks are SELinux, delegation, active state and process ownership (sender
  uid must own the process).
- Why it works: `unit_attach_pids_to_cgroup` tries `cg_attach` itself; on a
  privilege error in a user instance it calls `AttachProcessesToUnit` on the
  system manager ("let's ask the system instance about it instead"). This needs
  the system bus (`/run/dbus/system_bus_socket`). It is therefore a hard
  dependency of the session-scope case: without a system bus the call fails
  and Sluice must fail open. **Unverified**: behaviour when the system bus
  socket is absent (not reproducible here); the doctor probe is
  `test -S /run/dbus/system_bus_socket`.
- Latency of this path was not measured (the plan's 0.17 ms figure is for the
  direct write). It involves two D-Bus round trips; treat it as milliseconds
  and keep it off the per-command hot path: the hook does it once per session.

### 3.3 Consequence for the design

Order matters. In a login-scope session:

1. SessionStart hook: attach the agent PID and its existing descendants to the
   `ui` leaf through the user manager (works via the system-manager fallback).
2. From then on the agent lives in the user manager's tree, so every shell it
   forks starts in `ui`; the `CLAUDE_ENV_FILE` self-move from that shell to
   `tools` now has `user@UID.service` as common ancestor and works.
3. A shell that is **still** in `session-N.scope` (agent not attached, other
   agents without a session hook, a user's own terminal running a shim) must not
   self-move: detect it from `/proc/self/cgroup` and skip silently, or ask the
   daemon to attach it through the user manager (a daemon call from the shim is
   an option for wave 2; not needed for Claude Code).

Side effect of this spike, for transparency: testing step 3.2 used a real
process of the desktop session (`gnome-session-init-worker`, idle in `poll`)
because no login shell was available. It cannot be moved back into
`session-2.scope` (unprivileged), so it was parked in `sluice-s4-park.service`
(`KillMode=none`, so stopping the unit does not kill it). It keeps running; it
disappears from the cgroup tree at the next logout/reboot. Harmless, but do not
repeat the test on desktop processes: use an SSH login.

## 4. Distribution matrix: does `user@.service` delegate `cpu`?

Upstream: commit
[`b8df7f8629`](https://github.com/systemd/systemd/commit/b8df7f8629) "user:
delegate cpu controller, assign weights to user slices" changed
`units/user@.service.in` from `Delegate=pids memory` to
`Delegate=pids memory cpu`. First release containing it: **systemd 252**
(the commit is dated 2022-07-05; 252 is the first stable release after it;
the commit touches no NEWS file). Versions per
[Repology](https://repology.org/project/systemd/versions):

| Distribution | systemd | cgroup default | `cpu` delegated to `user@` | Notes |
|---|---|---|---|---|
| Ubuntu 22.04 | 249.11 | v2 | **no** (`pids memory`) | needs admin drop-in; nice fallback |
| Ubuntu 24.04 | 255.4 | v2 | yes | |
| Ubuntu 25.10 | 257.9 | v2 | yes | |
| Ubuntu 26.04 (this machine) | 259.5 | v2 | yes, measured | |
| Debian 11 | 247.3 | v2 | **no** | 247 predates 252; whether it delegates even `memory` is unverified |
| Debian 12 | 252.39 | v2 | yes if Debian carries the 252 unit unmodified (**unverified**, the packaged unit was not inspected) | |
| Debian 13 | 257.13 | v2 | yes | |
| Fedora 39 / 40 / 41 / 42 / 43 / 44 | 254 / 255 / 256 / 257 / 258 / 259 | v2 | yes | |
| RHEL / Rocky / Alma 8 | 239 | **v1** | **no, and v1** | unsupported: no containment, nice only |
| RHEL / Rocky / Alma 9, CentOS Stream 9 | 252 | v2 | yes if Red Hat kept the 252 unit (**unverified**: Red Hat patches heavily) | RHEL 10 / Stream 10: 257, yes |
| Arch Linux | 262.1 | v2 | yes | |
| openSUSE Leap 15.5 | 249.17 | **hybrid (unverified)** | **no** | |
| openSUSE Leap 15.6 | 254.27 | hybrid by default (**unverified**) | yes if unified | check `stat -fc %T`; `systemd.unified_cgroup_hierarchy=1` |
| openSUSE Tumbleweed | 261.3 | v2 | yes | |
| WSL2 + systemd | the distro's own | historically hybrid (v1+v2), fix: `cgroup_no_v1=all` in the `.wslconfig` kernel command line ([spurin/wsl-cgroupsv2](https://github.com/spurin/wsl-cgroupsv2)); newer WSL builds may differ (**unverified**) | follows the distro (Ubuntu 22.04 image: no; 24.04: yes) | requires `[boot] systemd=true` in `/etc/wsl.conf` ([Microsoft docs](https://learn.microsoft.com/windows/wsl/wsl-config)); the user manager and its delegation then behave as on the distro |

Other facts:

- `cpuset` and `io` are **not** in the upstream default even at 259 (measured:
  `Delegate=pids memory cpu`). The plan's `cpuset` and `io` are not needed.
- cgroup v1 deprecation: systemd deprecated v1 in 256 and (to the best of my
  reading, **unverified**) dropped it in 258; hybrid/v1 is a shrinking target
  and Sluice treats it as unsupported (no containment, doctor says so).
- Distro overrides: none on this machine (no `/etc/systemd/system/user@.service.d`;
  vendor drop-ins do not touch `Delegate=`). Whether other distributions ship a
  `Delegate=` drop-in is **unverified**; the detection below reads the
  effective value (`systemctl show user@UID.service -p Delegate -p DelegateControllers`
  and the real `cgroup.controllers` file), so overrides in either direction are
  honoured without a distro list.

## 5. Containers, devcontainers, Docker-in-Docker

Measured in `alpine:latest` under Docker on this machine:

```
cgroupfs=UNKNOWN                      # busybox stat cannot name the magic
self=0::/                             # own cgroup namespace root
controllers=cpuset cpu io memory hugetlb pids rdma misc dmem
XDG= DBUS=                            # no user manager, no bus
ls /run/systemd -> No such file       
cgroup on /sys/fs/cgroup type cgroup2 (ro,nosuid,nodev,noexec,relatime,nsdelegate,...)
mkdir /sys/fs/cgroup/x     -> Read-only file system
echo $$ > /sys/fs/cgroup/cgroup.procs -> Read-only file system
```

What Sluice sees and does:

| Situation | Sees | Sluice does |
|---|---|---|
| Docker/devcontainer, default | cgroup2 mounted `ro`, `self=0::/`, no `$XDG_RUNTIME_DIR`, no systemd | uncontained by design. Do not try to write. The container's own limits (`--cpus`, `--memory`) are the containment; `cpu.max`/`memory.max` visible at the root of the namespace may be read for `doctor` information. Queue and cache still work. |
| Privileged container, or `--cgroupns=host`, or rw cgroup mount without systemd | cgroup2 writable | **not** used in 0.5.0: without a manager nobody owns the hierarchy and creating cgroups by hand races with the runtime. Report "writable cgroup but no systemd user manager; containment off". Revisit only on demand. |
| systemd inside the container (e.g. systemd-nspawn, systemd-in-docker, LXC with a user manager) | works as on the host if `user@` is delegated | normal detection path decides |
| Docker-in-Docker | inner daemon's containers are one more level of the same case; the outer container is as above | the agent runs in whichever container hosts it; nothing special |
| Claude Code Bash sandbox (bubblewrap) | `/sys/fs/cgroup` may be read-only inside | covered by spike S1; same fail-open result as a container |

`$XDG_RUNTIME_DIR` can also be unset on a real host (cron, `sudo -u`, an IDE
launched oddly): derive `/run/user/$UID` before concluding "no manager", and
only then mark it unreachable.

## 6. CPU fallback when `cpu` is not delegated

All numbers: two `python3` busy loops pinned with `taskset -c 19` (one CPU), 3 s
window, utime+stime ticks from `/proc/<pid>/stat` (CLK_TCK=100), machine under
other load but both loops share the same CPU. Script:
`share.py` in the session scratchpad (not committed).

### 6.1 `nice` on the shell (works unprivileged)

| Scenario | A (nice 0) | B | share A / B |
|---|---|---|---|
| same session/autogroup, 0 vs 0 | 150 | 150 | 50.0 % / 50.0 % |
| same autogroup, 0 vs nice 10 | 271 | 29 | **90.3 % / 9.7 %** |
| same autogroup, 0 vs nice 19 | 296 | 5 | **98.3 % / 1.7 %** |
| two sessions (`setsid`), 0 vs nice 19 | 296 | 4 | 98.7 % / 1.3 % |

Properties: unprivileged, one syscall, inherited by every child and survives
`exec`, cannot be undone by the child (lowering nice needs `CAP_SYS_NICE`;
`ulimit -e` is 0 here). Perfect for the "env-file line `renice`s the shell"
fallback. Cost of this fallback: it also lowers the shell's priority against the
agent's own UI process **only if they are in the same cgroup**; in a login
scope with `cpu` enabled in `user.slice` they are in different cgroups only
when the agent was moved, so for the fallback case the agent and its tools do
share a cgroup and the split works as intended.

### 6.2 `nice` across cgroups does nothing (measured)

Two transient services in the user manager, same CPU, one at nice 0 and one at
nice 19, both `cpu.weight` default:

```
sluice-s4-ga (nice 0 ) usage_usec 258455  -> 1763303   (+1 504 848)
sluice-s4-gb (nice 19) usage_usec 257483  -> 1763405   (+1 505 922)
```

Equal shares. Nice only orders tasks inside one cgroup; between cgroups the
group weight decides. Consequences:

- On a machine where `cpu` is enabled for the user tree but not delegated
  (unusual: needs an admin to enable `cpu` for `user.slice` and not delegate it),
  nice on the shell protects the agent's UI only against siblings in the same
  cgroup; it does not protect the desktop from a runaway agent. The user
  manager's own `session.slice`/`app.slice`/`background.slice` weights (the
  upstream 252 change) already keep session services ahead of apps there.
- On systemd < 252 the `cpu` controller is normally not enabled below
  `user.slice`, so all the user's processes are one flat group for the CPU
  scheduler and nice **does** work across them. This is the reasoning behind
  the ADR's "nice on the shell" fallback for Ubuntu 22.04. **Unverified here**
  (this kernel and manager always enable `cpu`); verify on a 22.04 box (E3).

### 6.3 `autogroup` (not useful)

`/proc/sys/kernel/sched_autogroup_enabled` is `1`; each process has
`/proc/<pid>/autogroup`. Writing an autogroup nice is allowed unprivileged for
non-negative values and is recorded, but changed nothing:

| Scenario (two `setsid` sessions, task nice 0 both) | share A / B |
|---|---|
| autogroup nice 10 on B | 50.0 % / 50.0 % (150 / 150) |
| autogroup nice 19 on B | 50.0 % / 50.0 % (150 / 150) |

Reason (documented in the kernel's autogroup design): autogroup applies only to
tasks whose task group is the root group; here every task lives in a
non-root cgroup with the `cpu` controller enabled, so autogroup is bypassed.
Where it could apply (cgroup `cpu` not enabled, e.g. systemd < 252), it is
per-session and coarse: it would renice every process of the login session
including the agent's own UI. Decision: do not use autogroup.

### 6.4 `ionice`

`/sys/block/nvme0n1/queue/scheduler` is `[none] mq-deadline`: the I/O priority
classes are only honoured by BFQ, so `ionice` is a no-op on most modern
setups. Keep the existing `nice`/`ionice` job wrapper (0.4.1) as is; do not
add `ionice` to the fallback and do not promise anything about I/O.

### 6.5 Only `memory pids` delegated (measured)

A unit created with `-p "Delegate=memory pids"`:

```
move the unit's main process to a sub-cgroup  rc=0
echo "+memory +pids" > <unit>/cgroup.subtree_control  rc=0   (shows: memory pids)
leaf: echo 500M > memory.high  rc=0
leaf: echo 100  > pids.max     rc=0
leaf: ls | grep -c cpu.weight  -> 0
```

So memory throttling (`memory.high`) and `pids.max` work without `cpu`, and the
leaf simply has no `cpu.weight`. Note `Delegate=memory pids` on a unit does not
hide `cpu` from `cgroup.controllers` when the parent still passes it; the
authority is the leaf's file list. Behaviour: create the same tree, skip
weights, apply `MemoryHigh` and `pids.max`, and put `nice` on the shell for CPU.
The container-less "no controllers at all" case (cgroup v1, no manager) leaves
only `nice`.

## 7. `sluice doctor`: checks and exact messages

Each check is independent, prints one line, and never fails the command. A
single `Contain` section; status words as in the existing doctor (`ok`, `warn`,
`info`). Proposed codes are stable identifiers for JSON output.

| # | Code | Condition | Level | Message |
|---|---|---|---|---|
| 1 | `cg-v2` | statfs of `/sys/fs/cgroup` is cgroup2 | ok | `cgroup v2 (unified hierarchy)` |
|   |  | `tmpfs` with `/sys/fs/cgroup/unified` | warn | `cgroup v1/hybrid: containment off (needs the unified hierarchy). Boot with systemd.unified_cgroup_hierarchy=1 or use a distribution that defaults to cgroup v2.` |
|   |  | not a cgroup mount | info | `no cgroup filesystem visible: containment off` |
| 2 | `cg-writable` | in a container/sandbox: mount is `ro` | info | `cgroup filesystem is read-only (container or sandbox): containment off; the runtime's own limits apply` |
| 3 | `systemd` | `systemctl --version` parses, major N | ok | `systemd N` |
|   |  | binary missing | info | `no systemd: containment off (nice fallback active)` |
| 4 | `user-manager` | `$XDG_RUNTIME_DIR` (or `/run/user/UID`) has `systemd/private` and `systemctl --user is-system-running` is `running` or `degraded` | ok | `systemd user manager reachable` |
|   |  | socket missing | warn | `systemd user manager not reachable (no $XDG_RUNTIME_DIR/systemd/private): containment off. Typical for containers, cron and su. Run inside a login session (loginctl enable-linger USER for headless machines).` |
|   |  | `is-system-running` = `starting`/`stopping`/`offline` | warn | `systemd user manager is <state>: retry later` |
| 5 | `delegate-cpu` | `user@UID.service/cgroup.controllers` contains `cpu` | ok | `cpu delegated to the user manager: CPU weights available` |
|   |  | not, and systemd < 252 | warn | `cpu is not delegated to user@.service on systemd N (< 252): CPU containment off, nice fallback active. Fix: admin drop-in, see docs (Delegate=cpu memory pids).` |
|   |  | not, and systemd >= 252 | warn | `cpu is not delegated although systemd N should (an override or a distribution patch removed it): check systemctl show user@UID.service -p Delegate` |
| 6 | `delegate-memory-pids` | `memory` and `pids` both present | ok | `memory and pids delegated: memory.high and pids.max available` |
|   |  | one missing | warn | `memory/pids not delegated (<which>): that limit is skipped` |
| 7 | `own-cgroup` | `/proc/self/cgroup` is under `user@UID.service` | ok | `this shell is inside the user manager's tree: it can place itself` |
|   |  | under `session-N.scope` | warn | `this shell is in login scope session-N.scope (SSH/TTY/GDM): it cannot move itself (cgroup rule). The agent is attached through the user manager at session start instead.` |
|   |  | elsewhere | info | `this shell is in <path>, outside the user manager: containment off for it` |
| 8 | `system-bus` | only if check 7 is session scope: `/run/dbus/system_bus_socket` exists | ok | `system bus reachable (needed to attach login-scope processes)` |
|   |  | missing | warn | `system bus not reachable: login-scope shells cannot be attached; those sessions run uncontained` |
| 9 | `slice` | `sluice.slice` / `sluice-agents.scope` exist and are delegated (`systemctl --user show -p Delegate`) | ok | `sluice.slice present (agents scope, gates scope)` |
|   |  | absent | info | `sluice.slice not created yet (created by the daemon on first use)` |
| 10 | `session` | per live session: agent PID's cgroup is its `ui` leaf | ok | `session <id>: contained (ui/tools leaves)` |
|    |  | otherwise | warn | `session <id>: NOT contained: <reason from checks 1-8>` where the reason is the first failing check in order 1,2,4,5,7,8 |
| 11 | `fallback` | CPU not contained and shell niced | info | `CPU fallback: shell niced to +N (applies only among processes of the same cgroup)` |
| 12 | `unprivileged-userns` | n/a | - | not checked in this spike |

Rules: if 5 fails but 1, 4, 6 pass, containment is **partial** (memory, pids);
the summary line says `containment: partial (memory, pids; cpu by nice)`.
If 1 or 4 fails: `containment: off (<reason>)`. All pass: `containment: full`.
Messages name the cause, the consequence and (when one exists) the fix, in
that order, like the existing doctor lines.

## 8. Detection algorithm

```
detect() -> Report { level: Full | Partial | Off, reasons: [Code], plan }

# 1. filesystem
fs = statfs("/sys/fs/cgroup").f_type            # never parse `stat -f %T` text
if fs != CGROUP2_SUPER_MAGIC (0x63677270):
    return Off(cg-v2: v1/hybrid or none)
if mountinfo("/sys/fs/cgroup").options contains "ro" and !access(own_cgroup_dir, W_OK):
    return Off(cg-writable: read-only)           # containers, bwrap

# 2. manager
uid  = getuid()
rt   = env(XDG_RUNTIME_DIR) or "/run/user/{uid}"
if !is_socket(rt + "/systemd/private") and !is_socket(rt + "/bus"):
    return Off(user-manager)
state = run("systemctl --user is-system-running")      # running|degraded ok
if state not in {running, degraded}: return Off(user-manager: state)
ver = parse(run("systemctl --version"))                # for messages only

# 3. controllers (the file is the authority, not the version)
um = "/sys/fs/cgroup/user.slice/user-{uid}.slice/user@{uid}.service"
ctl = read(um + "/cgroup.controllers")                 # unreadable => Off
have_cpu, have_mem, have_pids = "cpu" in ctl, "memory" in ctl, "pids" in ctl
if !(have_mem or have_pids or have_cpu): return Off(no controllers)

# 4. where am I (per process; hook and shim run this for their own PID)
own = read("/proc/{pid}/cgroup").strip_prefix("0::")
if own starts with um + "/":      place = SelfMoveOk
elif own matches ".../session-*.scope" under user-{uid}.slice:
    place = AttachOnly                                  # self-move would EACCES
    if !is_socket("/run/dbus/system_bus_socket"): return Off(system-bus)
else: place = Outside                                   # e.g. a container scope

# 5. plan
level = Full if have_cpu and have_mem and have_pids else Partial
plan  = { use_weights: have_cpu, use_memory_high: have_mem, use_pids_max: have_pids,
          nice_fallback: !have_cpu, placement: place }
return Report(level, plan)

# creation is still verified at use time, because probing is not creating:
create_scope(...)            -> on any error: Off(create-failed: stderr), never panic
leaf_files = ls(leaf)        -> use_weights only if "cpu.weight" exists
```

Notes: the user-manager probe is read-only; the creation test is the real
test and any failure degrades to "off" with the stderr text in the doctor
message. Probing must not create or move anything. `AttachOnly` hooks call
`AttachProcessesToUnit` once and then re-detect (the agent is now inside the
tree, children self-move).

## 9. Matrix: environment x what works x behaviour x doctor message

| Environment | What works | Sluice behaviour | Doctor message (summary line) |
|---|---|---|---|
| Modern desktop/laptop (Ubuntu 24.04+, Fedora 39+, Debian 13, Arch, this machine), shell under `user@` | everything: cpu weight, memory.high, pids.max, self-move | full containment; hook attaches agent to `ui`, env line self-moves shells to `tools` | `containment: full` |
| Same, agent started from SSH/TTY/GDM (login scope) | all limits; self-move of the shell is impossible, bus attach works (measured) | hook attaches agent via the user manager (system-bus fallback), children inherit `ui`, then self-move to `tools` | `containment: full` plus check 7 `warn` on a shell still in scope |
| Same but system bus unavailable | limits, but no way to move login-scope processes | those sessions uncontained; gates and daemon still placed (they are started by the daemon, in the tree) | `containment: partial (login-scope sessions not attachable: no system bus)` |
| Ubuntu 22.04, Debian 11, Leap 15.5, any systemd < 252 without drop-in | memory/pids at best (22.04: `pids memory`); no cpu; nice works across the user's flat group (unverified) | tree without weights; `MemoryHigh`/`pids.max` if delegated; `renice` on the shell | `containment: partial (memory, pids; cpu by nice)` and check 5 `warn` with the drop-in fix |
| Same plus admin drop-in `Delegate=cpu memory pids` | as modern | as modern | `containment: full` |
| `cpu` enabled but not delegated by override/patch (systemd >= 252) | memory/pids; cpu only by nice within a cgroup (measured: no effect across cgroups) | as the row above | `containment: partial`, check 5 second message |
| RHEL/Rocky/Alma 8, any cgroup v1 or hybrid (incl. old WSL2 default) | nothing cgroup-based | nice only; everything else as 0.4.1 | `containment: off (cgroup v1/hybrid)` |
| RHEL/Rocky 9, Debian 12 | likely full (unverified: vendor-patched units) | decided by the real `cgroup.controllers` | whatever detection finds |
| WSL2 with systemd, cgroup v2 | follows the distro | as the distro row | as the distro row |
| WSL2 without systemd, or su/cron/no login session | no user manager | nice only | `containment: off (user manager not reachable)` |
| Docker / devcontainer (default) | nothing; mount is `ro` (measured) | uncontained by design; runtime limits apply | `containment: off (read-only cgroup filesystem: container)` |
| Docker-in-Docker | as container | as container | as container |
| Bwrap/Claude Bash sandbox with read-only cgroupfs (spike S1) | nothing for the sandboxed shell | fail open; hook-side attach (outside the sandbox) is the S1/S3 question | as container for that shell |
| Headless server, SSH only, user manager present (linger or SSH login starts `user@`) | as modern, agent in session scope | as the login-scope row | as modern |

## 10. Admin drop-in for older distributions

`/etc/systemd/system/user@.service.d/delegate.conf`:

```ini
[Service]
# reset first, so the list is exactly what is written below on every version
Delegate=
Delegate=cpu cpuset io memory pids
```

Minimal variant (all Sluice needs): `Delegate=cpu memory pids`. The plan's
list adds `cpuset` and `io`; they are not used by ADR-0017 and `io` cost is
non-trivial (see below), so prefer the minimal list unless another tool needs
the extra controllers. Whether repeated `Delegate=` lines accumulate or
replace is not documented in what was read; the leading empty assignment makes
the result independent of it (an empty assignment resetting a list setting is
the usual systemd convention; **unverified** specifically for `Delegate=`).

How to apply: write the file, `sudo systemctl daemon-reload`, then the user
manager must restart to pick it up: log out and in (the user manager stops when
the last session ends unless lingering is on) or reboot. Verify with
`systemctl show user@$UID.service -p DelegateControllers` (or
`cat /sys/fs/cgroup/user.slice/user-$UID.slice/user@$UID.service/cgroup.controllers`).
`sluice doctor` check 5 turns `ok`.

Implications:

- It applies to **every** user of the machine, not only agent users: each
  user's manager gets the controllers and can create its own cgroups.
- `cpu` enables CPU accounting for the user tree (the overhead upstream cited
  when it was off by default); small, and the default on systemd >= 252.
- `io` enables the I/O controller below the user slice: possible measurable
  overhead on some devices and schedulers; not needed by Sluice.
- No privilege escalation: delegation gives the user control only of their
  own subtree; still not a security boundary (a process can move itself out of
  its own leaves).
- Without the drop-in nothing breaks: Sluice degrades to the partial row.
- Needs root once per machine, so it belongs in the team rollout guide, not in
  Sluice itself (Sluice never touches `/etc`).
- Ubuntu 22.04 reaches end of standard support in 2027; moving the team to 24.04
  removes the need entirely.

## 11. Cleanup and caveats

- Everything started here was transient and is stopped:
  `systemctl --user list-units --all 'sluice-s4*'` shows only
  `sluice-s4-park.service` (the parked desktop process, section 3.3), which is
  intentional and harmless; no stray test processes (the `sleep`/`python3`
  helpers were killed or died with their units). Units `sluice-s1.scope` and
  `sluice-s3.scope` belong to the parallel spikes and were not touched.
- Not verified here (need other machines): Ubuntu 22.04 flat-group nice
  behaviour, Debian 12 / RHEL 9 packaged `Delegate=`, Leap 15.6 default
  hierarchy, WSL2 current default, behaviour without the system bus, a real
  SSH login scope (the GDM scope was used instead), latency of the bus
  fallback.

## 12. Sources

- Commit `b8df7f8629`, "user: delegate cpu controller, assign weights to user
  slices": https://github.com/systemd/systemd/commit/b8df7f8629
- Systemd `bus_unit_method_attach_processes` (delegated unit required, active
  unit, ownership checks, unprivileged vtable flag):
  https://raw.githubusercontent.com/systemd/systemd/main/src/core/dbus-unit.c
- Systemd `unit_attach_pids_to_cgroup` and `unit_attach_pid_to_cgroup_via_bus`
  (fallback to the system manager): https://raw.githubusercontent.com/systemd/systemd/main/src/core/cgroup.c
- `cg_attach` writes `cgroup.procs`:
  https://raw.githubusercontent.com/systemd/systemd/main/src/shared/cgroup-setup.c
- D-Bus API listing (`AttachProcessesToUnit`, `RemoveSubgroupFromUnit`):
  https://www.freedesktop.org/software/systemd/man/latest/org.freedesktop.systemd1.html
- Kernel cgroup v2, delegation and the containment rule:
  https://docs.kernel.org/admin-guide/cgroup-v2.html ; Heo, "cgroup: Use
  CAP_SYS_RESOURCE ..." thread: https://lkml.iu.edu/hypermail/linux/kernel/1612.0/03557.html ;
  uid-check removal patch: https://lkml.iu.edu/hypermail/linux/kernel/1701.2/04495.html
- Package versions: https://repology.org/project/systemd/versions
- WSL: https://learn.microsoft.com/windows/wsl/wsl-config ;
  https://github.com/spurin/wsl-cgroupsv2
- Related notes: [[sluice/plan/containment-plan.md]],
  [[sluice/design/adr/0017-agent-resource-containment.md]].
