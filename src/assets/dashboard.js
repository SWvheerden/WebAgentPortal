// Dashboard: the agent registry, account usage, notes, the spawn form, cloning
// and settings.
import { api, el, slugify, statusEl, fmtCost, fmtAgo, setAttention, Socket, toast } from '/assets/common.js';

const state = {
  agents: new Map(),
  config: null,
  repos: { recent: [], all: [], errors: [] },
  selectedRepo: null,
  repoInfo: null,
  cloneId: null,
  rateLimit: null,
  /// When the snapshot above was taken, so an old one can say so.
  rateLimitAt: null,
  notes: [],
  notesOpen: true,
  /// The id of the note being typed into, `NEW_NOTE` for one that has never
  /// been saved, or null. Exactly one row can be in the editor at a time.
  editing: null,
};

const $ = (id) => document.getElementById(id);

// -- rate limits ------------------------------------------------------------

// Rendered in this order whichever the CLI happens to report, so the panel does
// not reshuffle as windows come and go.
const WINDOWS = [
  ['five_hour', 'Session · 5 hours'],
  ['seven_day', 'Week · 7 days'],
  ['seven_day_overage_included', 'Week · incl. overage'],
];

// `resetsAt` is unix *seconds*, unlike every timestamp this app stores.
function fmtResetsIn(resetsAt) {
  if (!resetsAt) return '';
  const secs = (resetsAt * 1000 - Date.now()) / 1000;
  if (secs <= 0) return 'resetting';
  if (secs < 3600) return `resets in ${Math.ceil(secs / 60)}m`;
  const hours = Math.floor(secs / 3600);
  if (hours < 24) {
    const mins = Math.floor((secs % 3600) / 60);
    return mins ? `resets in ${hours}h ${mins}m` : `resets in ${hours}h`;
  }
  return `resets in ${Math.round(hours / 24)}d`;
}

function meter(label, utilization, resetsAt) {
  const pct = Math.max(0, Math.min(1, Number(utilization) || 0));
  const level = pct >= 0.9 ? 'err' : pct >= 0.6 ? 'warn' : 'ok';
  // Set through the CSSOM, not a style attribute: `style-src 'self'` carries no
  // 'unsafe-inline', so a style attribute would be dropped and the bar would
  // never fill.
  const fill = el('span');
  fill.style.width = `${pct * 100}%`;
  return el('div', { class: `meter ${level}` }, [
    el('div', { class: 'head' }, [
      el('span', { class: 'muted', text: label }),
      el('span', { class: 'pct', text: `${Math.round(pct * 100)}%` }),
    ]),
    el('div', { class: 'bar' }, [fill]),
    el('div', { class: 'foot', text: fmtResetsIn(resetsAt) }),
  ]);
}

// A window whose reset time has passed has rolled over, and the utilization we
// hold for it is from the window before. We do not know the new figure, so the
// meter is dropped rather than shown wrong — the snapshot is restored across
// restarts, so this is a real case, not a theoretical one.
function stillCurrent(resetsAt) {
  return !resetsAt || resetsAt * 1000 > Date.now();
}

function renderLimits() {
  const info = state.rateLimit;
  const panel = $('limits-panel');
  if (!info) {
    panel.classList.add('hidden');
    return;
  }
  const windows = info.unifiedWindows || {};
  const meters = WINDOWS
    .filter(([key]) => windows[key]?.utilization != null && stillCurrent(windows[key].resetsAt))
    .map(([key, label]) => meter(label, windows[key].utilization, windows[key].resetsAt));
  // Accounts that report no per-window breakdown still name a governing window
  // and its utilization; show that rather than an empty panel.
  if (!meters.length && info.utilization != null && stillCurrent(info.resetsAt)) {
    meters.push(meter(info.rateLimitType || 'Current window', info.utilization, info.resetsAt));
  }
  if (!meters.length) {
    panel.classList.add('hidden');
    return;
  }
  $('limits').replaceChildren(...meters);

  // Anything other than a plain "allowed" is the headline, not a footnote.
  const notes = [];
  if (info.status && info.status !== 'allowed') notes.push(info.status.replace(/_/g, ' '));
  if (info.isUsingOverage) notes.push('on overage');
  // Usage only ever climbs within a window, so an old reading is a floor, not a
  // lie — but it must not read as live. Said only once it is worth saying.
  const age = state.rateLimitAt ? Date.now() - state.rateLimitAt : 0;
  if (age > 60000) notes.push(`as of ${fmtAgo(state.rateLimitAt)}`);
  $('limits-note').textContent = notes.length ? `— ${notes.join(' · ')}` : '';
  panel.classList.toggle('warn-tint', info.status === 'rejected');
  panel.classList.remove('hidden');
}

