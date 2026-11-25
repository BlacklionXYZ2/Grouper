// A small Markdown subset rendered straight to DOM nodes (never via HTML strings):
// paragraphs, line breaks, headings, fenced and indented code, block quotes,
// ordered and unordered lists (nested by indentation), rules, inline code,
// bold, italic, strikethrough, links and bare URLs. Links only for http(s) and
// mailto (javascript:, data: and the rest stay text), opened with
// rel="noopener noreferrer". A link's text never holds another link, URLs may
// hold one level of balanced parentheses, and nesting is capped so hostile input
// can't recurse deeply.

const SAFE_URL = /^(https?:\/\/|mailto:)/i;
/** Deepest block nesting (quotes, lists) rendered as structure; deeper is text. */
const MAX_BLOCK_DEPTH = 8;
/** Deepest inline nesting (emphasis in emphasis …); deeper is text. */
const MAX_INLINE_DEPTH = 8;

export function renderMarkdown(text) {
  const root = document.createDocumentFragment();
  const lines = String(text ?? '').replace(/\r\n?/g, '\n').split('\n');
  renderBlocks(lines, root, 0);
  return root;
}

function renderBlocks(lines, parent, depth) {
  if (depth > MAX_BLOCK_DEPTH) {
    const p = document.createElement('p');
    p.textContent = lines.join('\n');
    parent.append(p);
    return;
  }
  let i = 0;
  while (i < lines.length) {
    const line = lines[i];
    if (!line.trim()) {
      i++;
      continue;
    }
    // Fenced code.
    const fence = line.match(/^\s*(`{3,}|~{3,})\s*([\w+#.-]*)\s*$/);
    if (fence) {
      const close = fence[1];
      const body = [];
      i++;
      while (i < lines.length && !lines[i].trim().startsWith(close)) body.push(lines[i++]);
      i++;
      parent.append(codeBlock(body.join('\n'), fence[2]));
      continue;
    }
    // Heading.
    const heading = line.match(/^(#{1,6})\s+(.*?)\s*#*\s*$/);
    if (heading) {
      const level = Math.min(6, heading[1].length + 2);
      const el = document.createElement(`h${level}`);
      el.className = 'md-h';
      inline(heading[2], el);
      parent.append(el);
      i++;
      continue;
    }
    // Rule.
    if (/^\s*([-*_])(\s*\1){2,}\s*$/.test(line)) {
      parent.append(document.createElement('hr'));
      i++;
      continue;
    }
    // Block quote.
    if (/^\s*>/.test(line)) {
      const body = [];
      while (i < lines.length && /^\s*>/.test(lines[i])) body.push(lines[i++].replace(/^\s*>\s?/, ''));
      const quote = document.createElement('blockquote');
      renderBlocks(body, quote, depth + 1);
      parent.append(quote);
      continue;
    }
    // List.
    if (listItem(line)) {
      i = renderList(lines, i, parent, depth);
      continue;
    }
    // Indented code (4 spaces or a tab), only at the start of a block.
    if (/^( {4}|\t)/.test(line)) {
      const body = [];
      while (i < lines.length && (/^( {4}|\t)/.test(lines[i]) || !lines[i].trim())) body.push(lines[i++].replace(/^( {4}|\t)/, ''));
      while (body.length && !body[body.length - 1].trim()) body.pop();
      parent.append(codeBlock(body.join('\n'), ''));
      continue;
    }
    // Paragraph: until a blank line or another block starts.
    const para = [];
    while (
      i < lines.length &&
      lines[i].trim() &&
      !/^\s*(`{3,}|~{3,})/.test(lines[i]) &&
      !/^#{1,6}\s/.test(lines[i]) &&
      !/^\s*>/.test(lines[i]) &&
      !(para.length && listItem(lines[i]))
    ) {
      para.push(lines[i++]);
    }
    const p = document.createElement('p');
    para.forEach((l, n) => {
      if (n) p.append(document.createElement('br'));
      inline(l.trim(), p);
    });
    parent.append(p);
  }
}

function listItem(line) {
  const m = line.match(/^(\s*)([-*+]|\d{1,9}[.)])\s+(.*)$/);
  if (!m) return null;
  return { indent: m[1].replace(/\t/g, '    ').length, ordered: /\d/.test(m[2]), start: parseInt(m[2], 10), text: m[3] };
}

