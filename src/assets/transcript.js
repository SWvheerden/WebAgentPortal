// Transcript bookkeeping: which events have been seen, what to reconnect with,
// and when the catch-up walk is finished.
//
// Deliberately free of the DOM and of the socket, so it can be exercised
// directly. The bug this exists to prevent: the catch-up walk used the render
// cursor as its termination test, so a single live event arriving mid-walk
// both aborted the walk and discarded the page in flight, leaving a permanent
// hole in the middle of the transcript.

export class Transcript {
  constructor() {
    this.seen = new Set();
    /** Everything at or below this seq is accounted for: the reconnect cursor. */
    this.contiguous = 0;
    /** The highest seq seen, which may be far ahead of `contiguous` mid-walk. */
    this.max = 0;
    /** The oldest seq rendered, for "load earlier". */
    this.earliest = null;
    this.seeded = false;
  }

  /**
   * Accept the starting point of the first replay page. Everything at or below
   * it was deliberately not loaded (a fresh view shows the last window), so it
   * counts as accounted for rather than as a hole to chase.
   */
  seed(after) {
    if (this.seeded) return;
    this.seeded = true;
    const start = Number.isFinite(after) ? Math.max(0, after) : 0;
    if (start > this.contiguous) this.contiguous = start;
    if (start > this.max) this.max = start;
  }

  /**
   * Record events and return the ones that are new, in seq order. Duplicates —
   * which replay and the live stream produce whenever they overlap — return
   * nothing, so a caller can render the result unconditionally.
   */
  accept(events) {
    const fresh = [];
    for (const event of events || []) {
      if (!event || !Number.isFinite(event.seq)) continue;
      if (this.seen.has(event.seq)) continue;
      this.seen.add(event.seq);
      fresh.push(event);
      if (event.seq > this.max) this.max = event.seq;
      if (this.earliest === null || event.seq < this.earliest) this.earliest = event.seq;
    }
    while (this.seen.has(this.contiguous + 1)) this.contiguous += 1;
    fresh.sort((a, b) => a.seq - b.seq);
    return fresh;
  }

  /** The cursor to reconnect with. Never runs ahead of a gap. */
  get replayFrom() {
    return this.contiguous;
  }

  /** True while events between `contiguous` and `max` are still missing. */
  get hasGap() {
    return this.max > this.contiguous;
  }
}

/**
 * Where the catch-up walk goes next, or `null` when it is done.
 *
 * The decision uses only the server's own reply — `has_more` and the page's
 * cursor — and never the render state, which live events also advance.
 */
export function nextWalkCursor(page) {
  if (!page || !page.has_more) return null;
  if (!Array.isArray(page.events) || page.events.length === 0) return null;
  if (!Number.isFinite(page.cursor)) return null;
  return page.cursor;
}