// The socket only carries changes, so a page loaded between two API calls needs
// the last snapshot from the server.
async function loadLimits() {
  const data = await api('/api/rate_limit').catch(() => null);
  state.rateLimit = data ? data.rate_limit : null;
  state.rateLimitAt = (data && data.captured_at) || null;
  renderLimits();
}

// -- agents -----------------------------------------------------------------

// Everything the card actually displays, in display form. Two agents with the
// same signature render identically, so the node can be left alone — which is
// what stops a card being torn down and rebuilt under the pointer on every
// status message. `fmtAgo` rather than the raw timestamp: `last_active_at`
// changes on every event, but "4m ago" changes once a minute.
function cardSignature(agent) {
  return JSON.stringify([
    agent.name, agent.slug, agent.status, agent.status_detail,
    agent.repo_path, agent.is_git, agent.branch, agent.uses_worktree,
    agent.permission_mode, fmtCost(agent.cost_usd), fmtAgo(agent.last_active_at),
    agent.status === 'failed' ? agent.last_stderr : null,
  ]);
}

function agentCard(agent, signature) {
  const where = agent.is_git
    ? `${agent.branch || 'detached'} · ${agent.uses_worktree ? 'worktree' : 'main checkout'}`
    : 'no VCS';
  const actions = el('div', { class: 'actions' });
  const running = agent.status !== 'stopped' && agent.status !== 'failed';

  actions.append(el('a', { class: 'btn', href: `/agent/${agent.slug}`, text: 'Open' }));
  if (running) {
    actions.append(
      el('button', { text: 'Interrupt', onclick: (e) => act(agent.id, 'interrupt', e.target) }),
    );
    actions.append(el('button', { text: 'Stop', onclick: (e) => act(agent.id, 'stop', e.target) }));
  } else {
    actions.append(
      el('button', { text: 'Resume', onclick: (e) => act(agent.id, 'resume', e.target) }),
    );
  }
  actions.append(el('button', { text: 'Rename', onclick: () => rename(agent) }));
  actions.append(el('button', { class: 'danger', text: 'Delete', onclick: () => remove(agent) }));

  return el('div', { class: 'card', 'data-id': agent.id, 'data-sig': signature }, [
    el('h3', {}, [
      el('a', { href: `/agent/${agent.slug}`, text: agent.name }),
    ]),
    statusEl(agent.status, agent.status_detail),
    el('div', { class: 'meta' }, [
      `${agent.repo_path}`,
      el('br'),
      `${where} · ${agent.permission_mode} · ${fmtCost(agent.cost_usd)} · ${fmtAgo(agent.last_active_at)}`,
    ]),
    agent.status === 'failed' && agent.last_stderr
      ? el('div', { class: 'errbox small', text: agent.last_stderr })
      : null,
    actions,
  ]);
}

