# Third-party notices

Sluice reimplements ideas from the projects below. No source code has been
copied so far; if a port of any code lands, this file lists the file, the
origin and the original license text alongside it.

| Project | License | What Sluice took |
|---|---|---|
| [natelandau/gate-cache](https://github.com/natelandau/gate-cache) | MIT | Tree hashing from a private copy of the index and a private object directory; shared markers under the git common dir; the "unknown tree runs uncached" policy; miss records. |
| [block/agent-task-queue](https://github.com/block/agent-task-queue) | Apache-2.0 | Output to file with a bounded tail on failure; process-group kill escalation; request/job decoupling; documentation of agent permission pitfalls. |
| [yongjip/mergetrain](https://github.com/yongjip/mergetrain) | MIT | Lease fencing and stale-runner detection for the daemon. |
| [modelcontextprotocol/rust-sdk](https://github.com/modelcontextprotocol/rust-sdk) (`rmcp`) | Apache-2.0 | Used as a dependency for the MCP server. |
