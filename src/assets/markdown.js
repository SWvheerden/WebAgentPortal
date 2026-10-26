// Markdown, parsed to a plain tree that agent.js turns into DOM nodes.
//
// Deliberately free of the DOM, so it can be exercised under node, and
// deliberately never producing HTML: the text is the model's, the page builds
// every node through `el()` with `textContent`, and so there is no sanitiser
// to own. Raw HTML in the source is shown as text. A link survives only with
// an http(s) or mailto target; anything else stays as the text it was written
// as.
//
// It covers what the model writes — headings, paragraphs, fenced code, lists,
// block quotes, rules, pipe tables, and the inline marks — not the CommonMark
// corner cases. A single newline inside a paragraph is a line break, as in the
// Claude apps, rather than a space.
//
// Block nodes:  {t:'p', c} {t:'h', level, c} {t:'code', lang, text}
//               {t:'ul'|'ol', start, items:[blocks]} {t:'quote', c:blocks}
//               {t:'hr'} {t:'table', align, head:[cells], rows:[[cells]]}
// Inline nodes: {t:'text', v} {t:'code', v} {t:'strong'|'em'|'del', c}
//               {t:'a', href, c} {t:'br'}

const FENCE = /^ {0,3}(`{3,}|~{3,})\s*([^`\s]*)[^`]*$/;
const HEADING = /^ {0,3}(#{1,6})(?:\s+(.*?))?\s*#*\s*$/;
const RULE = /^ {0,3}([-*_])(?:\s*\1){2,}\s*$/;
const QUOTE = /^ {0,3}>\s?(.*)$/;
const ITEM = /^( *)([-*+]|\d{1,9}[.)])(?:\s+(.*))?$/;
const TABLE_RULE = /^\s*\|?\s*:?-+:?\s*(\|\s*:?-+:?\s*)*\|?\s*$/;

const blank = (line) => !line.trim();
const indentOf = (line) => line.match(/^ */)[0].length;

export function parse(source) {
  const text = String(source ?? '').replace(/\r\n?/g, '\n').replace(/\t/g, '    ');
  return blocks(text.split('\n'));
}

function startsBlock(line) {
  return FENCE.test(line) || HEADING.test(line) || RULE.test(line) || QUOTE.test(line)
    || ITEM.test(line);
}

function blocks(lines) {
  const out = [];
  let i = 0;
  while (i < lines.length) {
    const line = lines[i];
    if (blank(line)) { i += 1; continue; }

    let m = line.match(FENCE);
    if (m) {
      const marker = m[1];
      const close = new RegExp(`^ {0,3}${marker[0]}{${marker.length},}\\s*$`);
      const body = [];
      i += 1;
      while (i < lines.length && !close.test(lines[i])) body.push(lines[i++]);
      i += 1; // the closing fence, or past the end of an unclosed one
      out.push({ t: 'code', lang: m[2] || '', text: body.join('\n') });
      continue;
    }

    m = line.match(HEADING);
    if (m) {
      out.push({ t: 'h', level: m[1].length, c: inline(m[2] || '') });
      i += 1;
      continue;
    }

    if (RULE.test(line)) { out.push({ t: 'hr' }); i += 1; continue; }

    if (QUOTE.test(line)) {
      const body = [];
      while (i < lines.length && !blank(lines[i])) {
        const q = lines[i].match(QUOTE);
        // A line without `>` is a lazy continuation of the quoted paragraph.
        body.push(q ? q[1] : lines[i]);
        i += 1;
      }
      out.push({ t: 'quote', c: blocks(body) });
      continue;
    }

    m = line.match(ITEM);
    if (m) {
      const [list, next] = listAt(lines, i);
      out.push(list);
      i = next;
      continue;
    }

    if (line.includes('|') && i + 1 < lines.length && TABLE_RULE.test(lines[i + 1])
      && lines[i + 1].includes('-')) {
      const [table, next] = tableAt(lines, i);
      if (table) { out.push(table); i = next; continue; }
    }

    const para = [];
    while (i < lines.length && !blank(lines[i]) && (para.length === 0 || !startsBlock(lines[i]))) {
      para.push(lines[i].trim());
      i += 1;
    }
    out.push({ t: 'p', c: inline(para.join('\n')) });
  }
  return out;
}

function listAt(lines, start) {
  const first = lines[start].match(ITEM);
  const base = first[1].length;
  const ordered = /\d/.test(first[2]);
  const list = { t: ordered ? 'ol' : 'ul', start: ordered ? parseInt(first[2], 10) : 1, items: [] };
  let i = start;
  while (i < lines.length) {
    const m = lines[i].match(ITEM);
    if (!m || m[1].length !== base || /\d/.test(m[2]) !== ordered) break;
    // Continuation lines are those indented to at least the marker's width;
    // anything less starts something else.
    const inner = base + m[2].length + 1;
    const body = [m[3] || ''];
    i += 1;
    while (i < lines.length) {
      const line = lines[i];
      if (blank(line)) {
        // A blank line ends the item unless what follows is still indented
        // into it.
        let j = i;
        while (j < lines.length && blank(lines[j])) j += 1;
        if (j < lines.length && indentOf(lines[j]) >= inner) {
          for (; i < j; i += 1) body.push('');
          continue;
        }
        break;
      }
      const sibling = line.match(ITEM);
      if (sibling && sibling[1].length <= base) break;
      if (indentOf(line) >= Math.min(inner, base + 2)) {
        body.push(line.slice(Math.min(indentOf(line), inner)));
      } else if (!startsBlock(line)) {
        body.push(line.trim()); // lazy continuation of the item's paragraph
      } else {
        break;
      }
      i += 1;
    }
    list.items.push(blocks(body));
    // A blank line between two items of the same list does not end it.
    let j = i;
    while (j < lines.length && blank(lines[j])) j += 1;
    const again = j < lines.length && lines[j].match(ITEM);
    if (j > i && again && again[1].length === base && /\d/.test(again[2]) === ordered) i = j;
  }
  return [list, i];
}

function cells(line) {
  let row = line.trim();
  if (row.startsWith('|')) row = row.slice(1);
  if (row.endsWith('|') && !row.endsWith('\\|')) row = row.slice(0, -1);
  const out = [];
  let cell = '';
  let code = false;
  for (let k = 0; k < row.length; k += 1) {
    const ch = row[k];
    if (ch === '\\' && row[k + 1] === '|') { cell += '|'; k += 1; continue; }
    if (ch === '`') code = !code;
    if (ch === '|' && !code) { out.push(cell.trim()); cell = ''; continue; }
    cell += ch;
  }
  out.push(cell.trim());
  return out;
}