function renderAgents() {
  const host = $('cards');
  // Ordered by when the agent was created, which never changes: a card stays
  // where it was first put, for as long as it exists, and a new one is appended
  // after the rest. Ordering by `last_active_at` meant every status message —
  // one per tool call — sent whichever agent just did something to the front,
  // so with two agents working the board reshuffled continuously and a button
  // moved out from under the cursor between aiming and clicking.
  const list = [...state.agents.values()].sort((a, b) => a.created_at - b.created_at);

  // Reconcile rather than rebuild: an untouched card keeps its own DOM node, so
  // it holds its hover, focus and any in-flight button state.
  const stale = new Map([...host.children].map((node) => [node.dataset.id, node]));
  let previous = null;
  for (const agent of list) {
    const signature = cardSignature(agent);
    let node = stale.get(agent.id);
    if (node) {
      stale.delete(agent.id);
      if (node.dataset.sig !== signature) {
        const fresh = agentCard(agent, signature);
        node.replaceWith(fresh);
        node = fresh;
      }
    } else {
      node = agentCard(agent, signature);
    }
    // A no-op whenever the card is already in the right place, which — the
    // order being stable — is nearly always.
    if (node.parentNode !== host || node.previousElementSibling !== previous) {
      if (previous) previous.after(node);
      else host.prepend(node);
    }
    previous = node;
  }
  for (const gone of stale.values()) gone.remove();

  $('empty').classList.toggle('hidden', list.length > 0);
  const running = list.filter((a) => a.status !== 'stopped' && a.status !== 'failed').length;
  $('agent-count').textContent = list.length ? `— ${running} running of ${list.length}` : '';
  // Every render, so the tab clears itself the moment the last one is answered.
  setAttention(list.filter((a) => a.status === 'awaiting_approval').length);
}

async function loadAgents() {
  const data = await api('/api/agents');
  state.agents = new Map(data.agents.map((a) => [a.id, a]));
  renderAgents();
}

// One verb per agent at a time. A double-clicked Resume would otherwise put
// two requests in flight for one session.
const inFlight = new Set();

async function act(id, verb, button) {
  const key = `${id}:${verb}`;
  if (inFlight.has(key)) return;
  inFlight.add(key);
  if (button) button.disabled = true;
  try {
    await api(`/api/agents/${id}/${verb}`, { method: 'POST', body: '{}' });
  } catch (err) {
    toast(err.message, 'error');
  } finally {
    inFlight.delete(key);
    if (button && button.isConnected) button.disabled = false;
  }
}

async function rename(agent) {
  const name = prompt('New display name (the slug and branch never change):', agent.name);
  if (name === null) return;
  try {
    const data = await api(`/api/agents/${agent.id}/rename`, {
      method: 'POST',
      body: JSON.stringify({ name }),
    });
    state.agents.set(data.agent.id, { ...agent, ...data.agent });
    renderAgents();
  } catch (err) {
    toast(err.message, 'error');
  }
}

async function remove(agent) {
  if (!confirm(`Delete "${agent.name}"? The branch is kept by default.`)) return;
  // A branch the agent did not create is not ours to destroy, so it is not
  // even offered — the server refuses it too.
  const deleteBranch = agent.branch && agent.branch_is_new !== false
    ? confirm(`Also delete the branch ${agent.branch}?`)
    : false;
  const qs = `?delete_branch=${deleteBranch}`;
  try {
    await api(`/api/agents/${agent.id}${qs}`, { method: 'DELETE' });
  } catch (err) {
    if (err.status === 409 && err.body && err.body.report) {
      const report = err.body.report;
      const lost = [
        ...report.uncommitted.map((l) => `  uncommitted: ${l}`),
        ...report.unpushed.map((l) => `  unpushed: ${l}`),
      ].join('\n');
      if (!confirm(`${err.body.error}\n\nThis would be lost:\n${lost}\n\nDelete anyway?`)) return;
      try {
        await api(`/api/agents/${agent.id}${qs}&force=true`, { method: 'DELETE' });
      } catch (inner) {
        toast(inner.message, 'error');
      }
    } else {
      toast(err.message, 'error');
    }
  }
}

// -- notes ------------------------------------------------------------------
//
// A note is inert text: no repo, no branch, no spawn parameters. Retyping a
// task into the spawn form costs seconds and keeps the note a memo.

/// The row that has been appended but never saved. Held client-side until it
/// has a body, so the database never sees a blank.
const NEW_NOTE = '\u0000new';

/// localStorage, not the sessionStorage the token uses: keeping the *token* out
/// of the browser profile past the tab is a decision about the token, and it
/// does not generalise to a panel that is open or shut.
const NOTES_OPEN_KEY = 'claude-web-notes-open';

