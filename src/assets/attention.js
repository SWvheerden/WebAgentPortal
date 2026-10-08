// The decisions behind the tab alert, free of the DOM, the socket and Web
// Audio, so they can be driven directly (see the node tests in web/routes.rs).
// common.js owns the wiring: it paints what `tabLook` says, runs a `Flasher`
// on a real timer, and plays what a `Chimer` allows.

export const ICON = '/assets/favicon.svg';
export const ICON_ALERT = '/assets/favicon-alert.svg';
export const ICON_FLASH = '/assets/favicon-flash.svg';

/// What the tab should show. Watching, the badge sits still: the page already
/// shows the amber card, and something blinking under their nose is just
/// noise. Away, the icon alternates between the badge and a solid orange tile.
export function tabLook({ attention, watching, loud, baseTitle }) {
  if (!attention) return { title: baseTitle, icon: ICON };
  if (watching) return { title: `(${attention}) ${baseTitle}`, icon: ICON_ALERT };
  const noun = attention === 1 ? 'approval' : 'approvals';
  return loud
    ? { title: `🔔 ${attention} ${noun} needed`, icon: ICON_FLASH }
    : { title: `(${attention}) ${baseTitle}`, icon: ICON_ALERT };
}

/// The blink: a phase that flips on a timer while wanted. The timer functions
/// are injected so a test can tick it by hand.
export class Flasher {
  constructor({
    every,
    // Wrapped, not passed bare: called as `this.start(...)`, a bare native
    // would get the Flasher as its receiver and throw "Illegal invocation".
    start = (fn, ms) => setInterval(fn, ms),
    stop = (id) => clearInterval(id),
    onTick = () => {},
  }) {
    this.every = every;
    this.start = start;
    this.stop = stop;
    this.onTick = onTick;
    this.timer = null;
    this.loud = false;
  }

  get running() {
    return this.timer !== null;
  }

  /// Start or stop. Starting begins on the loud half, so the tab goes orange
  /// the moment it is needed rather than one tick later.
  want(wanted) {
    if (wanted && !this.running) {
      this.loud = true;
      this.timer = this.start(() => {
        this.loud = !this.loud;
        this.onTick();
      }, this.every);
    } else if (!wanted && this.running) {
      this.stop(this.timer);
      this.timer = null;
      this.loud = false;
    }
  }

  /// A fresh request while already flashing jumps back to the loud half.
  jolt() {
    if (this.running) this.loud = true;
  }
}

/// Request ids are the child process's to choose, and only unique per agent,
/// so a chime is keyed by both.
export function attentionKey(agentId, requestId) {
  return `${agentId}:${requestId}`;
}

export const CHIME_STORAGE_KEY = 'claude-web-chimed';
/// Long enough to outlast a reconnect. A background tab's timers are throttled
/// — Chrome's intensive throttling runs them once a minute — so a second tab
/// can reach the same request well after the first chimed for it. The map is
/// pruned on every write, so a long TTL only costs a few more entries.
export const CHIME_CLAIM_MS = 10 * 60 * 1000;

/// Cross-tab dedupe. Every tab of this app shares one localStorage, holding a
/// small map of `key -> claimed at`; the first tab to claim a key chimes, and
/// the rest see it and stay quiet. Entries older than `ttl` are pruned on every
/// write, so the map stays as small as the last few seconds of requests.
export class ChimeClaims {
  constructor(storage, now = () => Date.now(), ttl = CHIME_CLAIM_MS) {
    this.storage = storage;
    this.now = now;
    this.ttl = ttl;
  }

  /// The live claims, pruned of expired ones.
  read() {
    const at = this.now();
    let claims = JSON.parse(this.storage.getItem(CHIME_STORAGE_KEY) || '{}');
    if (!claims || typeof claims !== 'object' || Array.isArray(claims)) claims = {};
    const fresh = {};
    for (const [k, t] of Object.entries(claims)) {
      if (typeof t === 'number' && at - t < this.ttl) fresh[k] = t;
    }
    return fresh;
  }

  claim(key) {
    try {
      const fresh = this.read();
      if (Object.hasOwn(fresh, key)) return false;
      fresh[key] = this.now();
      this.storage.setItem(CHIME_STORAGE_KEY, JSON.stringify(fresh));
    } catch {
      // Storage unavailable (private mode, quota): a double chime beats none.
    }
    return true;
  }

  /// A request was answered. Ids are the child's to choose and may come round
  /// again, so an answered one must not mute its successor for the whole TTL.
  release(key) {
    try {
      const fresh = this.read();
      delete fresh[key];
      this.storage.setItem(CHIME_STORAGE_KEY, JSON.stringify(fresh));
    } catch {
      // Nothing to release into; the claim simply expires.
    }
  }
}

