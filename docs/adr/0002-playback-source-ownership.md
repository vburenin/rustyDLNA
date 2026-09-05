# ADR 0002: Scope asynchronous playback work to its source

- Status: accepted
- Date: 2026-09-05

## Context

A title may replace its media source during a seek, codec fallback, or network
retry. Media events and producer-status responses can arrive after replacement,
and several failure signals can describe the same interruption. Completion,
source cancellation, and delayed user actions therefore need explicit owners.

## Decision

`SourceSelector` chooses a plan from a state snapshot and browser capability
checks, and owns the bounded completed-probe cache. Native Safari HLS selection
is synchronous so the initial media request retains user activation.

Each load creates a `PlaybackSource` with a new monotonically increasing ID.
It owns the abort signal, media listeners, polling/startup timers, startup
reports, and a shared recovery promise. Cancellation stops its work and makes
queued callbacks harmless. Concurrent decoder, resource, and premature-end
failures share recovery for that source.

The controller owns user intent and coordinates source changes with the store.
Seek debounce and retry delays stay with the controller because they schedule
a replacement after the previous source is cancelled. The store applies
source readiness and completion atomically. Pure codec recovery decisions live
in `core.js`; finite retry budgets belong to the selected title or an explicit
user restart.

## Consequences

Source selection does not mutate the UI, and source cancellation has one
cleanup path. Late callbacks cannot alter a completed or replaced source.
Reaching the end of a media response is checked against the title timeline
before completion can clear progress, loop, or advance the queue.

The store holds the global title position and display state; the media element
holds the decoder's local timeline. The controller translates between them
using the source's segment offset. Browser regressions exercise that boundary
with real media; Node tests cover recovery decisions and lifetime ownership.