function readNotesOpen() {
  try {
    return localStorage.getItem(NOTES_OPEN_KEY) !== '0';
  } catch {
    return true;
  }
}

/// The first line, derived here and never stored: a note is one body, and a
/// title column would be a second thing to keep in step with it.
function noteHeading(body) {
  return String(body).split('\n')[0] || '(empty)';
}

/// Collapsed and expanded are different rows, so the mode belongs in the
/// signature — otherwise toggling the panel leaves every row as it was.
function noteSignature(note) {
  return JSON.stringify([state.notesOpen, note.body]);
}

function noteRow(note, signature) {
  if (!state.notesOpen) {
    return el('div', { class: 'note', 'data-id': note.id, 'data-sig': signature }, [
      el('div', { class: 'body', text: noteHeading(note.body), title: note.body }),
    ]);
  }
  return el('div', { class: 'note', 'data-id': note.id, 'data-sig': signature }, [
    // Plain text, not markdown: the frontend is embedded in the binary with no
    // bundler, and rendering markdown means vendoring a parser and then owning
    // a sanitiser for text only its author will read.
    el('div', { class: 'body', text: note.body, onclick: () => startEdit(note.id) }),
    el('button', { class: 'del', text: '×', title: 'Delete this note', onclick: () => removeNote(note) }),
  ]);
}

/// One editor, one code path: a new note and an edit are the same row.
///
/// Blur saves and Escape cancels, so there is no third button to aim at. The
/// blur that the teardown itself causes is not a save — a detached row has
/// nothing to commit.
function noteEditor(id, body) {
  const area = el('textarea', { rows: '3' });
  area.value = body;
  const row = el('div', { class: 'note editing', 'data-id': id, 'data-sig': 'editing' }, [area]);
  area.addEventListener('keydown', (event) => {
    if (event.key !== 'Escape') return;
    event.preventDefault();
    state.editing = null;
    renderNotes();
  });
  area.addEventListener('blur', () => {
    if (!row.isConnected) return;
    commitEdit(id, area.value);
  });
  return row;
}

function renderNotes() {
  $('notes-panel').classList.toggle('collapsed', !state.notesOpen);
  $('notes-toggle').setAttribute('aria-expanded', String(state.notesOpen));
  const host = $('notes');

  // The row being typed into keeps its own node — and with it the caret and
  // whatever has not been saved — however often the poll comes round. Every
  // other row is reconciled the way the agent cards are.
  const editingNode = state.editing ? host.querySelector('.note.editing') : null;
  const wanted = state.notes.map((note) => [note.id, note]);
  if (state.editing === NEW_NOTE) wanted.push([NEW_NOTE, null]);

  const leftover = new Map([...host.children].map((node) => [node.dataset.id, node]));
  let previous = null;
  for (const [id, note] of wanted) {
    let node;
    if (id === state.editing) {
      const old = leftover.get(id);
      leftover.delete(id);
      node = editingNode || noteEditor(id, note ? note.body : '');
      if (old && old !== node) old.replaceWith(node);
    } else {
      const signature = noteSignature(note);
      node = leftover.get(id);
      if (node) {
        leftover.delete(id);
        if (node.dataset.sig !== signature) {
          const fresh = noteRow(note, signature);
          node.replaceWith(fresh);
          node = fresh;
        }
      } else {
        node = noteRow(note, signature);
      }
    }
    if (node.parentNode !== host || node.previousElementSibling !== previous) {
      if (previous) previous.after(node);
      else host.prepend(node);
    }
    previous = node;
  }
  for (const gone of leftover.values()) gone.remove();

  $('notes-count').textContent = state.notes.length ? `— ${state.notes.length}` : '';
  $('notes-empty').classList.toggle('hidden', !!wanted.length || !state.notesOpen);
}

async function loadNotes() {
  const data = await api('/api/notes').catch(() => null);
  if (!data) return;
  state.notes = data.notes;
  renderNotes();
}

function setNotesOpen(open) {
  state.notesOpen = open;
  try {
    localStorage.setItem(NOTES_OPEN_KEY, open ? '1' : '0');
  } catch {
    // A browser that refuses storage still gets the panel; it just forgets.
  }
  // Collapsed rows are an index, not a form: an editor left open in one would
  // be a control the operator cannot see the rest of.
  if (!open) state.editing = null;
  renderNotes();
}

