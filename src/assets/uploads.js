// Attachments on the agent page: the decisions, kept free of the DOM and of
// XHR so a test can drive them (routes.rs runs this under node).

/// A byte count the way a person reads it. Mirrors `uploads::human_size` in
/// Rust, so a chip and the trailer the agent reads say the same size.
export function humanSize(bytes) {
  const units = ['KB', 'MB', 'GB', 'TB'];
  if (bytes < 1024) return `${bytes} B`;
  let value = bytes / 1024;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value.toFixed(1)} ${units[unit]}`;
}

/// Where an agent's uploads live on the API. The name is a path segment, so
/// it is encoded; the server refuses anything it would not have stored.
export function uploadsUrl(agentId, name) {
  const base = `/api/agents/${encodeURIComponent(agentId)}/uploads`;
  return name === undefined ? base : `${base}/${encodeURIComponent(name)}`;
}

/// What Send should do, given the composer's state. `chips` is the list of
/// attachments, each with a `status` of `uploading`, `done` or `failed`.
///
/// - `blocked` greys the button: the agent is not running, or a file is still
///   on its way (the message would otherwise go without it).
/// - `attachments` are the names to send: finished uploads only.
/// - `empty` means there is nothing to send at all.
export function composerState({ running, text, chips }) {
  const uploading = chips.some((c) => c.status === 'uploading');
  const attachments = chips.filter((c) => c.status === 'done').map((c) => c.name);
  let reason = '';
  if (!running) reason = 'The agent is not running. Resume it to send.';
  else if (uploading) reason = 'Waiting for uploads to finish.';
  return {
    blocked: !running || uploading,
    reason,
    attachments,
    empty: !text.trim() && attachments.length === 0,
  };
}

/// The files a paste should upload, or an empty list to let the browser paste
/// as usual.
///
/// A screenshot arrives as a file and nothing else. A file copied in a file
/// manager arrives as the file plus its name as plain text — upload it. But
/// rich text copied from a document or a web page carries `text/html` and
/// often a rendered image of itself too; that is a text paste, and uploading
/// the picture would be a surprise.
export function pastedFiles(types, files) {
  const list = Array.from(files || []);
  if (!list.length) return [];
  if (Array.from(types || []).includes('text/html')) return [];
  return list;
}
