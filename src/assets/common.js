// Shared helpers: API calls, the multiplexed socket, small DOM utilities.
// No build step, no dependencies — plain ES modules (§7).

// The credential this page presents. There are two of them, and which one a
// page holds depends on how it was opened (§7, §12).
//
// *The per-boot session token* arrives in the URL the server opens (`?t=`), is
// kept out of the address bar afterwards, and is required on every API call and
// on the socket upgrade. Loopback is not an authentication boundary —
// everything on this machine can reach the server, agents included — so this
// raises the bar in front of the control plane. It lives in sessionStorage, not
// localStorage: scoped to this tab, gone when the browser session ends, which
// shortens the window in which it sits in the browser profile on disk. The
// server refuses it from any peer that is not loopback.
//
// *The paired device key* arrives in the fragment of the URL the QR code from
// `claude-web pair` carries (`#k=`). A fragment is never sent to the server and
// never appears in a Referer; it is read on load and cleared immediately with
// replaceState, so it does not sit in browser history either. It lives in
// localStorage — a deliberate departure from the reasoning above, and a narrow
// one: a phone that must be re-paired every time the browser drops the tab is
// not usable, and the device is one you chose to pair.
//
// Neither can be made unreadable to a process running as the same user; see
// DESIGN §7.
const TOKEN_KEY = 'claude-web-token';
const PAIRED_KEY = 'claude-web-key';

/// Was this page served over loopback? The two credentials are not
/// interchangeable, and this decides which one is worth presenting.
function servedLocally() {
  return ['localhost', '127.0.0.1', '::1', '[::1]'].includes(location.hostname);
}

function readCredential() {
  try {
    const url = new URL(location.href);
    const fromUrl = url.searchParams.get('t');
    if (fromUrl) {
      sessionStorage.setItem(TOKEN_KEY, fromUrl);
      url.searchParams.delete('t');
      history.replaceState(null, '', url.pathname + url.search + url.hash);
      return fromUrl;
    }
    const paired = takeFragmentKey(url);
    if (paired) {
      localStorage.setItem(PAIRED_KEY, paired);
      return paired;
    }
    const token = sessionStorage.getItem(TOKEN_KEY) || '';
    const key = localStorage.getItem(PAIRED_KEY) || '';
    // Off loopback the per-boot token is refused outright, so the device key is
    // the only one worth sending; on loopback it is the one the server minted
    // for this run.
    return servedLocally() ? token || key : key || token;
  } catch {
    return '';
  }
}

/// Read `#k=<key>` and strip it from the address bar, the way `?t=` is handled.
function takeFragmentKey(url) {
  const hash = url.hash.startsWith('#') ? url.hash.slice(1) : url.hash;
  if (!hash) return '';
  let params;
  try {
    params = new URLSearchParams(hash);
  } catch {
    return '';
  }
  const key = params.get('k');
  if (!key) return '';
  params.delete('k');
  const rest = params.toString();
  history.replaceState(null, '', url.pathname + url.search + (rest ? `#${rest}` : ''));
  return key;
}

export const token = readCredential();

export async function api(path, options = {}) {
  const response = await fetch(path, {
    ...options,
    headers: {
      'content-type': 'application/json',
      'x-claude-web-token': token,
      ...(options.headers || {}),
    },
  });
  const text = await response.text();
  let body = null;
  if (text) {
    try {
      body = JSON.parse(text);
    } catch {
      body = { error: text };
    }
  }
  if (!response.ok) {
    if (response.status === 401) needToken(body && body.error);
    const err = new Error((body && body.error) || `${response.status} ${response.statusText}`);
    err.status = response.status;
    err.body = body;
    throw err;
  }
  return body;
}

// Mirrors repo::git::slugify so the branch preview matches what the server
// will actually create.
export function slugify(name) {
  let out = '';
  let underscore = false;
  for (const ch of String(name)) {
    if (/[a-zA-Z0-9]/.test(ch)) {
      out += ch.toLowerCase();
      underscore = false;
    } else if (!underscore) {
      out += '_';
      underscore = true;
    }
    if (out.length >= 40) break;
  }
  out = out.replace(/^_+|_+$/g, '');
  return out || 'agent';
}