function startEdit(id) {
  if (!state.notesOpen) return;
  state.editing = id;
  renderNotes();
  const area = $('notes').querySelector('.note.editing textarea');
  if (!area) return;
  area.focus();
  area.setSelectionRange(area.value.length, area.value.length);
}

function newNote() {
  if (!state.notesOpen) setNotesOpen(true);
  startEdit(NEW_NOTE);
}

async function commitEdit(id, raw) {
  const body = raw.trim();
  try {
    if (id === NEW_NOTE) {
      // Nothing typed: the row is dropped and the server is never told.
      if (body) {
        const data = await api('/api/notes', { method: 'POST', body: JSON.stringify({ body }) });
        state.notes.push(data.note);
      }
    } else {
      const note = state.notes.find((n) => n.id === id);
      // An emptied body is not a delete — deleting is its own action, and it
      // asks first. Left alone, the note keeps the text it had.
      if (note && body && body !== note.body) {
        const data = await api(`/api/notes/${id}`, {
          method: 'PATCH',
          body: JSON.stringify({ body }),
        });
        Object.assign(note, data.note);
      }
    }
  } catch (err) {
    toast(err.message, 'error');
  } finally {
    // Clicking straight from one note into another blurs the first and opens
    // the second before this settles; closing "the editor" then would shut the
    // row the operator had just aimed at.
    if (state.editing === id) state.editing = null;
    renderNotes();
  }
}

async function removeNote(note) {
  // The body is text the operator typed and cannot get back.
  if (!confirm(`Delete this note?\n\n${noteHeading(note.body)}`)) return;
  try {
    await api(`/api/notes/${note.id}`, { method: 'DELETE' });
    state.notes = state.notes.filter((n) => n.id !== note.id);
    renderNotes();
  } catch (err) {
    toast(err.message, 'error');
  }
}

// -- repo picker ------------------------------------------------------------

function repoRow(repo) {
  const badges = [
    el('span', { class: `badge ${repo.is_git ? 'git' : 'plain'}`, text: repo.is_git ? 'git' : 'plain' }),
  ];
  if (repo.branch) badges.push(el('span', { class: 'badge', text: repo.branch }));
  if (repo.dirty) badges.push(el('span', { class: 'badge dirty', text: 'dirty' }));
  // Its own git config declares commands, so nothing was run in it — and
  // spawning there is refused for the same reason.
  if (repo.refused) {
    badges.push(el('span', {
      class: 'badge dirty',
      text: 'not inspected',
      title: repo.refused,
    }));
  }
  return el(
    'div',
    {
      class: `repo${state.selectedRepo && state.selectedRepo.path === repo.path ? ' selected' : ''}`,
      onclick: () => selectRepo(repo),
    },
    [el('span', { class: 'name', text: repo.name }), ...badges],
  );
}

function renderRepos() {
  const list = $('repo-list');
  list.replaceChildren();
  if (state.repos.recent.length) {
    list.append(el('div', { class: 'group-label', text: 'Recent' }));
    for (const repo of state.repos.recent) list.append(repoRow(repo));
  }
  list.append(el('div', { class: 'group-label', text: 'All' }));
  for (const repo of state.repos.all) list.append(repoRow(repo));
  $('repo-errors').textContent = state.repos.errors.join(' · ');
}

async function selectRepo(repo) {
  if (repo.refused) {
    toast(repo.refused, 'error');
    return;
  }
  state.selectedRepo = repo;
  renderRepos();
  if (!$('task-name').value.trim()) $('task-name').value = repo.name;
  updateBranchPreview();

  const info = await api(`/api/repos/branches?path=${encodeURIComponent(repo.path)}`).catch(() => null);
  state.repoInfo = info;
  const base = $('base-ref');
  const existing = $('existing-branch');
  base.replaceChildren();
  existing.replaceChildren();
  if (info && info.is_git) {
    for (const branch of info.branches) {
      base.append(el('option', { value: branch, text: branch }));
      existing.append(el('option', { value: branch, text: branch }));
    }
    if (info.current) base.value = info.current;
  } else {
    base.append(el('option', { value: '', text: '(no VCS)' }));
    existing.append(el('option', { value: '', text: '(no VCS)' }));
  }
  renderBranchMode();
  renderSpawnWarnings(info);
}

