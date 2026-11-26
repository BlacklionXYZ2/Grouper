// Model family: a colour accent plus a short glyph, so the owner can tell at a
// glance which kind of model is doing what. Used by the thread rows, the thread
// view's fixed model chip and the model picker.
//
// The family comes from the registry entry's provider model string (then its id),
// matched on clear name prefixes only. Anything else is Unknown: never a guess.
// The colour is a supplement. The model's name is always shown as text beside it.

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

/**
 * The family key of a registry entry `{id, model}` (or of a bare model string).
 * The provider model string wins; the registry id is the fallback.
 */
export function familyKey(entry) {
  if (!entry) return 'unknown';
  if (typeof entry === 'string') return match(entry) || 'unknown';
  return match(entry.model) || match(entry.id) || 'unknown';
}

/** Family of a registry id as the hub reports it on a thread (`thread.model`). */
export function familyOfId(id) {
  const entry = store.models.find((m) => m.id === id);
  return familyKey(entry || id);
}

export function familyInfo(key) {
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
    { class: ['fam-mark', `fam-${f.key}`], 'aria-hidden': 'true', title: f.key === 'unknown' ? 'Family unknown' : `${f.label} family` },
    f.glyph || icon('models'),
  );
}

/** Words for screen readers, e.g. "Claude family" or "family unknown". */
export function familyLabel(key) {
  const f = familyInfo(key);
  return f.key === 'unknown' ? 'family unknown' : `${f.label} family`;
}

/** Point an element's family class at `key` (keeps other classes). */
export function setFamilyClass(el, key) {
  for (const c of [...el.classList]) if (c.startsWith('fam-') && c !== 'fam-mark') el.classList.remove(c);
  el.classList.add(`fam-${key}`);
}