export function el(tag, attrs = {}, children = []) {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(attrs)) {
    if (value === null || value === undefined || value === false) continue;
    if (key === 'class') node.className = value;
    // Deliberately no `html` escape hatch: every node in this app is built
    // through here, and much of the content is attacker-influenced.
    else if (key === 'text') node.textContent = value;
    else if (key.startsWith('on')) node.addEventListener(key.slice(2), value);
    else node.setAttribute(key, value === true ? '' : value);
  }
  for (const child of [].concat(children)) {
    if (child === null || child === undefined) continue;
    node.append(child instanceof Node ? child : document.createTextNode(String(child)));
  }
  return node;
}

export function statusLabel(status, detail) {
  const base = {
    starting: 'Starting',
    idle: 'Idle',
    working: 'Working',
    awaiting_approval: 'Awaiting approval',
    stopped: 'Stopped',
    failed: 'Failed',
  }[status] || status;
  return detail ? `${base} — ${detail}` : base;
}

export function statusEl(status, detail) {
  return el('span', { class: `status s-${status}` }, [
    el('span', { class: 'dot' }),
    el('span', { text: statusLabel(status, detail) }),
  ]);
}

export function fmtCost(usd) {
  const n = Number(usd || 0);
  return n < 0.01 ? `$${n.toFixed(4)}` : `$${n.toFixed(2)}`;
}

export function fmtTime(ms) {
  if (!ms) return '';
  return new Date(ms).toLocaleTimeString();
}

export function fmtAgo(ms) {
  if (!ms) return '';
  const secs = Math.max(0, Math.round((Date.now() - ms) / 1000));
  if (secs < 60) return `${secs}s ago`;
  if (secs < 3600) return `${Math.round(secs / 60)}m ago`;
  if (secs < 86400) return `${Math.round(secs / 3600)}h ago`;
  return `${Math.round(secs / 86400)}d ago`;
}

// -- tab attention ----------------------------------------------------------
//
// An agent in `awaiting_approval` is *blocked on a human*, and the operator is
// usually in their editor rather than on this page. The tab is the only surface
// that reaches them there, so it carries the alert: the title counts what is
// waiting, and the turtle gets an amber badge.
//
// Only `awaiting_approval`. `failed` looks like it belongs here but does not:
// it is terminal, nothing is waiting on the human, and a flash that cannot be
// resolved by answering it would simply never stop.

const ICON = '/assets/favicon.svg';
const ICON_ALERT = '/assets/favicon-alert.svg';
const FLASH_MS = 1200;

let baseTitle = document.title;
let attention = 0;
let loudPhase = false;
let flashTimer = null;

/// Is the operator actually looking at this tab? A visible tab in an unfocused
/// window does not count — that is the case this whole feature exists for.
function watching() {
  return !document.hidden && document.hasFocus();
}

function setIcon(href) {
  const link = document.querySelector('link[rel="icon"]');
  if (link && link.getAttribute('href') !== href) link.setAttribute('href', href);
}

function paintTab() {
  if (!attention) {
    document.title = baseTitle;
    setIcon(ICON);
    return;
  }
  const noun = attention === 1 ? 'approval' : 'approvals';
  const loud = !watching() && loudPhase;
  document.title = loud ? `🔔 ${attention} ${noun} needed` : `(${attention}) ${baseTitle}`;
  // Looking at it, the badge sits still: the page already shows the amber card,
  // and something blinking under their nose is just noise. Away, it blinks.
  setIcon(watching() || loud ? ICON_ALERT : ICON);
}

function scheduleFlash() {
  const wanted = attention > 0 && !watching();
  if (wanted && !flashTimer) {
    flashTimer = setInterval(() => {
      loudPhase = !loudPhase;
      paintTab();
    }, FLASH_MS);
  } else if (!wanted && flashTimer) {
    clearInterval(flashTimer);
    flashTimer = null;
    loudPhase = false;
  }
  paintTab();
}

/// The title to show when nothing is waiting. Pages own their own title, so
/// they set it through here rather than writing `document.title` — otherwise
/// the two would overwrite each other every time the flash ticks.
export function setTitle(text) {
  baseTitle = text;
  paintTab();
}

/// How many things are waiting on a human right now. 0 clears the alert.
export function setAttention(count) {
  const next = Math.max(0, Number(count) || 0);
  if (next === attention) return;
  attention = next;
  scheduleFlash();
}

document.addEventListener('visibilitychange', scheduleFlash);
window.addEventListener('focus', scheduleFlash);
window.addEventListener('blur', scheduleFlash);