// A repository with no branches to pick has nothing to reuse, and a folder
// with no VCS has no branch choice at all: each option is disabled where it
// cannot be honoured, and a selection that has just become impossible falls
// back to "new" rather than being left as a control that does nothing.
//
// "Stay on the current one" needs the main checkout — a worktree is a second
// checkout, which is precisely what it is asking us not to make — so it takes
// the Workspace control over and holds it there.
function renderBranchMode() {
  const info = state.repoInfo;
  const isGit = !!(info && info.is_git);
  const canReuse = isGit && !!info.branches.length;
  const source = $('branch-source');
  source.disabled = !isGit;
  source.querySelector('option[value="existing"]').disabled = !canReuse;
  if (!isGit || (source.value === 'existing' && !canReuse)) source.value = 'new';

  const reusing = source.value === 'existing';
  const untouched = source.value === 'none';
  $('new-branch-fields').classList.toggle('hidden', reusing || untouched);
  $('existing-branch-field').classList.toggle('hidden', !reusing);

  const isolation = $('isolation');
  if (untouched) isolation.value = 'in-place';
  isolation.disabled = untouched;
}

function renderSpawnWarnings(info) {
  const host = $('spawn-warnings');
  host.replaceChildren();
  if (!info) return;
  const source = $('branch-source').value;
  const inPlace = $('isolation').value === 'in-place';
  if (source === 'none') {
    host.append(el('div', {
      class: 'warnbox small',
      text: `No worktree and no new branch: the agent works directly in the main checkout on ${info.current || 'the current HEAD'}, so its changes land where you are working.`,
    }));
  }
  if (info.dirty && !inPlace) {
    host.append(el('div', {
      class: 'warnbox small',
      text: 'This checkout has uncommitted changes. A worktree starts from the branch head, so those changes will be invisible to the agent.',
    }));
  }
  if (info.dirty && inPlace && source === 'existing') {
    host.append(el('div', {
      class: 'warnbox small',
      text: 'Switching the main checkout to an existing branch with uncommitted changes: git will refuse if the switch would overwrite them.',
    }));
  }
  if (!info.is_git) {
    host.append(el('div', {
      class: 'warnbox small',
      text: 'Not a git repository. The agent runs in place with no branch — nothing is ever git init-ed for you.',
    }));
  }
}

function updateBranchPreview() {
  const prefix = state.config ? state.config.branch_prefix : 'sw_';
  const repo = state.selectedRepo;
  const name = $('task-name').value.trim();
  if (!repo || !repo.is_git) {
    $('branch-preview').value = repo ? '(no VCS — no branch)' : '';
    return;
  }
  if ($('branch-source').value === 'none') {
    $('branch-preview').value = '(no branch — the current one is kept)';
    return;
  }
  $('branch-preview').value = prefix + slugify(name || repo.name);
}

async function loadRepos() {
  state.repos = await api('/api/repos');
  renderRepos();
  const rootSelect = $('clone-root');
  rootSelect.replaceChildren();
  const roots = state.config ? state.config.repo_roots : [];
  for (const root of roots) rootSelect.append(el('option', { value: root, text: root }));
}

// -- spawn ------------------------------------------------------------------

