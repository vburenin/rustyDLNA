# ADR 0004: Streaming output and index identity

- Status: accepted
- Date: 2026-09-12
- Deciders: rustyDLNA maintainers

## Context

Concurrent delivery and incremental indexing share an output inode that moves
from staging to its published cache name. Duplicated Unix descriptors can share
a seek cursor. Completed index metadata is useful across attachments, but it
does not establish that an encoder produced a complete, valid movie. A fallback
encoder can also produce different bytes and HDR behavior from the requested
recipe before a client observes the generation.

## Decision

Delivery and indexing use positional reads on the pinned descriptor. The pin
lock establishes generation ownership; it is not a delivery cursor lock.
Pinning is an irrevocable fallback boundary, including response headers and
index/status access before the first media body. A producer cannot replace an
output once a reader may hold that inode.

Index histories use immutable shared chunks. Formatting retains a view and
releases the live index lock. Completed index reuse is process-local and bounded
by both entry count and retained metadata bytes. Its identity includes the output
inode, size, timestamps and parser revision. A validated completed-output stamp
remains a prerequisite for reopening cached media. Indexes retain no file
descriptors or companion disk artifacts, so they cannot protect evicted media or
escape disk quota accounting. Process restart reparses the metadata.

Native HLS retains its complete EVENT history. Target duration belongs to the
playlist request generation. A later copied GOP that exceeds the published
target requires a new generation; it cannot silently alter an existing EVENT
playlist. A new generation can select the known larger maximum from the shared
index. This remains a compatibility limitation for growing variable-GOP copies.

A request cache pathname remains a reserved lookup slot. A fallback completion
stamp identifies the effective successful recipe, including source/tool identity,
exact output arguments and bounded device/driver observations. Only a currently
offered fallback may match that stamp. Preference after stable unsupported
failure expires; resource pressure, bad input and unknown failures do not disable
the primary path. No device-wide failure blacklist is introduced.

## Consequences

Delivery does not contend with whole-history formatting or mutate another
reader's cursor. Index reuse avoids repeated parsing within one process, with
bounded metadata retention and no new disk-artifact lifecycle. Native EVENT
network cost remains proportional to history; a sliding window needs a separate
compatible seek contract and native Safari evidence.

Conservative pinning can prevent a fallback even before a client receives media
bytes. Recovery then creates a new generation. Fallback bytes never claim the
requested hardware/HDR identity, and stream details can report the actual output.