// One socket, one reconnect path, one schema. Handlers are keyed by the
// envelope's `type`.
export class Socket {
  constructor() {
    this.handlers = new Map();
    this.queue = [];
    this.ws = null;
    this.retry = 0;
    this.onopen = null;
    /** Set once reconnecting has been given up on, and never unset. */
    this.stopped = false;
    this.connect();
  }

  connect() {
    if (this.stopped) return;
    if (!token) {
      // Nothing to present, so the upgrade would be refused every time.
      this.stopped = true;
      needToken();
      return;
    }
    const proto = location.protocol === 'https:' ? 'wss' : 'ws';
    // A browser cannot set headers on an upgrade, so the credential rides in
    // the query string for this one request — the per-boot token or the paired
    // device key, whichever this page holds.
    this.ws = new WebSocket(`${proto}://${location.host}/ws?token=${encodeURIComponent(token)}`);
    this.ws.addEventListener('open', () => {
      this.retry = 0;
      for (const msg of this.queue.splice(0)) this.ws.send(msg);
      if (this.onopen) this.onopen();
    });
    this.ws.addEventListener('message', (event) => {
      let msg;
      try {
        msg = JSON.parse(event.data);
      } catch {
        return;
      }
      const handler = this.handlers.get(msg.type);
      if (handler) handler(msg);
      const any = this.handlers.get('*');
      if (any) any(msg);
    });
    this.ws.addEventListener('close', () => {
      this.reconnect();
    });
  }

  /// Decide whether reconnecting can ever work, and back off if it can.
  ///
  /// A browser deliberately hides the HTTP status of a rejected upgrade: a
  /// refused token and a server that is merely down both arrive here as a bare
  /// close with code 1006. Untangled, that made a page whose token had gone
  /// stale — the usual cause being a tab left open across a restart, since the
  /// token is minted per boot — retry every 16s for as long as it stayed open,
  /// showing the operator nothing and filling the server log with refusals.
  ///
  /// So ask over HTTP, which does report the status. A 401 is a verdict, not a
  /// hiccup: no number of retries will make a dead token live, and only opening
  /// the new link will. Anything else — including an unreachable server, which
  /// is exactly the transient case — backs off and tries again.
  async reconnect() {
    if (this.stopped) return;
    const refusal = await credentialRefused();
    if (refusal !== null) {
      this.stopped = true;
      needToken(refusal);
      return;
    }
    // Reconnect with a cursor, so only the delta is replayed.
    this.retry = Math.min(this.retry + 1, 6);
    setTimeout(() => this.connect(), 250 * 2 ** this.retry);
  }

  on(type, handler) {
    this.handlers.set(type, handler);
    return this;
  }

  send(msg) {
    const text = JSON.stringify(msg);
    if (this.ws && this.ws.readyState === WebSocket.OPEN) this.ws.send(text);
    else this.queue.push(text);
  }
}

/// Does the server refuse our credential outright? The refusal's own wording if
/// so — the server knows whether we reached it over loopback, and says
/// something different to a phone — and `null` for anything else, including not
/// being able to ask: a server that is down is not a verdict on the credential.
async function credentialRefused() {
  try {
    const response = await fetch('/api/health', {
      headers: { 'x-claude-web-token': token },
    });
    if (response.status !== 401) return null;
    const body = await response.json().catch(() => null);
    return (body && body.error) || '';
  } catch {
    return null;
  }
}

/// Tell the operator their credential is stale, once, and stop pretending to
/// work.
///
/// "Open the link claude-web printed when it started" is useless advice on a
/// phone that has never been near the terminal, so the server's own message is
/// preferred — it branches on the peer address it can see — and the fallback
/// branches the same way on where this page was served from.
let toldAboutToken = false;
export function needToken(message) {
  if (toldAboutToken) return;
  toldAboutToken = true;
  const fallback = servedLocally()
    ? 'Open the link claude-web printed when it started — the token changes every '
      + 'time the server restarts.'
    : 'This device is not paired, or its key has been replaced. Run `claude-web pair` '
      + 'on the machine running the server and scan the new code.';
  const banner = el('div', { class: 'errbox' }, [
    el('strong', { text: 'This page has no valid credential. ' }),
    message || fallback,
  ]);
  document.querySelector('main')?.prepend(banner);
}

export function toast(text, level = 'info') {
  let host = document.querySelector('.toasts');
  if (!host) {
    host = el('div', { class: 'toasts' });
    document.body.append(host);
  }
  const node = el('div', { class: `toast ${level}`, text });
  host.append(node);
  setTimeout(() => node.remove(), level === 'error' ? 12000 : 6000);
}