async function spawn() {
  if (!state.selectedRepo) {
    toast('Pick a repository first', 'warn');
    return;
  }
  const budget = parseFloat($('budget').value);
  const source = $('branch-source').value;
  const reusing = source === 'existing';
  const untouched = source === 'none';
  const body = {
    repo_path: state.selectedRepo.path,
    task_name: $('task-name').value.trim() || state.selectedRepo.name,
    // A reused branch has its own head; a base ref would only move it, so it is
    // not sent at all rather than sent and ignored.
    base_ref: reusing || untouched ? null : $('base-ref').value || null,
    existing_branch: reusing ? $('existing-branch').value || null : null,
    no_branch: untouched,
    model: $('model').value.trim() || null,
    effort: $('effort').value.trim() || null,
    max_budget_usd: Number.isFinite(budget) ? budget : null,
    permission_mode: $('permission-mode').value,
    // Staying on the current branch means the main checkout by definition; the
    // control is held there, but the flag is sent explicitly all the same.
    in_place: untouched || $('isolation').value === 'in-place',
    add_dirs: $('add-dirs').value.split('\n').map((s) => s.trim()).filter(Boolean),
    first_message: $('first-message').value.trim() || null,
  };
  const button = $('spawn-btn');
  button.disabled = true;
  try {
    const data = await api('/api/agents', { method: 'POST', body: JSON.stringify(body) });
    if (data.warning) toast(data.warning, 'warn');
    $('first-message').value = '';
    location.href = `/agent/${data.agent.slug}`;
  } catch (err) {
    toast(err.message, 'error');
  } finally {
    button.disabled = false;
  }
}

// -- clone ------------------------------------------------------------------

async function startClone() {
  const url = $('clone-url').value.trim();
  if (!url) return;
  $('clone-log').textContent = '';
  try {
    const data = await api('/api/repos/clone', {
      method: 'POST',
      body: JSON.stringify({
        url,
        root: $('clone-root').value || null,
        folder: $('clone-folder').value.trim() || null,
      }),
    });
    state.cloneId = data.clone_id;
  } catch (err) {
    toast(err.message, 'error');
  }
}

// The spawn form opens on the configured default rather than on whatever the
// markup happens to list first. The server applies the same default to a
// request that omits the mode, so the picker and that fallback agree; a
// hand-edited config naming a mode this build does not offer is left alone
// rather than blanking the control.
function applySpawnDefaults() {
  const cfg = state.config;
  if (!cfg) return;
  const mode = $('permission-mode');
  if ([...mode.options].some((option) => option.value === cfg.default_permission_mode)) {
    mode.value = cfg.default_permission_mode;
  }
  $('model').placeholder = cfg.default_model || 'opus';
}

// -- settings ---------------------------------------------------------------

function fillSettings() {
  const cfg = state.config;
  if (!cfg) return;
  $('cfg-roots').value = cfg.repo_roots.join('\n');
  $('cfg-prefix').value = cfg.branch_prefix;
  $('cfg-max').value = cfg.max_agents;
  $('cfg-model').value = cfg.default_model;
  $('cfg-bin').value = cfg.claude_bin;
  $('cfg-mode').value = cfg.default_permission_mode;
  $('cfg-pin').value = cfg.pinned_cli_version;
  $('cfg-open').checked = cfg.open_browser;
}

async function saveSettings() {
  const cfg = {
    ...state.config,
    repo_roots: $('cfg-roots').value.split('\n').map((s) => s.trim()).filter(Boolean),
    branch_prefix: $('cfg-prefix').value,
    max_agents: parseInt($('cfg-max').value, 10) || 8,
    default_model: $('cfg-model').value,
    claude_bin: $('cfg-bin').value,
    default_permission_mode: $('cfg-mode').value,
    pinned_cli_version: $('cfg-pin').value,
    open_browser: $('cfg-open').checked,
  };
  try {
    state.config = await api('/api/config', { method: 'PUT', body: JSON.stringify(cfg) });
    toast('Settings saved');
    applySpawnDefaults();
    await loadRepos();
    updateBranchPreview();
  } catch (err) {
    toast(err.message, 'error');
  }
}

// -- wiring -----------------------------------------------------------------

function togglePanel(id) {
  const panel = $(id);
  panel.classList.toggle('hidden');
  if (id === 'spawn-panel' && !panel.classList.contains('hidden')) loadRepos();
}