function tableAt(lines, start) {
  const head = cells(lines[start]);
  const rule = cells(lines[start + 1]);
  if (rule.length !== head.length) return [null, start];
  const align = rule.map((r) => {
    const left = r.startsWith(':');
    const right = r.endsWith(':');
    if (left && right) return 'center';
    if (right) return 'right';
    if (left) return 'left';
    return null;
  });
  const rows = [];
  let i = start + 2;
  while (i < lines.length && !blank(lines[i]) && lines[i].includes('|')) {
    const row = cells(lines[i]).slice(0, head.length);
    while (row.length < head.length) row.push('');
    rows.push(row.map(inline));
    i += 1;
  }
  return [{ t: 'table', align, head: head.map(inline), rows }, i];
}

// -- inline -----------------------------------------------------------------

const SAFE_HREF = /^(https?:\/\/|mailto:)/i;

export function inline(source) {
  const out = [];
  let text = '';
  const flush = () => {
    if (text) out.push({ t: 'text', v: text });
    text = '';
  };
  const s = String(source ?? '');
  let i = 0;
  while (i < s.length) {
    const ch = s[i];

    if (ch === '\\' && i + 1 < s.length && /[!-/:-@[-`{-~]/.test(s[i + 1])) {
      text += s[i + 1];
      i += 2;
      continue;
    }

    if (ch === '\n') { flush(); out.push({ t: 'br' }); i += 1; continue; }

    if (ch === '`') {
      const run = s.slice(i).match(/^`+/)[0];
      const end = s.indexOf(run, i + run.length);
      if (end !== -1) {
        flush();
        let v = s.slice(i + run.length, end).replace(/\n/g, ' ');
        if (/^ .* $/.test(v) && v.trim()) v = v.slice(1, -1);
        out.push({ t: 'code', v });
        i = end + run.length;
        continue;
      }
      text += run;
      i += run.length;
      continue;
    }

    if (ch === '[') {
      const link = s.slice(i).match(/^\[([^\]\n]*)\]\(\s*<?([^\s<>()]+(?:\([^\s()]*\))?[^\s<>()]*)>?(?:\s+"[^"]*")?\s*\)/);
      if (link && SAFE_HREF.test(link[2])) {
        flush();
        out.push({ t: 'a', href: link[2], c: inline(link[1]) });
        i += link[0].length;
        continue;
      }
    }

    if (ch === 'h' && /^https?:\/\//.test(s.slice(i, i + 8)) && !/[\w/]/.test(s[i - 1] || '')) {
      let url = s.slice(i).match(/^https?:\/\/[^\s<>"`]+/)[0];
      url = url.replace(/[.,;:!?)\]'*_]+$/, (tail) => {
        // Keep a closing paren that balances one in the URL itself.
        const opens = (url.match(/\(/g) || []).length;
        const closes = (url.match(/\)/g) || []).length - (tail.match(/\)/g) || []).length;
        return opens > closes && tail.startsWith(')') ? ')' : '';
      });
      flush();
      out.push({ t: 'a', href: url, c: [{ t: 'text', v: url }] });
      i += url.length;
      continue;
    }

    if (ch === '~' && s[i + 1] === '~') {
      const end = s.indexOf('~~', i + 2);
      if (end > i + 2) {
        flush();
        out.push({ t: 'del', c: inline(s.slice(i + 2, end)) });
        i = end + 2;
        continue;
      }
    }

    if (ch === '*' || ch === '_') {
      const span = emphasis(s, i);
      if (span) {
        flush();
        out.push({ t: span.t, c: inline(span.inner) });
        i = span.end;
        continue;
      }
    }

    text += ch;
    i += 1;
  }
  flush();
  return out;
}