function renderList(lines, i, parent, depth) {
  const first = listItem(lines[i]);
  const list = document.createElement(first.ordered ? 'ol' : 'ul');
  if (first.ordered && first.start !== 1) list.setAttribute('start', String(first.start));
  parent.append(list);
  let li = null;
  while (i < lines.length) {
    const line = lines[i];
    const item = listItem(line);
    if (item && item.indent <= first.indent + 1 && item.ordered === first.ordered) {
      li = document.createElement('li');
      inline(item.text, li);
      list.append(li);
      i++;
      continue;
    }
    if (item && item.indent > first.indent + 1 && li) {
      // Nested list inside the current item.
      const nested = [];
      while (i < lines.length) {
        const n = listItem(lines[i]);
        if (n && n.indent <= first.indent + 1) break;
        if (!n && !/^\s{2,}\S/.test(lines[i])) break;
        nested.push(lines[i].slice(Math.min(item.indent, lines[i].length - lines[i].trimStart().length)));
        i++;
      }
      renderBlocks(nested, li, depth + 1);
      continue;
    }
    if (!item && li && /^\s{2,}\S/.test(line)) {
      // Continuation line of the current item.
      li.append(document.createElement('br'));
      inline(line.trim(), li);
      i++;
      continue;
    }
    break;
  }
  return i;
}

function codeBlock(text, lang) {
  const pre = document.createElement('pre');
  pre.className = 'md-code';
  const code = document.createElement('code');
  if (lang) code.dataset.lang = lang;
  code.textContent = text;
  pre.append(code);
  return pre;
}

// A URL: no spaces or angle brackets, with at most one level of balanced
// parentheses, e.g. https://en.wikipedia.org/wiki/Rust_(programming_language).
const URL_PART = String.raw`(?:[^\s()<>]|\([^\s()<>]*\))+`;

// Inline: the earliest match wins; emphasis and link text recurse.
// `ctx` is {inLink, depth}: inside a link's text, links render as plain text.
const INLINE = [
  { re: /(`+)([\s\S]+?)\1/, make: (m) => el('code', m[2].replace(/^ (.*) $/, '$1')) },
  { re: new RegExp(String.raw`\[([^\]\n]+)\]\(\s*(?:<([^>\s]+)>|(${URL_PART}))(?:\s+"[^"]*")?\s*\)`), make: (m, ctx) => link(m[2] || m[3], m[1], ctx) },
  { re: /<(https?:\/\/[^>\s]+)>/, make: (m, ctx) => link(m[1], null, ctx) },
  { re: /\*\*(?=\S)([\s\S]+?\S)\*\*/, make: (m, ctx) => wrap('strong', m[1], ctx) },
  { re: /__(?=\S)([\s\S]+?\S)__(?!\w)/, make: (m, ctx) => wrap('strong', m[1], ctx) },
  { re: /~~(?=\S)([\s\S]+?\S)~~/, make: (m, ctx) => wrap('del', m[1], ctx) },
  { re: /\*(?=[^\s*])([\s\S]*?[^\s*])\*/, make: (m, ctx) => wrap('em', m[1], ctx) },
  { re: /(?<![\w])_(?=\S)([\s\S]*?\S)_(?![\w])/, make: (m, ctx) => wrap('em', m[1], ctx) },
  // A bare URL ends before trailing punctuation (and an unbalanced ")").
  { re: /\bhttps?:\/\/(?:[^\s<>"'()]|\([^\s<>"'()]*\))*(?:[^\s<>"'().,;:!?\]]|\([^\s<>"'()]*\))/, make: (m, ctx) => link(m[0], null, ctx) },
];

function inline(text, parent, ctx = { inLink: false, depth: 0 }) {
  if (ctx.depth > MAX_INLINE_DEPTH) {
    parent.append(document.createTextNode(unescape(text)));
    return;
  }
  let rest = text;
  while (rest) {
    let best = null;
    for (const rule of INLINE) {
      const m = rule.re.exec(rest);
      if (m && (best === null || m.index < best.m.index)) best = { m, rule };
    }
    if (!best) {
      parent.append(document.createTextNode(unescape(rest)));
      break;
    }
    if (best.m.index) parent.append(document.createTextNode(unescape(rest.slice(0, best.m.index))));
    parent.append(best.rule.make(best.m, ctx));
    rest = rest.slice(best.m.index + best.m[0].length);
  }
}

function unescape(s) {
  return s.replace(/\\([\\`*_{}[\]()#+\-.!~>|])/g, '$1');
}

function el(tag, text) {
  const e = document.createElement(tag);
  e.textContent = text;
  return e;
}

function wrap(tag, inner, ctx) {
  const e = document.createElement(tag);
  inline(inner, e, { ...ctx, depth: ctx.depth + 1 });
  return e;
}

/** A link, or its text when the URL isn't allowed or we're already inside a link. */
function link(url, label, ctx) {
  const inner = { inLink: true, depth: ctx.depth + 1 };
  if (ctx.inLink || !SAFE_URL.test(url)) {
    const span = document.createElement('span');
    if (label) inline(label, span, inner);
    else span.textContent = url;
    return span;
  }
  const a = document.createElement('a');
  a.href = url;
  a.rel = 'noopener noreferrer';
  a.target = '_blank';
  if (label) inline(label, a, inner);
  else a.textContent = url;
  return a;
}