async function main() {
  state.config = await api('/api/config').catch(() => null);
  fillSettings();
  applySpawnDefaults();
  await loadAgents();
  await loadLimits();
  state.notesOpen = readNotesOpen();
  await loadNotes();
  await loadRepos();
  // Nothing is reusable until a repository is picked, so the control starts
  // disabled rather than offering an empty list.
  renderBranchMode();

  $('toggle-spawn').onclick = () => togglePanel('spawn-panel');
  $('toggle-clone').onclick = () => togglePanel('clone-panel');
  $('toggle-settings').onclick = () => togglePanel('settings-panel');
  $('notes-toggle').onclick = () => setNotesOpen(!state.notesOpen);
  $('note-new').onclick = newNote;
  $('task-name').oninput = updateBranchPreview;
  $('isolation').onchange = () => renderSpawnWarnings(state.repoInfo);
  $('branch-source').onchange = () => {
    renderBranchMode();
    updateBranchPreview();
    renderSpawnWarnings(state.repoInfo);
  };
  $('spawn-btn').onclick = spawn;
  $('clone-btn').onclick = startClone;
  $('cfg-save').onclick = saveSettings;
  $('fetch-btn').onclick = async () => {
    if (!state.selectedRepo) return;
    try {
      await api('/api/repos/fetch', {
        method: 'POST',
        body: JSON.stringify({ path: state.selectedRepo.path }),
      });
      toast('Fetched');
      await selectRepo(state.selectedRepo);
    } catch (err) {
      toast(err.message, 'error');
    }
  };
  $('clone-url').oninput = () => {
    if ($('clone-folder').dataset.touched) return;
    const match = $('clone-url').value.trim().replace(/\/$/, '').split(/[/:]/).pop();
    $('clone-folder').value = (match || '').replace(/\.git$/, '');
  };
  $('clone-folder').oninput = () => { $('clone-folder').dataset.touched = '1'; };

  const socket = new Socket();
  socket.onopen = () => { $('conn').textContent = 'live'; };
  socket
    .on('status', (msg) => {
      const agent = state.agents.get(msg.agent_id);
      if (!agent) return;
      Object.assign(agent, {
        status: msg.status,
        status_detail: msg.status_detail,
        exit_code: msg.exit_code,
        last_stderr: msg.last_stderr ?? agent.last_stderr,
        cost_usd: msg.cost_usd,
        last_active_at: Date.now(),
      });
      renderAgents();
    })
    .on('agent_added', (msg) => {
      state.agents.set(msg.agent.id, msg.agent);
      renderAgents();
    })
    .on('agent_renamed', (msg) => {
      const agent = state.agents.get(msg.agent_id);
      if (!agent) return;
      agent.name = msg.name;
      renderAgents();
    })
    .on('agent_removed', (msg) => {
      state.agents.delete(msg.agent_id);
      renderAgents();
    })
    .on('permission_request', (msg) => {
      const agent = state.agents.get(msg.agent_id);
      toast(`${agent ? agent.name : msg.agent_id} needs approval for ${msg.request.tool_name}`, 'warn');
    })
    .on('permission_mode_changed', (msg) => {
      const agent = state.agents.get(msg.agent_id);
      if (!agent) return;
      agent.permission_mode = msg.mode;
      renderAgents();
    })
    .on('rate_limit', (msg) => {
      state.rateLimit = msg.info;
      state.rateLimitAt = Date.now();
      renderLimits();
    })
    .on('notice', (msg) => toast(msg.text, msg.level))
    .on('clone_progress', (msg) => {
      const log = $('clone-log');
      log.textContent = `${log.textContent}${msg.line}\n`.split('\n').slice(-12).join('\n');
    })
    .on('clone_done', async (msg) => {
      if (msg.error) {
        $('clone-log').textContent += `\n${msg.error}\n`;
        toast('Clone failed', 'error');
      } else {
        toast(`Cloned into ${msg.path}`);
        await loadRepos();
      }
    });

  // Keep relative timestamps honest without a full re-render storm, and pick up
  // notes written in another tab — or on a phone over Tailscale. Notes ride
  // this poll rather than the socket: a wide broadcast would be real protocol
  // work in service of a list of tens of rows.
  setInterval(() => {
    renderAgents();
    renderLimits();
    loadNotes();
  }, 30000);
}

main().catch((err) => toast(err.message, 'error'));
