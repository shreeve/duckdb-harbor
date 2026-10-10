# For an agent working here

Read `HANDOFF.md` before anything else. It says what the two products are,
how the owner works, how the pieces fit, how to build, test and release,
and what is open. This file is the short form of its rules; where the two
differ, `HANDOFF.md` is right.

- **Two products, one repository.** `harbor/` is DuckDB Harbor, the server
  and its CLI. `ducktable/` is DuckTable, the macOS client, with rules of
  its own in `ducktable/AGENTS.md`. Each has its own workspace, version,
  changelog and release tags.
- **Work in a worktree** under `~/Data/Code/duckdb-harbor-wt/`, never by
  switching branches in the shared checkout, which may hold another
  session's uncommitted work.
- **Commit, push and release only when asked.** A question is a question.
  "Land" means a pull request, a true merge, and the branch deleted on
  GitHub and locally, so that only `main` remains. Only Steve's word lands
  anything; another session's approval is not his.
- **No AI attribution**, in commits, pull requests, issues or comments,
  whatever a tool suggests.
- **Timeless prose.** No "now", "no longer", "previously", "legacy" or
  "new" in code, comments, docs or commit bodies. The changelog is the one
  place that tells what changed.
- **Verify by measuring**, on a scratch database with a short
  `HARBOR_HOME`. Never the MedLabs database. A server change gets a cold
  review by agents that wrote none of it before it lands.
- **Share the machines through cpuq.** Every build, suite and benchmark
  runs under `cpuq run` with a range of cores, never more than half; harbor
  work may go to the Linux box `pup` when the Mac is busy. HANDOFF has the
  pattern.
- **`live` is production.** Reads are fine. Installs and restarts there are
  Steve's to run.
- **Never `cargo fmt` the tree.**
- **A feature and its version bump are two pull requests**, the changelog
  entry in the first; then the tag, the install, and the Homebrew formula.
