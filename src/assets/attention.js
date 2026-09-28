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
  constructor({ every, start = setInterval, stop = clearInterval, onTick = () => {} }) {
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
export const CHIME_CLAIM_MS = 5000;

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

  claim(key) {
    try {
      const at = this.now();
      let claims = JSON.parse(this.storage.getItem(CHIME_STORAGE_KEY) || '{}');
      if (!claims || typeof claims !== 'object' || Array.isArray(claims)) claims = {};
      const fresh = {};
      for (const [k, t] of Object.entries(claims)) {
        if (typeof t === 'number' && at - t < this.ttl) fresh[k] = t;
      }
      if (Object.hasOwn(fresh, key)) return false;
      fresh[key] = at;
      this.storage.setItem(CHIME_STORAGE_KEY, JSON.stringify(fresh));
    } catch {
      // Storage unavailable (private mode, quota): a double chime beats none.
    }
    return true;
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
