// Model family: a colour accent plus a short glyph, so the owner can tell at a
// glance which kind of model is doing what. Used by the thread rows, the thread
// view's fixed model chip and the model picker.
//
// The family comes from the registry entry's provider model string (then its id),
// matched on clear name prefixes only. Anything else stays Unknown (no guessed
// name or glyph). Its colour is a hash of the model id onto four vivid tokens,
// so two unknown models can still be told apart. The name beside the mark is
// the fact; the colour is only a supplement.
// Colours live in app.css (--fam-<key>); the common families sit on the concept
// art's set: Gemini/Gemma teal, Qwen purple, Llama orange, Radium indigo.

import { h, icon } from './dom.js';
import { store } from './store.js';

/** key -> label (for people), maker, and a two-letter glyph (the non-colour cue). */
export const FAMILIES = {
  claude: { label: 'Claude', maker: 'Anthropic', glyph: 'Cl' },
  gpt: { label: 'GPT', maker: 'OpenAI', glyph: 'GP' },
  gemini: { label: 'Gemini', maker: 'Google', glyph: 'Ge' },
  llama: { label: 'Llama', maker: 'Meta', glyph: 'Ll' },
  qwen: { label: 'Qwen', maker: 'Alibaba', glyph: 'Qw' },
  deepseek: { label: 'DeepSeek', maker: 'DeepSeek', glyph: 'DS' },
  mistral: { label: 'Mistral', maker: 'Mistral AI', glyph: 'Mi' },
  nemotron: { label: 'Nemotron', maker: 'NVIDIA', glyph: 'Nv' },
  ernie: { label: 'ERNIE', maker: 'Baidu', glyph: 'Er' },
  grok: { label: 'Grok', maker: 'xAI', glyph: 'Gk' },
  radium: { label: 'Radium', maker: 'Radium', glyph: 'Ra' },
  scripted: { label: 'Scripted', maker: 'test', glyph: 'T' },
  unknown: { label: 'Unknown family', maker: '', glyph: '' },
};

// Matched against the model's own name: the part after the last "/" (so
// "google/gemma-3-27b-it:free" is tested as "gemma-3-27b-it"), lower-cased.
// Order matters only where one name holds another family's word (Nemotron
// models built on Llama say "nemotron" explicitly, so that is checked first).
const RULES = [
  ['nemotron', /^nemotron(?:[-_.\d]|$)|-nemotron(?:[-_.]|$)/],
  ['claude', /^claude(?:[-_.\d]|$)|^(?:(?:us|eu|apac|global)\.)?anthropic\.claude/],
  ['gpt', /^gpt(?:[-_.\d]|$)|^chatgpt(?:[-_.]|$)|^o[134](?:-|$)/],
  ['gemini', /^gemini(?:[-_.\d]|$)|^gemma(?:[-_.\d]|$)/],
  ['llama', /^llama(?:[-_.\d]|$)|^meta-llama(?:[-_.]|$)|^codellama(?:[-_.]|$)/],
  ['qwen', /^qwen(?:[-_.\d]|$)|^qwq(?:[-_.\d]|$)/],
  ['deepseek', /^deepseek(?:[-_.]|$)/],
  ['mistral', /^(?:mistral|mixtral|codestral|ministral|magistral|devstral|pixtral)(?:[-_.]|$)/],
  ['ernie', /^ernie(?:[-_.\d]|$)/],
  ['grok', /^grok(?:[-_.\d]|$)/],
  ['radium', /^(?:tycho|hal|clarke)(?:[-_.\d]|$)/],
  ['scripted', /^(?:fake|scripted)(?:[-_.\d]|$)/],
];

function nameOf(s) {
  const last = String(s || '').trim().toLowerCase().split('/').pop() || '';
  return last.replace(/:[a-z0-9-]+$/, ''); // ":free", ":beta" variants
}

function match(s) {
  const name = nameOf(s);
  if (!name) return null;
  for (const [key, re] of RULES) if (re.test(name)) return key;
  return null;
}

// Pink, green, amber, indigo. CSS maps these onto the existing family tokens.
const VIVID = ['vivid-claude', 'vivid-nemotron', 'vivid-ernie', 'vivid-radium'];

function isVivid(key) {
  return typeof key === 'string' && key.startsWith('vivid-');
}

/** Stable hue for an unmatched model id. FNV-1a, so the same id always lands. */
function vividKey(id) {
  let h = 2166136261;
  const s = String(id);
  for (let i = 0; i < s.length; i++) {
    h ^= s.charCodeAt(i);
    h = Math.imul(h, 16777619);
  }
  return VIVID[(h >>> 0) % VIVID.length];
}

/**
 * The family key of a registry entry `{id, model}` (or of a bare model string).
 * The provider model string wins; the registry id is the fallback.
 */
export function familyKey(entry) {
  if (!entry) return 'unknown';
  if (typeof entry === 'string') return match(entry) || (nameOf(entry) ? vividKey(entry.trim()) : 'unknown');
  const known = match(entry.model) || match(entry.id);
  if (known) return known;
  const id = String(entry.id || '').trim();
  if (id) return vividKey(id);
  const model = String(entry.model || '').trim();
  return model ? vividKey(model) : 'unknown';
}

/** Family of a registry id as the hub reports it on a thread (`thread.model`). */
export function familyOfId(id) {
  const entry = store.models.find((m) => m.id === id);
  return familyKey(entry || id);
}

export function familyInfo(key) {
  // A hashed hue is not a family: empty label and glyph, so callers keep
  // saying "unknown" and drawing the generic mark.
  if (isVivid(key)) return { key, label: '', maker: '', glyph: '' };
  return { key, ...(FAMILIES[key] || FAMILIES.unknown) };
}

/**
 * The family mark: a small box in the family colour with its glyph (Unknown gets
 * a generic model icon). Decorative for assistive tech: the text beside it says
 * the model, and `familyLabel` gives screen readers the family.
 */
export function familyMark(key) {
  const f = familyInfo(key);
  return h(
    'span',
    { class: ['fam-mark', `fam-${f.key}`], 'aria-hidden': 'true', title: FAMILIES[f.key] && f.key !== 'unknown' ? `${f.label} family` : 'Family unknown' },
    f.glyph || icon('models'),
  );
}

/**
 * The family's colour dot (thread rows, the threads card). Decorative: the row
 * names the model in text. Pair with `statusPill(status, { family: key })` so the
 * dot and the Running pill share a hue.
 */
export function familyDot(key) {
  const f = familyInfo(key);
  const colour = FAMILIES[f.key] || isVivid(f.key) ? f.key : 'unknown';
  return h('span', { class: ['fam-dot', `fam-${colour}`], 'aria-hidden': 'true' });
}

/** Words for screen readers, e.g. "Claude family" or "family unknown". */
export function familyLabel(key) {
  const f = familyInfo(key);
  if (!FAMILIES[key] || key === 'unknown') return 'family unknown';
  return `${f.label} family`;
}

/** Point an element's family class at `key` (keeps other classes). */
export function setFamilyClass(el, key) {
  for (const c of [...el.classList]) if (c.startsWith('fam-') && c !== 'fam-mark') el.classList.remove(c);
  el.classList.add(`fam-${key}`);
}
