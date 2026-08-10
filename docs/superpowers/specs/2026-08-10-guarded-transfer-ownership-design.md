# Guarded Transfer Ownership Design

**Date:** 2026-08-10

**Status:** Approved quality addendum to the scoped-sync design

## Problem

Guarded pull and push currently stage through the predictable sibling name
`TARGET.tmp.zedftp`. A second writer can replace those bytes before the final
claim, and cleanup by a shared pathname can remove another writer's file.
Scoped FTP adapters also retain raw `suppaftp` errors whose server replies can
contain attacker-controlled terminal text.

## Owned sibling temps

Every guarded transfer uses a fresh sibling named:

```text
.<target-leaf>.ferry-tmp.<32-lowercase-hex>
```

The 32 hexadecimal digits are 128 bits read directly from the operating
system through `getrandom` 0.4. Local creation is exclusive. An occupied local
candidate is discarded and generation retries with fresh entropy. Remote FTP
has no exclusive-create primitive, so Ferry first requires the high-entropy
candidate to be strictly absent; an occupied candidate is discarded, and only
a fresh candidate is uploaded. The candidate source is injectable in tests so
collision and retry behavior is deterministic.

Scoped inventory reserves only the exact protocol grammar above. Local and
remote scoped traversal skip an exact match, including a crash-stale temp, but
ordinary near-miss names remain user content. This does not change Matcher
defaults or legacy traversal behavior.

## Claim-time revalidation and cleanup

After local staging, Ferry captures the stable parent, file identity, regular
file type, size, modification time, and content hash. Inside the final
`CommitGate` claim, it revalidates that snapshot immediately before rename.
Cleanup removes the path only while its identity is still the file this
transfer created.

After remote staging, Ferry captures a strict temp snapshot containing size,
modification time, and content hash. Inside the claim, it strictly re-snapshots
the temp and requires exact equality immediately before rename. Cleanup first
requires the same exact snapshot, so a replaced temp is left untouched.

Before that exact owned snapshot has been established, a failed upload or
snapshot capture may trigger one fresh strict snapshot. Ferry removes the temp
only when that snapshot proves a regular file with the intended size and hash;
the server-provided modification time is captured only after that match. A
successful first snapshot that has the wrong type or payload is not owned and
is never removed.

Destination and source checks stay in the claim. Successful rename is followed
immediately by state insertion; upload computes `last_synced` at that point.
`CommitDecision::Cancelled` means the mutation closure was not invoked, while
`Committed` means the mutation completed successfully.

## Scoped FTP errors

Legacy public `Ftp` methods keep their existing signatures and error chains.
Only scoped download, upload, mtime, rename, and remove adapters convert every
returned `suppaftp` error, including protocol/server-reply failures, into a
fresh path-sanitized error and discard the raw source chain.
`StrictDestinationRead::download_destination` and each corresponding
production `RemoteWrite for Ftp` operation route through these adapters.
Strict LIST and MKD retain their existing sanitized behavior.

## Verification

Tests cover exact and near-miss inventory names, local and remote temp
mutation/replacement, interleaved writers and cleanup ownership, rename/state
ordering, upload timestamp placement, and attacker-controlled FTP replies.
Formatting, focused suites, strict Clippy, the full test suite, diff checks,
and artifact checks are required before completion.