/// Decides whether this tab plays. A tab whose audio is still locked (no
/// gesture yet) must not claim: it would stay silent *and* silence a tab that
/// could have played. So the claim comes only after the context is running.
export class Chimer {
  constructor({ context, claims, play }) {
    this.context = context;
    this.claims = claims;
    this.play = play;
  }

  /// True when this tab played the chime for `key`.
  announce(key) {
    const ctx = this.context();
    if (!ctx) return false;
    if (ctx.state !== 'running') {
      // Asking to resume is harmless; playing into a suspended context would
      // only queue the notes for a surprise later.
      Promise.resolve()
        .then(() => ctx.resume())
        .catch(() => {});
      return false;
    }
    if (!this.claims.claim(key)) return false;
    try {
      this.play(ctx);
    } catch {
      // Audio is a courtesy; never let it take the page's socket handler down.
      return false;
    }
    return true;
  }
}

/// The ids in `snapshot` that `previous` did not hold — what arrived while the
/// socket was down and so never came as a `permission_request`.
export function newKeys(previous, snapshot) {
  const out = [];
  for (const key of snapshot) if (!previous.has(key)) out.push(key);
  return out;
}

/// The requests waiting on a human, across every agent, as `attentionKey`s. A
/// request chimes when it first lands here, whichever way it arrived — live,
/// or found in a snapshot after the socket was down.
export class PendingRequests {
  constructor(announce) {
    this.announce = announce;
    this.keys = new Set();
  }

  /// Replace everything with a server snapshot (`/api/agents`). With
  /// `announce`, whatever the snapshot holds that was not already known chimes.
  snapshot(agents, { announce = false } = {}) {
    const next = new Map();
    for (const agent of agents) {
      for (const request of agent.pending_permissions || []) {
        next.set(attentionKey(agent.id, request.request_id), [agent.id, request.request_id]);
      }
    }
    if (announce) {
      for (const key of newKeys(this.keys, next.keys())) this.announce(...next.get(key));
    }
    this.keys = new Set(next.keys());
  }

  /// A live `permission_request`. True when it was news (and so chimed).
  request(agentId, requestId) {
    const key = attentionKey(agentId, requestId);
    if (this.keys.has(key)) return false;
    this.keys.add(key);
    this.announce(agentId, requestId);
    return true;
  }

  /// A live `permission_resolved`.
  resolved(agentId, requestId) {
    this.keys.delete(attentionKey(agentId, requestId));
  }
}

/// Keeps a snapshot fetched over HTTP from overwriting newer socket news. The
/// socket keeps delivering while the fetch is in flight, and the response —
/// taken at some unknown point in that window — would otherwise replace state
/// the live messages had already moved past: an answered request put back, or
/// a fresh one dropped.
///
/// So while a fetch is in flight, live messages are held; the snapshot is
/// applied and the held messages replayed on top, in order. A newer fetch
/// supersedes an older one: its snapshot is taken after everything held so
/// far, so those are dropped, and the older fetch's result is discarded when it
/// lands.
export class Resync {
  constructor() {
    this.generation = 0;
    this.held = null;
  }

  get holding() {
    return this.held !== null;
  }

  /// A fetch is about to start. Returns the token to hand back to `finish`.
  begin() {
    this.generation += 1;
    this.held = [];
    return this.generation;
  }

  /// Apply a live message now, or hold it until the snapshot lands.
  route(apply) {
    if (this.held) this.held.push(apply);
    else apply();
  }

  /// The fetch for `generation` is done. `applySnapshot` is null when it
  /// failed: the held messages still apply, onto what the page already had.
  /// False, and nothing applied, when a newer fetch has superseded this one.
  finish(generation, applySnapshot) {
    if (generation !== this.generation) return false;
    const held = this.held || [];
    this.held = null;
    if (applySnapshot) applySnapshot();
    for (const apply of held) {
      try {
        apply();
      } catch (err) {
        // One bad message must not strand the rest behind it.
        console.error(err);
      }
    }
    return true;
  }
}

/// Run `run(signal)`, giving up after `ms`: the signal aborts, and the returned
/// promise rejects even if `run` ignores the signal. The timer is cleared
/// however it settles. For fetches whose stall would hold something else up —
/// a `Resync` holding every live message until the snapshot lands.
export function withDeadline(
  run,
  ms,
  // Wrapped for the same reason as Flasher's: `timers.set(...)` would hand the
  // native `timers` as its receiver.
  timers = { set: (fn, delay) => setTimeout(fn, delay), clear: (id) => clearTimeout(id) },
) {
  const controller = new AbortController();
  let timer = null;
  const expired = new Promise((_, reject) => {
    timer = timers.set(() => {
      const err = new Error(`gave up after ${ms / 1000}s`);
      controller.abort(err);
      reject(err);
    }, ms);
  });
  let work;
  try {
    work = Promise.resolve(run(controller.signal));
  } catch (err) {
    work = Promise.reject(err);
  }
  return Promise.race([work, expired]).finally(() => timers.clear(timer));
}
