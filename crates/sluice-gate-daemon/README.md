# sluice-gate-daemon

Internal daemon of Sluice: weighted budget scheduler, request coalescing, gate executor and metrics.

This crate is an internal component of [Sluice](https://github.com/josegarridodigio/sluice), a stack-agnostic verification queue and cache for parallel coding agents. It is published only so that the `sluice-gate` binary crate can depend on it. Its API has **no stability promise** outside Sluice: it can change in any release without notice.

If you want to use Sluice, install the `sluice-gate` crate (the `sluice` binary) and read the [project README](https://github.com/josegarridodigio/sluice#readme).

License: MIT.
