// The divider between the conversation and the rail on the agent page: how wide
// the rail may be, what a key press does to it, and how the choice is kept.
// Free of the DOM so a test can drive it under node; agent.js does the wiring.
//
// The width is the rail's, in px, because the rail is what the operator is
// sizing — an AskUserQuestion with four options and a paragraph each is what
// made the old 300–400px track too cramped. It is kept in localStorage rather
// than config.toml: it is a fact about this screen, not about the server, and a
// width chosen on a 32" monitor means nothing to the phone that shares the
// config (where the rail stacks above the conversation and there is no divider).

/// The old track's floor. Narrower and an approval's buttons start wrapping.
export const MIN_SIDE_WIDTH = 300;
/// Wider than the old 400px ceiling, so a question has room by default.
/// Mirrored by the fallback in app.css; a test holds them together.
export const DEFAULT_SIDE_WIDTH = 480;
/// However far the divider is dragged, the transcript keeps this much.
/// Mirrored by the `calc()` in app.css; a test holds them together.
export const MIN_CONVERSATION_WIDTH = 480;
/// A stored value above this is not a width anyone chose.
export const MAX_STORED_WIDTH = 4000;
export const STEP = 16;
export const BIG_STEP = 64;
export const SIDE_WIDTH_KEY = 'claude-web-side-width';

/// The range the rail can take when `total` px are shared between it and the
/// conversation (the divider's own width already taken out). The floor wins
/// when the window is too small for both — the stylesheet does the same.
export function sideWidthBounds(total) {
  const room = Math.floor(Number(total) - MIN_CONVERSATION_WIDTH);
  return { min: MIN_SIDE_WIDTH, max: Math.max(MIN_SIDE_WIDTH, Number.isFinite(room) ? room : 0) };
}

export function clampSideWidth(px, total) {
  const { min, max } = sideWidthBounds(total);
  const n = Number.isFinite(Number(px)) ? Number(px) : DEFAULT_SIDE_WIDTH;
  return Math.round(Math.min(Math.max(n, min), max));
}

/// A usable stored width from whatever `raw` is, or null. Not clamped to the
/// window: a width chosen on a big window is still the preference when this one
/// is smaller, and the stylesheet holds it in for as long as it is.
export function parseSideWidth(raw) {
  if (raw === null || raw === undefined || raw === '') return null;
  const px = Number(raw);
  return Number.isInteger(px) && px >= MIN_SIDE_WIDTH && px <= MAX_STORED_WIDTH ? px : null;
}

/// The rail's width after `key`, or null for a key the divider does not handle.
/// The rail is on the right, so the arrows move the divider the way they point:
/// left widens the rail, right narrows it. Home and End follow the value the
/// separator reports (`aria-valuenow`, the rail's width), as the WAI-ARIA
/// window-splitter pattern has it: Home is the narrowest rail, End the widest.
export function keyedSideWidth(key, current, total, big = false) {
  const { min, max } = sideWidthBounds(total);
  const step = big ? BIG_STEP : STEP;
  switch (key) {
    case 'ArrowLeft': return clampSideWidth(current + step, total);
    case 'ArrowRight': return clampSideWidth(current - step, total);
    case 'Home': return min;
    case 'End': return max;
    default: return null;
  }
}

export function loadSideWidth(storage) {
  try {
    return parseSideWidth(storage && storage.getItem(SIDE_WIDTH_KEY));
  } catch {
    return null;
  }
}

/// Remember `px`, or forget the choice when it is null (a reset), so a later
/// change to the default reaches this browser too.
export function saveSideWidth(storage, px) {
  try {
    if (!storage) return;
    const value = parseSideWidth(px);
    if (value === null) storage.removeItem(SIDE_WIDTH_KEY);
    else storage.setItem(SIDE_WIDTH_KEY, String(value));
  } catch {
    // Storage refused: the width still applies, it is just not remembered.
  }
}
