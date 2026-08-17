# TODO

Ongoing maintenance items. Not the roadmap — that's [PLAN.md](PLAN.md).

## Watch: upstream tiny_http fix (un-vendor when merged + released)

We carry a **vendored, one-function patch** of tiny_http at
`vendor/tiny_http/` (wired in via `[patch.crates-io]` in the root
`Cargo.toml`). The patch fixes an unauthenticated memory-exhaustion DoS:
`EqualReader::drop` drained an unread request body with
`vec![0; declared_content_length]`, so a request declaring a huge
`Content-Length` while sending a few bytes cost the process that whole
allocation at drop time. Measured before the fix: RSS 22 MB → 2.2 GB under
a handful of concurrent probes. See the note in
`vendor/tiny_http/src/util/equal_reader.rs`.

The same fix is filed upstream:

- **PR: https://github.com/tiny-http/tiny-http/pull/290**

**Check periodically** whether it's merged and released to crates.io:

```sh
gh pr view 290 --repo tiny-http/tiny-http --json state,mergedAt
```

**When it's merged AND in a published release:** un-vendor.
1. Delete the `[patch.crates-io]` block from the root `Cargo.toml`.
2. Bump the `tiny_http` version in `crates/harbor-core/Cargo.toml` to the
   release that contains the fix.
3. `rm -rf vendor/tiny_http` (and `vendor/` if nothing else lives there).
4. `make binary && make binary2 && make check` — all 10 suites must pass.
5. Re-run the DoS check to confirm the published fix holds:
   concurrent `POST /sql` with `Content-Length: 1000000000` sending a few
   bytes must leave RSS flat, not climb.

Until then the vendored copy is correct and harbor is safe; this is cleanup,
not a fix.