/// `**strong**`, `*em*`, `***both***` and the underscore forms, if the run at
/// `i` opens one that closes. Underscores inside a word — `snake_case_name` —
/// are left alone, as GFM does.
function emphasis(s, i) {
  const ch = s[i];
  const run = s.slice(i).match(ch === '*' ? /^\*+/ : /^_+/)[0];
  const after = s[i + run.length];
  if (!after || /\s/.test(after)) return null;
  if (ch === '_' && /\w/.test(s[i - 1] || '')) return null;
  for (const n of run.length >= 3 ? [3, 2, 1] : run.length === 2 ? [2, 1] : [1]) {
    const mark = ch.repeat(n);
    let from = i + n;
    for (;;) {
      const end = s.indexOf(mark, from);
      if (end === -1) break;
      const before = s[end - 1];
      const next = s[end + n];
      const okClose = end > i + n && !/\s/.test(before)
        && (ch === '*' || !/\w/.test(next || ''))
        // `**a**` must not close at the first `*` of `**`.
        && next !== ch;
      if (okClose) {
        const inner = s.slice(i + n, end);
        if (n === 3) return { t: 'strong', inner: `${ch}${inner}${ch}`, end: end + 3 };
        return { t: n === 2 ? 'strong' : 'em', inner, end: end + n };
      }
      from = end + 1;
    }
  }
  return null;
}
