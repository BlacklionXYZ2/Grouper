#!/usr/bin/env python3
"""Generate docs/design-system/board.html from design-system.md, research/profiles.md and the screenshots.

Usage: python build_board.py [repo_root]
Reproducible: reads the two documents, parses their tables for token values, base64-embeds the jpgs,
and checks that every hex colour in the output also appears in the two documents.
"""
import base64, html, json, re, sys
from pathlib import Path

ROOT = Path(sys.argv[1] if len(sys.argv) > 1 else r"C:\Users\oscar\Documents\GitHub\Grouper")
DS = ROOT / "docs" / "design-system"
DOC = (DS / "design-system.md").read_text(encoding="utf-8")
PROF = (DS / "research" / "profiles.md").read_text(encoding="utf-8")
OUT = DS / "board.html"

HEX = re.compile(r"#[0-9A-Fa-f]{6,8}")
DOC_HEX = {h.lower() for h in HEX.findall(DOC + PROF)}


# ---------------------------------------------------------------- parsing helpers
def esc(s):
    return html.escape(s, quote=True)


def inline(s):
    s = esc(s)
    s = re.sub(r"`([^`]+)`", r"<code>\1</code>", s)
    s = re.sub(r"\*\*([^*]+)\*\*", r"<strong>\1</strong>", s)
    s = re.sub(r"(?<![\w*])\*([^*\n]+)\*(?![\w*])", r"<em>\1</em>", s)
    return s


def sec(prefix):
    lines = DOC.splitlines()
    for i, l in enumerate(lines):
        if l.startswith(prefix):
            break
    else:
        raise SystemExit("section not found: " + prefix)
    out = []
    for l in lines[i + 1:]:
        if re.match(r"^#{1,3} \S", l):
            break
        out.append(l)
    return "\n".join(out)


def tables(text):
    res, cur = [], []
    for l in text.splitlines() + [""]:
        if l.strip().startswith("|"):
            cells = [c.strip() for c in l.strip().strip("|").split("|")]
            if all(re.fullmatch(r":?-{3,}:?", c) for c in cells):
                continue
            cur.append(cells)
        elif cur:
            res.append(cur)
            cur = []
    return res


def hexes(cell):
    return HEX.findall(cell)


def name_of(cell):
    m = re.search(r"--[a-z0-9-]+", cell)
    return m.group(0) if m else cell.strip("` ")


def items(text):
    """numbered or dash list items with continuation lines"""
    res = []
    for l in text.splitlines():
        if re.match(r"^(\d+\.|-) ", l):
            res.append(re.sub(r"^(\d+\.|-) ", "", l))
        elif l.startswith("  ") and res and l.strip():
            res[-1] += " " + l.strip()
    return res


def table_html(rows, cls=""):
    head, body = rows[0], rows[1:]
    h = "".join(f"<th scope='col'>{inline(c) or '&nbsp;'}</th>" for c in head)
    b = ""
    for r in body:
        b += "<tr>" + "".join(
            (f"<th scope='row'>{inline(c)}</th>" if i == 0 else f"<td>{inline(c)}</td>") for i, c in enumerate(r)
        ) + "</tr>"
    return f"<div class='tscroll' tabindex='0'><table class='{cls}'><thead><tr>{h}</tr></thead><tbody>{b}</tbody></table></div>"


# ---------------------------------------------------------------- token data
dark, light = {}, {}

neutral = tables(sec("### 2.1"))[0][1:]
for r in neutral:
    n = name_of(r[0])
    d, l = hexes(r[1]), hexes(r[2])
    if n == "--backdrop":
        continue
    dark[n], light[n] = d[-1], l[-1]
backdrop_row = [r for r in neutral if name_of(r[0]) == "--backdrop"][0]

tiers = tables(sec("### 2.2"))[0][1:]
for r in tiers:
    n = name_of(r[0])
    dark[n], light[n] = hexes(r[1])[-1], hexes(r[2])[-1]

hair = tables(sec("### 2.3"))[0][1:]
for r in hair:
    n = name_of(r[0])
    dark[n], light[n] = hexes(r[1])[-1], hexes(r[2])[-1]

brand_rows = tables(sec("### 2.4"))[0][1:]
grad = {}
for r in brand_rows:
    n = name_of(r[0])
    d, l = hexes(r[1]), hexes(r[2]) or hexes(r[1])
    if len(d) > 1:
        grad[n] = (d, l)
    else:
        dark[n], light[n] = d[0], l[0]

sem_rows = tables(sec("### 2.5"))[0][1:]
for r in sem_rows:
    base = re.findall(r"--[a-z0-9-]+", r[0])
    d, l = hexes(r[1]), hexes(r[2])
    dark[base[0]], dark[base[1]] = d[0], d[1]
    light[base[0]], light[base[1]] = l[0], l[1]
dark["--danger-fill"], light["--danger-fill"] = "#C9362B", "#B42318"  # stated in 2.5 prose
dark["--on-fill"] = light["--on-fill"] = "#FFFFFF"  # "white text" on danger fill / count badges; #FFFFFF appears in 2.1

hue_t = tables(sec("### 2.6"))
HUES = []
hue_dark, hue_light = {}, {}
for rows, store in ((hue_t[1], hue_dark), (hue_t[2], hue_light)):
    for r in rows[1:]:
        nm = r[0].split(" (")[0].strip()
        fam = re.search(r"\((.*?)\)", r[0])
        store[nm] = hexes(" ".join(r[1:5]))
        if store is hue_dark:
            HUES.append((nm, fam.group(1) if fam else ""))
for nm, _ in HUES:
    for i, step in enumerate(("bg", "border", "solid", "text")):
        dark[f"--hue-{nm}-{step}"] = hue_dark[nm][i]
        light[f"--hue-{nm}-{step}"] = hue_light[nm][i]


def grad_css(hs):
    return "linear-gradient(90deg," + ",".join(hs) + ")"


dark["--grad-text"] = grad_css(grad["--brand-gradient-text"][0])
light["--grad-text"] = grad_css(grad["--brand-gradient-text"][1])
dark["--brand-gradient"] = light["--brand-gradient"] = grad_css(grad["--brand-gradient"][0])

dark["--shadow-sm"] = "none"
light["--shadow-sm"] = "0 0 0 1px rgb(15 23 34 / .04), 0 2px 8px rgb(15 23 34 / .04)"
dark["--shadow-float"] = "0 0 0 1px rgb(214 226 245 / .08)"
light["--shadow-float"] = "0 0 0 1px rgb(15 23 34 / .06)"
dark["--shadow-lg"] = "0 0 0 1px rgb(214 226 245 / .10)"
light["--shadow-lg"] = "0 0 0 1px rgb(15 23 34 / .08)"
dark["--keycap-hl"] = light["--keycap-hl"] = "inset 0 1px 0 rgb(255 255 255 / .08), 0 1px 0 rgb(2 6 14 / .5)"
dark["--backdrop-grad"] = grad_css(hexes(backdrop_row[1]))
light["--backdrop-grad"] = grad_css(hexes(backdrop_row[2]))


def block(d):
    return ";".join(f"{k}:{v}" for k, v in d.items()) + ";"


# fixed (non-themed) design tokens from the document
type_rows = tables(sec("### 2.7"))[0][1:]
space_rows = tables(sec("### 2.8"))[0][1:]
size_rows = tables(sec("### 2.8"))[1][1:]
radii_rows = tables(sec("### 2.9"))[0][1:]
elev_rows = tables(sec("### 2.10"))[0][1:]
motion_rows = tables(sec("### 2.11"))[0][1:]

fixed = {}
for r in space_rows:
    fixed[name_of(r[0])] = r[1] + "px"
for r in radii_rows:
    fixed[name_of(r[0])] = r[1] if "px" in r[1] else r[1] + "px"
for r in motion_rows:
    fixed[name_of(r[0])] = r[1].strip("`")
fixed_css = ";".join(f"{k}:{v}" for k, v in fixed.items()) + ";"

# ---------------------------------------------------------------- images
IMG = {}
for p in sorted((DS / "research").glob("*.jpg")):
    IMG[p.stem] = "data:image/jpeg;base64," + base64.b64encode(p.read_bytes()).decode("ascii")

# ---------------------------------------------------------------- html builders
def sw(label, hx, cssval=None, small=False):
    """a labelled swatch: block then name and hex underneath"""
    c = cssval or hx
    return (f"<div class='sw{' sm' if small else ''}'><span class='chipblock' style='background:{c}'></span>"
            f"<span class='swl'>{esc(label)}</span><span class='swh'>{esc(hx)}</span></div>")


def scope(cls, title, inner):
    return (f"<div class='scope {cls}'><h4 class='scope-title'>{title}</h4>{inner}</div>")


def two(fn):
    return f"<div class='duo'>{scope('t-dark', 'Dark theme', fn('dark'))}{scope('t-light', 'Light theme', fn('light'))}</div>"


# ---- reference gallery
REFS = [
    dict(name="Claude", host="claude.ai, claude.com", imgs=[("claude-signin", "Claude sign-in page"), ("claude-product", "Claude product page")],
         sw=[("clay accent", "#d97757"), ("gray-20", "#f9f9f7"), ("gray-800", "#20201f")],
         chips=["radius 8 / 10 / 16-24", "ease-out .165,.84,.44,1", "60ms feedback"],
         sig=["Serif voice headline beside a plain sans UI", "Warm paper neutrals, one clay accent", "8% alpha hairlines"],
         borrow=["A voice reserved for the product speaking", "Control-height and gap tokens; 60ms feedback"],
         avoid=["Warm beige palette", "Copying the serif look"]),
    dict(name="ChatGPT", host="chatgpt.com", imgs=[("chatgpt-home", "ChatGPT logged-out home")],
         sw=[("page", "#fcfcfc"), ("text", "#0d0d0d"), ("secondary", "#5d5d5d")],
         chips=["composer radius 28", "768px column", "44px touch min"],
         sig=["Floating pill composer with round send", "One question on an empty screen", "Plain icon + label rail"],
         borrow=["Composer anatomy and shape", "Ring-plus-blur elevation, 768px column"],
         avoid=["Colourlessness", "Pills for everything"]),
    dict(name="Hermes", host="hermes-agent.nousresearch.com/desktop", imgs=[("hermes-hero", "Hermes Desktop hero"), ("hermes-downloads", "Hermes Desktop downloads")],
         sw=[("background", "rgb(0,0,242)", "rgb(0,0,242)"), ("text", "#f2f2f2"), ("deeper blue", "rgb(0,0,194)", "rgb(0,0,194)")],
         chips=["Oswald 300 display", "uppercase mono labels", "radius 1.8-3"],
         sig=["One colour, total commitment", "Condensed light display type", "Fine-art imagery as brand"],
         borrow=["Uppercase mono eyebrows as instrument labels", "Full brand colour on brand surfaces only"],
         avoid=["Single-hue UI inside the app", "Square corners"]),
    dict(name="Orca", host="orca.dev", imgs=[("orca-hero", "Orca home page"), ("orca-app", "Orca app mock-up")],
         sw=[("background", "#08090a"), ("surface", "#0e0f11"), ("muted text", "#8a8f98"), ("border", "#ffffff14")],
         chips=["radius 4 / 8 / 12 / 16", "8% white hairline", "status-bar meters"],
         sig=["Coordinator chat plus worker panes", "Activity lines in the transcript", "Permission chips on composers"],
         borrow=["Activity lines, permission chips", "Status line with subscription meters"],
         avoid=["IDE density on phone", "Tabs as modes"]),
    dict(name="Apple", host="apple.com/macos", imgs=[("apple-macos", "Apple macOS page")],
         sw=[("ink", "#1d1d1f"), ("surface", "#f5f5f7"), ("secondary", "#6e6e73")],
         chips=["tracking per size", "weights 400 / 600", "radius 28 / 36"],
         sig=["Optical tracking for each size", "Segmented pill for scope", "Translucent chrome over content"],
         borrow=["Per-size tracking, two-weight discipline", "Segmented pill, glass on chrome only"],
         avoid=["Product-render marketing layouts", "28-36px card radii"]),
    dict(name="Linear", host="linear.app", imgs=[("linear-app", "Linear app mock-up")],
         sw=[("tier 1", "#f7f8f8"), ("tier 2", "#d0d6e0"), ("tier 3", "#8a8f98"), ("tier 4", "#62666d")],
         chips=["Inter 510", "0.667px hairline", "named easings"],
         sig=["Four text tiers on near-black", "Agent actions as timeline lines", "Named accents with one job"],
         borrow=["Four-tier text ramp, sub-pixel hairlines", "Agent events as lines; easing set"],
         avoid=["Nothing major (the density reference)"]),
    dict(name="Raycast", host="raycast.com", imgs=[("raycast-hero", "Raycast hero"), ("raycast-hero-art", "Raycast hero artwork")],
         sw=[("base", "#07080a"), ("brand red", "#ff6363"), ("grey", "#9c9c9d")],
         chips=["radius 6 / 11 / 20", "bevelled keys", "white at 2-10% alpha"],
         sig=["Bevelled key buttons", "Translucent layered surfaces", "Floating rounded nav"],
         borrow=["Keycaps with a top-edge highlight", "Alpha-tier text, layered chrome"],
         avoid=["The streak artwork", "Red as brand (ours means danger)"]),
    dict(name="Vercel", host="vercel.com", imgs=[("vercel-hero", "Vercel hero")],
         sw=[("text", "#ededed"), ("blue-700", "#0070f7"), ("green-700", "#28a948"), ("red-700", "#fc0035")],
         chips=["Geist, tight tracking", "radius 6", "100-1000 scales"],
         sig=["Hairline grid", "Ten-step scale per hue", "Colour only in status"],
         borrow=["Ramp-per-hue structure", "Colour only for meaning in data"],
         avoid=["Pure black", "Extreme tracking on body text"]),
]


def ref_card(r):
    imgs = "".join(
        f"<img src='{IMG[k]}' alt='Screenshot of {esc(a)}, research reference' loading='lazy' width='1440' height='900'>"
        for k, a in r["imgs"])
    sws = ref_sw(r['sw'])
    chips = "".join(f"<li>{esc(c)}</li>" for c in r["chips"])
    li = lambda xs: "".join(f"<li>{esc(x)}</li>" for x in xs)
    return f"""<article class='card ref'>
<h3>{esc(r['name'])} <span class='host'>{esc(r['host'])}</span></h3>
<div class='shots n{len(r['imgs'])}'>{imgs}</div>
<div class='swrow'>{sws}</div>
<ul class='chips'>{chips}</ul>
<div class='rcols'>
<div><h4>Signatures</h4><ul>{li(r['sig'])}</ul></div>
<div><h4>Borrow</h4><ul class='plus'>{li(r['borrow'])}</ul></div>
<div><h4>Avoid</h4><ul class='minus'>{li(r['avoid'])}</ul></div>
</div></article>"""


# fix sw tuple shape: ref sw entries are (label, hex[, css])
def ref_sw(entries):
    out = ""
    for e in entries:
        label, hx = e[0], e[1]
        css = e[2] if len(e) > 2 else None
        out += sw(label, hx, css, small=True)
    return out


ref_card2 = ref_card


# ---- tokens
def neutral_panel(theme):
    d = dark if theme == "dark" else light
    names = [name_of(r[0]) for r in neutral if name_of(r[0]) != "--backdrop"]
    roles = {name_of(r[0]): r[3] for r in neutral}
    h = "<div class='swgrid'>" + "".join(
        f"<div class='sw'><span class='chipblock' style='background:var({n})'></span><span class='swl'>{n}</span><span class='swh'>{d[n]}</span><span class='swr'>{esc(roles[n])}</span></div>"
        for n in names)
    bd = hexes(backdrop_row[1 if theme == "dark" else 2])
    h += (f"<div class='sw'><span class='chipblock' style='background:var(--backdrop-grad)'></span><span class='swl'>--backdrop</span>"
          f"<span class='swh'>{bd[0]} to {bd[1]}</span><span class='swr'>{esc(backdrop_row[3])}</span></div></div>")
    return h


def text_panel(theme):
    d = dark if theme == "dark" else light
    used = {name_of(r[0]): r[3] for r in tiers}
    surfaces = ["--surface", "--surface-2", "--surface-select"]
    out = ""
    for s in surfaces:
        rows = ""
        for t in ("--text", "--text-2", "--text-3", "--text-4"):
            rows += (f"<div class='trow'><span style='color:var({t})'>The quiet work of routing a task to the right model</span>"
                     f"<span class='tmeta' style='color:var({t})'>{t} {d[t]}</span></div>")
        out += f"<div class='tsurf' style='background:var({s})'><div class='tlabel' style='color:var(--text-4)'>on {s} {d[s]}</div>{rows}</div>"
    return out


def hair_panel(theme):
    d = dark if theme == "dark" else light
    out = ""
    for r in hair:
        n = name_of(r[0])
        out += (f"<div class='hrow'><span class='hline' style='border-top:1px solid var({n})'></span>"
                f"<span class='swl'>{n}</span><span class='swh'>{d[n]}</span><span class='swr'>{esc(re.sub('`','',r[4]))}</span></div>")
    return out


def brand_panel(theme):
    d = dark if theme == "dark" else light
    out = "<div class='swgrid'>"
    for r in brand_rows:
        n = name_of(r[0])
        if n in grad:
            hs = grad[n][0 if theme == "dark" else 1]
            css = grad_css(hs)
            out += (f"<div class='sw wide'><span class='chipblock' style='background:{css}'></span><span class='swl'>{n}</span>"
                    f"<span class='swh'>{' to '.join(hs)}</span></div>")
        else:
            out += f"<div class='sw'><span class='chipblock' style='background:var({n})'></span><span class='swl'>{n}</span><span class='swh'>{d[n]}</span></div>"
    out += "</div>"
    # action demo
    out += ("<div class='demo-row'><span class='btn primary'>Approve</span><span class='btn outline'>Deny</span>"
            "<span class='gradtext'>What are we working on?</span></div>")
    return out


def sem_panel(theme):
    d = dark if theme == "dark" else light
    out = "<div class='semgrid'>"
    labels = {"--ok": "Done", "--warn": "Waiting", "--danger": "Failed", "--info": "Stale"}
    for r in sem_rows:
        b = re.findall(r"--[a-z0-9-]+", r[0])
        out += (f"<div class='sem'><span class='pill' style='color:var({b[0]});background:var({b[1]})'>"
                f"<span aria-hidden='true'>&#9679;</span> {labels[b[0]]}</span>"
                f"<span class='swl'>{b[0]}</span><span class='swh'>{d[b[0]]}</span>"
                f"<span class='swl'>{b[1]}</span><span class='swh'>{d[b[1]]}</span>"
                f"<span class='swr'>{esc(r[4])}</span></div>")
    out += (f"<div class='sem'><span class='btn danger'>Delete</span><span class='swl'>--danger-fill</span>"
            f"<span class='swh'>{d['--danger-fill']}</span><span class='swr'>white text, unchanged</span></div></div>")
    return out


def hue_panel(theme):
    store = hue_dark if theme == "dark" else hue_light
    out = "<div class='hues'>"
    for nm, fam in HUES:
        cells = ""
        for i, step in enumerate(("1 bg", "2 border", "3 solid", "4 text")):
            hx = store[nm][i]
            cells += (f"<div class='sw sm'><span class='chipblock' style='background:var(--hue-{nm}-{('bg','border','solid','text')[i]})'></span>"
                      f"<span class='swl'>{step}</span><span class='swh'>{hx}</span></div>")
        out += (f"<div class='hue'><div class='hname'><span class='hdot' style='background:var(--hue-{nm}-solid)'></span>"
                f"<strong>{nm}</strong> <span class='host'>{esc(fam)}</span>"
                f"<span class='pill' style='background:var(--hue-{nm}-bg);color:var(--hue-{nm}-text);border:1px solid var(--hue-{nm}-border)'>{nm} pill</span></div>"
                f"<div class='hsw'>{cells}</div></div>")
    return out + "</div>"


def type_html():
    out = "<div class='typelist'>"
    for r in type_rows:
        size, lh = [x.strip() for x in r[2].split("/")]
        weight = re.search(r"\d+", r[3]).group(0)
        tr = re.search(r"[−+-]?\d*\.?\d+", r[4].replace("\u2212", "-")).group(0).replace("\u2212", "-")
        trpx = tr + "px"
        label = f"{size}px / {lh}px, weight {r[3]}, tracking {r[4].replace(chr(8722), '-')}"
        step = r[0]
        mono = ""
        text = "Route the task, keep the name" if step in ("Display", "Title", "Heading") else (
            "Grouper sends the work to the model that fits it, and says which one." if step == "Body" else
            "Sidebar row, button label, input" if step == "UI" else "Chip, pill, meta line" if step == "Caption" else "Status line, keycap")
        extra = ""
        if step == "Micro":
            extra = "<div class='specimen mono-label'>PLAN &middot; API COSTS &middot; TOOL INPUT <span class='tmeta'>uppercase mono label: Micro in --mono, +0.7px</span></div>"
        out += (f"<div class='trowt'><div class='tspec'>"
                f"<div class='specimen' style='font-size:{size}px;line-height:{lh}px;font-weight:{weight};letter-spacing:{trpx}'>{esc(text)}</div>{extra}"
                f"</div><div class='tinfo'><strong>{esc(step)}</strong> <code>{esc(name_of(r[1]))}</code><br>{esc(label)}<br><span class='tmeta'>{esc(r[5])}</span></div></div>")
    out += ("<div class='trowt'><div class='tspec'><div class='specimen' style='font:12.5px/18px var(--mono)'>thread_a91f / config_template.yaml / 62%</div></div>"
            "<div class='tinfo'><strong>Mono</strong> <code>--mono</code><br>12.5px / 18px, weight 400, tracking 0<br><span class='tmeta'>IDs, paths, exact model ids, tool I/O, keycaps, status line</span></div></div>")
    out += ("<div class='trowt'><div class='tspec'><div class='specimen' style='font-size:20px;line-height:26px;font-weight:600;font-variant-numeric:tabular-nums;font-family:var(--font)'>$4.82 &middot; 1,111 &middot; 62%</div></div>"
            "<div class='tinfo'><strong>Tabular figures</strong><br>UI font with <code>font-variant-numeric: tabular-nums</code><br><span class='tmeta'>Money and percentages stay in the UI font</span></div></div>")
    return out + "</div>"


def space_html():
    out = "<div class='spacelist'>"
    for r in space_rows:
        v = int(r[1])
        out += (f"<div class='srow'><code>{name_of(r[0])}</code><span class='bar' style='width:{v*4}px'></span>"
                f"<span class='tmeta'>{v}px &middot; {esc(r[2])}</span></div>")
    out += "</div><h4>Sizes</h4><div class='spacelist'>"
    for r in size_rows:
        m = re.match(r"\d+", r[1])
        bar = f"<span class='bar alt' style='width:{min(int(m.group(0)),60)*4}px'></span>" if m else "<span class='bar alt' style='width:0'></span>"
        out += (f"<div class='srow'><code>{name_of(r[0])}</code>{bar}<span class='tmeta'>{esc(re.sub('`','',r[1]))} &middot; {esc(r[2])}</span></div>")
    return out + "</div>"


def radii_html():
    out = "<div class='radgrid'>"
    for r in radii_rows:
        v = r[1] if "px" in r[1] else r[1] + "px"
        out += (f"<div class='rad'><span class='radbox' style='border-radius:{v if v!='999px' else '999px'}'></span>"
                f"<code>{name_of(r[0])}</code><span class='swh'>{v}</span><span class='swr'>{esc(r[3])}</span></div>")
    return out + "</div>"


def elev_html(theme):
    out = "<div class='elevgrid'>"
    shadows = {"0": "none", "1": "var(--shadow-sm)", "2": "var(--shadow-float)", "3": "var(--shadow-lg)"}
    for r in elev_rows:
        lvl = r[0][0]
        out += (f"<div class='elev' style='box-shadow:{shadows[lvl]};{'border:1px solid var(--line-2);' if lvl=='0' else ''}'>"
                f"<strong>{esc(r[0])}</strong><code>{esc(name_of(r[1]) if r[1] != chr(8212) else 'tone + --line-2')}</code>"
                f"<span class='swr'>{esc(r[4])}</span></div>")
    return out + "</div>"


def bezier_svg(vals, label):
    x1, y1, x2, y2 = [float(v) for v in vals]
    p = f"M4,96 C{4+x1*92:.1f},{96-y1*92:.1f} {4+x2*92:.1f},{96-y2*92:.1f} 96,4"
    return (f"<svg viewBox='0 0 100 100' width='88' height='88' role='img' aria-label='Easing curve {esc(label)}'>"
            f"<rect x='4' y='4' width='92' height='92' fill='none' stroke='var(--line-2)'/>"
            f"<line x1='4' y1='96' x2='96' y2='4' stroke='var(--line-2)' stroke-dasharray='2 3'/>"
            f"<line x1='4' y1='96' x2='{4+x1*92:.1f}' y2='{96-y1*92:.1f}' stroke='var(--text-4)'/>"
            f"<line x1='96' y1='4' x2='{4+x2*92:.1f}' y2='{96-y2*92:.1f}' stroke='var(--text-4)'/>"
            f"<path d='{p}' fill='none' stroke='var(--accent)' stroke-width='3' stroke-linecap='round'/></svg>")


def motion_html():
    out = "<div class='eases'>"
    for r in motion_rows:
        if "cubic-bezier" in r[1]:
            vals = re.search(r"cubic-bezier\(([^)]+)\)", r[1]).group(1).split(",")
            n = name_of(r[0])
            out += (f"<div class='ease'>{bezier_svg(vals, n)}<div><code>{n}</code><br><span class='swh'>{esc(r[1].strip('`'))}</span><br>"
                    f"<span class='tmeta'>{esc(r[2])}</span></div></div>")
    out += "</div><div class='durs' role='radiogroup' aria-label='Demo duration'>"
    for r in motion_rows:
        if r[1].endswith("ms"):
            n = name_of(r[0])
            sel = " aria-checked='true'" if n == "--dur-slow" else " aria-checked='false'"
            out += f"<button type='button' role='radio' class='durbtn'{sel} data-d='{r[1]}'>{n} {r[1]}</button>"
    out += "</div>"
    out += "<div class='modemo' id='modemo' tabindex='0' aria-label='Motion demo: hover, focus or tap to run'><p class='tmeta'>Hover, focus or tap to run. <span id='rm-note'></span></p>"
    for r in motion_rows:
        if "cubic-bezier" in r[1]:
            n = name_of(r[0])
            out += f"<div class='track'><span class='tmeta lbl'>{n}</span><span class='dot' style='transition-timing-function:var({n})'></span></div>"
    out += "</div>"
    return out


# ---- components (sketches)
def fam_mark(h, glyph, size=18):
    return (f"<span class='fmark' style='width:{size}px;height:{size}px;background:var(--hue-{h}-bg);color:var(--hue-{h}-text);border:1px solid var(--hue-{h}-border)' aria-hidden='true'>{glyph}</span>")


def model_chip(h, glyph, name, caret=False):
    return f"<span class='mchip'>{fam_mark(h, glyph)}<span>{esc(name)}</span>{'<span aria-hidden=\"true\" class=\"caret\">&#9662;</span>' if caret else ''}</span>"


ICON_TREE = "<svg width='14' height='14' viewBox='0 0 16 16' aria-hidden='true'><path d='M3 3v10M3 8h5M3 13h7' fill='none' stroke='currentColor' stroke-width='1.5' stroke-linecap='round'/></svg>"


def donut_svg():
    segs = [("teal", 38), ("purple", 24), ("orange", 18), ("pink", 12), ("blue", 8)]
    r, c = 80, 2 * 3.14159265 * 80
    out = "<svg class='donut' viewBox='0 0 200 200' role='img' aria-label='Sample donut: five providers, teal largest at 38 percent'>"
    out += f"<circle cx='100' cy='100' r='{r}' fill='none' stroke='var(--line-3)' stroke-width='22' opacity='.35'/>"
    off = 0
    for h, v in segs:
        ln = c * v / 100 - 2
        out += (f"<circle cx='100' cy='100' r='{r}' fill='none' stroke='var(--hue-{h}-solid)' stroke-width='22' "
                f"stroke-dasharray='{ln:.2f} {c-ln:.2f}' stroke-dashoffset='{-off:.2f}' transform='rotate(-90 100 100)'/>")
        off += c * v / 100
    out += ("<text x='100' y='102' text-anchor='middle' font-size='20' font-weight='600' fill='var(--text)' style='font-variant-numeric:tabular-nums'>$4.82</text>"
            "<text x='100' y='122' text-anchor='middle' font-size='12.5' fill='var(--text-3)'>today, estimated</text></svg>")
    return out


def components_html():
    c = []

    def fig(title, inner, cls=""):
        c.append(f"<figure class='sketch {cls}'><figcaption><strong>{title}</strong> <span class='tag'>sketch, not production</span></figcaption><div class='stage'>{inner}</div></figure>")

    # 1 chat pair
    fig("Chat message pair, model chip and activity lines", f"""
<div class='chat'>
 <div class='msg user'><span class='avatar u' aria-hidden='true'>O</span><div><div class='bubble'>Plan the memory system and have a worker draft the schema.</div><div class='ts'>21:14</div></div></div>
 <div class='msg asst'><span class='avatar a' aria-hidden='true'>G</span><div>
   <div class='who'>Grouper {model_chip('pink','Cl','Claude Sonnet 5.5')}</div>
   <p class='body'>I split this into two threads. The schema draft is running on the server; I will summarise when both report back.</p>
   <div class='act'>{ICON_TREE}<span>Started <a href='#sketches' class='lnk2'>Memory system</a> on N100 Server</span><span class='ts'>21:15</span></div>
   <div class='act'>{ICON_TREE}<span><a href='#sketches' class='lnk2'>Architecture design</a> finished in 2m 14s</span><span class='ts'>21:17</span></div>
   <div class='act'>{ICON_TREE}<span>Approved Web Search</span><span class='ts'>21:18</span></div>
   <span class='think'>Thinking<i></i><i></i><i></i></span>
 </div></div>
</div>""")

    # 2 composer
    fig("Composer", f"""
<div class='composer'>
 <div class='ctop'>{model_chip('teal','Ge','Gemini model',True)}<span class='permchip warn'><span aria-hidden='true'>&#9888;</span> Auto</span><span class='sp'></span>
  <button type='button' class='ghostic' aria-label='Attach file'>&#x1F4CE;</button>
  <button type='button' class='send' aria-label='Send message'>&#8593;</button></div>
 <textarea rows='2' aria-label='Message (sketch)' placeholder='Ask Grouper anything\u2026'></textarea>
</div>""")

    # 3 thread card + plan
    fig("Thread card with live plan checklist", """
<div class='split'>
<div class='tcard'>
 <div class='thead'>""" + ICON_TREE + """<strong>2 running &middot; 1 done</strong></div>
 <div class='trw'><span class='dot10' style='background:var(--hue-orange-solid)'></span><span class='tt'>Memory system</span><span class='plan mono'>1/4</span>""" + model_chip('orange', 'Ll', 'Llama') + """<span class='pill' style='background:var(--hue-orange-bg);color:var(--hue-orange-text)'><span aria-hidden='true'>&#9679;</span> Running</span><span class='chev' aria-hidden='true'>&rsaquo;</span></div>
 <div class='trw'><span class='dot10' style='background:var(--hue-purple-solid)'></span><span class='tt'>Schema draft</span><span class='plan mono'>3/4</span>""" + model_chip('purple', 'Qw', 'Qwen') + """<span class='pill' style='background:var(--warn-soft);color:var(--warn)'><span aria-hidden='true'>&#9888;</span> Waiting</span><span class='chev' aria-hidden='true'>&rsaquo;</span></div>
 <div class='trw'><span class='dot10' style='background:var(--hue-blue-solid)'></span><span class='tt'>Architecture design</span><span class='plan mono'>4/4</span>""" + model_chip('blue', 'Gp', 'GPT') + """<span class='pill' style='background:var(--ok-soft);color:var(--ok)'><span aria-hidden='true'>&#10003;</span> Done</span><span class='chev' aria-hidden='true'>&rsaquo;</span></div>
 <div class='tfoot'>View details</div>
</div>
<div class='plan-box'>
 <div class='mlabel'>PLAN</div><div class='planmeta'>2 of 4 done</div>
 <div class='prog' role='progressbar' aria-valuenow='50' aria-valuemin='0' aria-valuemax='100' aria-label='Plan progress'><span style='width:50%'></span></div>
 <ul class='pl'>
  <li class='done'><span class='tick' aria-hidden='true'>&#10003;</span>Read the existing notes</li>
  <li class='done'><span class='tick' aria-hidden='true'>&#10003;</span>List the tables needed</li>
  <li class='cur'><span class='spin' aria-hidden='true'></span>Gathering requirements\u2026</li>
  <li class='pend'><span class='ring' aria-hidden='true'></span>Write the summary</li>
 </ul>
</div>
</div>""")

    # 4 usage
    keys = [("teal", "provider-a", "$1.83", "38%"), ("purple", "provider-b", "$1.16", "24%"), ("orange", "provider-c", "$0.87", "18%"), ("pink", "provider-d", "$0.58", "12%")]
    klist = "".join(
        f"<div class='krow'><span class='kdisc' style='background:var(--hue-{h}-solid)'></span><span class='kid'>{n}</span><span class='kcost'>{cost}</span><span class='kshare'>{sh}</span><span class='chev' aria-hidden='true'>&rsaquo;</span></div>"
        for h, n, cost, sh in keys)
    fig("Usage: segmented pill, donut, key list and subscription meters", f"""
<div class='usage'>
 <div class='uleft'>
  <div class='seg' role='radiogroup' aria-label='Period'>
   <button type='button' role='radio' aria-checked='false'>Day</button><button type='button' role='radio' aria-checked='true'>Week</button><button type='button' role='radio' aria-checked='false'>Month</button>
  </div>
  <div class='total'>$4.82 <span class='pill' style='background:var(--info-soft);color:var(--info)'>estimated</span></div>
  <div class='cap3'>Sample figures for the sketch</div>
  {donut_svg()}
 </div>
 <div class='uright'>
  <div class='klist'>{klist}</div>
  <div class='meter'><div class='mtop'><span>Claude subscription</span><span class='tab'>62% used</span></div>
   <div class='mbar'><span style='width:62%;background:var(--hue-pink-solid)'></span></div><div class='cap3'>resets in 2h 14m</div></div>
  <div class='meter'><div class='mtop'><span>Gemini subscription</span><span class='tab'>84% used &middot; <span style='color:var(--warn)'>&#9888; Nearly used</span></span></div>
   <div class='mbar'><span style='width:84%;background:var(--warn)'></span></div><div class='cap3'>resets in 3d 4h</div></div>
 </div>
</div>""")

    # 5 palette
    fig("Command palette row with keycaps", """
<div class='palette'>
 <div class='psearch'>Search commands and threads\u2026</div>
 <div class='mlabel pg'>ACTIONS</div>
 <div class='prow sel'><span aria-hidden='true'>&#10133;</span><span class='pl1'>New chat</span><span class='cap3'>Start a conversation</span><span class='keys'><kbd>Ctrl</kbd><kbd>N</kbd></span></div>
 <div class='prow'><span aria-hidden='true'>&#x1F50E;</span><span class='pl1'>Open Usage</span><span class='cap3'>Go to page</span><span class='keys'><kbd>Ctrl</kbd><kbd>U</kbd></span></div>
 <div class='prow'><span aria-hidden='true'>&#9881;</span><span class='pl1'>Open palette</span><span class='cap3'>Anywhere</span><span class='keys'><kbd>Ctrl</kbd><kbd>K</kbd></span></div>
 <div class='pfoot'><span><kbd>&uarr;</kbd><kbd>&darr;</kbd> move</span><span><kbd>&crarr;</kbd> run</span><span><kbd>esc</kbd> close</span></div>
</div>""")

    # 6 status line
    def meter_item(h, glyph, pct, reset, warn=False):
        col = "var(--warn)" if warn else f"var(--hue-{h}-solid)"
        return (f"<a class='sitem' href='#sketches'>{fam_mark(h, glyph, 14)}<span class='smeter'><span style='width:{pct}%;background:{col}'></span></span>"
                f"<span>{pct}%</span><span class='t4'>{reset}</span></a>")
    fig("Desktop status line with meters", f"""
<div class='statusline'><span>3 of 3 devices</span><span class='sp'></span>
 {meter_item('pink','Cl',84,'3d 4h',True)}{meter_item('teal','Ge',62,'2h 14m')}{meter_item('blue','Gp',46,'2h 12m')}</div>""")

    # 7 controls
    fig("Buttons and badges", """
<div class='demo-row'><span class='btn primary'>Approve</span><span class='btn outline'>Deny</span><span class='btn ghost'>View details</span><span class='btn danger'>Stop</span>
 <span class='pill' style='background:var(--ok-soft);color:var(--ok)'>Free</span>
 <span class='pill neutral'>N100 Server</span><span class='count'>3</span></div>""")
    return "\n".join(c)


# ---- reference tables
cmp_rows = tables(sec("### 6.1"))[0]
borrow_rows = tables(sec("### 6.2"))[0]
adopt_rows = tables(sec("## 7."))[0]
adds_rows = tables(DOC.split("### What this system adds", 1)[1].split("## 2.", 1)[0])[0]
decisions = items(sec("## 8."))
sec4 = sec("## 4.")
principles = items(sec4.split("Principles:", 1)[1].split("Anti-patterns:", 1)[0])
anti = items(sec4.split("Anti-patterns:", 1)[1])
mermaid_src = re.search(r"```mermaid\n(.*?)```", DOC, re.S).group(1).strip("\n")
status_line = re.search(r"^Status: (.*?)\n\n", DOC, re.S | re.M).group(1).replace("\n", " ")

cmp_html = table_html(cmp_rows, "cmp").replace("<th scope='row'>**Grouper**", "<th scope='row'>Grouper")
cmp_html = re.sub(r"<tr>(<th scope='row'><strong>Grouper</strong></th>.*?)</tr>", r"<tr class='me'>\1</tr>", cmp_html, flags=re.S)

CSS = r"""
:root,.t-dark{color-scheme:dark;/*@@DARK@@*/}
:root[data-theme=light],.t-light{color-scheme:light;/*@@LIGHT@@*/}
:root{/*@@FIXED@@*/--font:system-ui,-apple-system,"Segoe UI Variable","Segoe UI",Roboto,sans-serif;--mono:ui-monospace,"SF Mono","Cascadia Code",Menlo,Consolas,monospace}
*{box-sizing:border-box}
html{scroll-behavior:smooth;-webkit-text-size-adjust:100%}
body{margin:0;background:var(--bg);color:var(--text);font:400 15px/22px var(--font);letter-spacing:-.1px;overflow-x:hidden}
.t-dark,.t-light{color:var(--text)}
a{color:var(--accent)}
:focus-visible{outline:2px solid var(--focus);outline-offset:1px}
code,.mono{font-family:var(--mono);font-size:12.5px}
code{background:var(--surface-2);border:1px solid var(--line-2);border-radius:var(--r-chip);padding:0 5px;overflow-wrap:anywhere}
h1,h2,h3,h4{margin:0;font-weight:600}
.skip{position:absolute;left:-999px;top:8px;background:var(--surface);padding:8px 12px;border-radius:var(--r-control);z-index:50}
.skip:focus{left:8px}
header.top{position:sticky;top:0;z-index:20;background:var(--glass-solid);border-bottom:1px solid var(--line-2)}
.topin{max-width:1180px;margin:0 auto;padding:8px 16px;display:flex;gap:12px;align-items:center}
.brandmini{font-weight:600;white-space:nowrap}
.brandmini b{background:var(--grad-text);-webkit-background-clip:text;background-clip:text;color:transparent}
nav.sec{display:flex;gap:4px;overflow-x:auto;flex:1;scrollbar-width:thin}
nav.sec a{white-space:nowrap;text-decoration:none;color:var(--text-2);font:500 13.5px/18px var(--font);padding:7px 10px;border-radius:var(--r-control)}
nav.sec a:hover{background:var(--surface-3);color:var(--text)}
#theme{font:500 13.5px/18px var(--font);height:36px;min-width:44px;padding:0 12px;border-radius:var(--r-control);border:1px solid var(--border-control);background:var(--surface);color:var(--text);cursor:pointer}
#theme:hover{background:var(--surface-3)}
main{max-width:1180px;margin:0 auto;padding:0 16px 64px}
.hero{padding:40px 0 24px}
.hero h1{font-size:28px;line-height:34px;letter-spacing:-.4px}
.hero .gradtext{font-size:28px;line-height:34px;font-weight:600;letter-spacing:-.4px;display:block;margin-bottom:8px}
.gradtext{background:var(--grad-text);-webkit-background-clip:text;background-clip:text;color:transparent;font-weight:600}
.hero p{color:var(--text-2);max-width:760px;margin:8px 0 0}
.hero .status{color:var(--text-3);font-size:12.5px;line-height:16px;margin-top:16px;max-width:760px}
.hero ul{display:flex;gap:8px;flex-wrap:wrap;list-style:none;padding:0;margin:16px 0 0}
.hero li{background:var(--surface-select);border-radius:var(--r-control);padding:4px 10px;font:500 12.5px/16px var(--font)}
section.blk{padding-top:48px}
section.blk>h2{font-size:20px;line-height:26px;letter-spacing:-.25px;margin-bottom:4px}
section.blk>.lead{color:var(--text-3);margin:0 0 20px;max-width:780px}
h3.sub{font-size:17px;line-height:22px;letter-spacing:-.2px;margin:32px 0 12px}
h4{font:600 12.5px/16px var(--font);color:var(--text-2);margin:12px 0 6px;letter-spacing:.05px}
.card{background:var(--surface);border:1px solid var(--line-2);border-radius:var(--r-card);padding:16px}
.host{font:400 12.5px/16px var(--mono);color:var(--text-4);margin-left:6px;overflow-wrap:anywhere}
/* gallery */
.refgrid{display:grid;gap:16px;grid-template-columns:1fr}
.ref h3{font-size:17px;line-height:22px;letter-spacing:-.2px;margin-bottom:12px}
.shots{display:grid;gap:8px;grid-template-columns:1fr}
.shots.n2{grid-template-columns:1fr 1fr}
.shots img{width:100%;height:auto;border-radius:var(--r-control);border:1px solid var(--line-2);display:block;background:var(--surface-2)}
.swrow{display:flex;flex-wrap:wrap;gap:12px;margin:14px 0 4px}
ul.chips{list-style:none;padding:0;margin:8px 0;display:flex;gap:6px;flex-wrap:wrap}
ul.chips li{background:var(--surface-2);border:1px solid var(--line-2);color:var(--text-2);font:500 12.5px/16px var(--font);padding:2px 8px;border-radius:var(--r-chip)}
.rcols{display:grid;gap:4px 16px;grid-template-columns:1fr}
.rcols ul{margin:0;padding-left:16px;color:var(--text-2);font-size:13.5px;line-height:18px}
.rcols li{margin:3px 0}
.rcols .plus li::marker{color:var(--ok)}.rcols .minus li::marker{color:var(--danger)}
/* swatches */
.sw{display:flex;flex-direction:column;gap:2px;min-width:0}
.sw .chipblock{display:block;height:36px;border-radius:var(--r-control);border:1px solid var(--line-3)}
.sw.sm .chipblock{height:24px;width:64px}
.sw.sm{width:auto}
.swl{font:500 12.5px/16px var(--font);color:var(--text);overflow-wrap:anywhere}
.swh{font:400 11.5px/14px var(--mono);color:var(--text-3);overflow-wrap:anywhere}
.swr{font:400 11.5px/14px var(--font);color:var(--text-4)}
.swgrid{display:grid;gap:12px;grid-template-columns:repeat(auto-fill,minmax(120px,1fr))}
.swgrid .wide{grid-column:span 2}
.duo{display:grid;gap:16px;grid-template-columns:1fr}
.scope{background:var(--bg);color:var(--text);border:1px solid var(--line-2);border-radius:var(--r-card);padding:16px;min-width:0}
.scope-title{font:600 11.5px/14px var(--mono);letter-spacing:.7px;text-transform:uppercase;color:var(--text-4);margin:0 0 12px}
.scope .card,.scope .hue{background:var(--surface)}
.tsurf{border:1px solid var(--line-2);border-radius:var(--r-control);padding:10px 12px;margin-bottom:10px}
.tlabel{font:400 11.5px/14px var(--mono);margin-bottom:6px}
.trow{display:flex;flex-wrap:wrap;justify-content:space-between;gap:2px 12px;padding:3px 0;font-size:13.5px;line-height:18px}
.tmeta{font:400 11.5px/14px var(--mono);color:var(--text-4)}
.trow .tmeta{opacity:1}
.hrow{display:grid;grid-template-columns:1fr;gap:2px;padding:8px 0}
.hline{display:block;height:0;width:100%;margin-bottom:6px}
.semgrid{display:grid;gap:12px;grid-template-columns:repeat(auto-fill,minmax(150px,1fr))}
.sem{display:flex;flex-direction:column;gap:2px;align-items:flex-start;background:var(--surface);border:1px solid var(--line-2);border-radius:var(--r-card);padding:10px}
.pill{display:inline-flex;align-items:center;gap:5px;height:22px;padding:0 8px;border-radius:var(--r-control);font:500 12.5px/16px var(--font);margin:0 0 6px}
.pill.neutral{background:var(--surface-2);color:var(--text-2)}
.count{display:inline-grid;place-items:center;min-width:20px;height:20px;border-radius:var(--r-full);background:var(--purple-fill);color:var(--on-fill);font:600 11.5px/14px var(--font)}
.demo-row{display:flex;gap:10px;align-items:center;flex-wrap:wrap;margin-top:14px}
.btn{display:inline-flex;align-items:center;justify-content:center;height:36px;padding:0 14px;border-radius:var(--r-control);font:600 13.5px/18px var(--font);cursor:default;white-space:nowrap}
.btn.primary{background:var(--accent-fill);color:var(--accent-ink);border:1px solid var(--accent-fill-edge)}
.btn.outline{border:1px solid var(--border-control);color:var(--text)}
.btn.ghost{color:var(--text-2)}
.btn.danger{background:var(--danger-fill);color:var(--on-fill)}
/* hues */
.hues{display:grid;gap:10px}
.hue{border:1px solid var(--line-2);border-radius:var(--r-control);padding:10px;background:var(--surface)}
.hname{display:flex;align-items:center;gap:8px;flex-wrap:wrap;margin-bottom:8px}
.hname .pill{margin:0 0 0 auto}
.hdot{width:12px;height:12px;border-radius:var(--r-full)}
.hsw{display:grid;grid-template-columns:repeat(4,1fr);gap:8px}
.hsw .sw.sm .chipblock{width:100%;height:28px}
/* type */
.typelist{display:grid;gap:0}
.trowt{display:grid;grid-template-columns:1fr;gap:6px;padding:14px 0;border-top:1px solid var(--line-1)}
.specimen{color:var(--text);overflow-wrap:anywhere}
.mono-label{font:500 11.5px/14px var(--mono);letter-spacing:.7px;text-transform:uppercase;color:var(--text-4);margin-top:6px}
.mono-label .tmeta{text-transform:none;letter-spacing:0;margin-left:8px}
.tinfo{font:400 12.5px/18px var(--font);color:var(--text-3)}
/* spacing */
.spacelist{display:grid;gap:6px}
.srow{display:grid;grid-template-columns:96px minmax(0,1fr);gap:4px 12px;align-items:center}
.srow .bar{display:block;height:12px;max-width:100%;border-radius:2px;background:var(--accent-fill)}
.srow .bar.alt{background:var(--purple-fill)}
.srow .tmeta{grid-column:2}
.radgrid{display:grid;gap:12px;grid-template-columns:repeat(auto-fill,minmax(130px,1fr))}
.rad{display:flex;flex-direction:column;gap:2px}
.radbox{display:block;height:64px;background:var(--surface-2);border:1px solid var(--line-3)}
.elevgrid{display:grid;gap:16px;grid-template-columns:repeat(auto-fill,minmax(150px,1fr))}
.elev{background:var(--surface);border-radius:var(--r-card);padding:14px;display:flex;flex-direction:column;gap:4px;min-height:112px}
/* motion */
.eases{display:grid;gap:12px;grid-template-columns:repeat(auto-fill,minmax(250px,1fr))}
.ease{display:flex;gap:12px;align-items:center;background:var(--surface);border:1px solid var(--line-2);border-radius:var(--r-card);padding:12px}
.durs{display:flex;gap:6px;flex-wrap:wrap;margin:16px 0 8px}
.durbtn{font:500 12.5px/16px var(--font);padding:6px 10px;min-height:32px;border-radius:var(--r-control);border:1px solid var(--border-control);background:var(--surface);color:var(--text-2);cursor:pointer}
.durbtn[aria-checked=true]{background:var(--surface-select);color:var(--text)}
.modemo{--d:300ms;background:var(--surface);border:1px solid var(--line-2);border-radius:var(--r-card);padding:12px 16px;cursor:pointer}
.track{position:relative;height:34px;border-top:1px solid var(--line-1);display:flex;align-items:center}
.track .lbl{position:absolute;left:0;top:2px}
.track .dot{position:absolute;left:0;bottom:6px;width:16px;height:16px;border-radius:var(--r-full);background:var(--accent-fill);transition:transform var(--d),opacity var(--d)}
.modemo:hover .dot,.modemo:focus .dot,.modemo.go .dot{transform:translateX(var(--run,260px))}
@media(prefers-reduced-motion:reduce){
 .track .dot{transition:opacity var(--dur-instant) linear}
 .modemo:hover .dot,.modemo:focus .dot,.modemo.go .dot{transform:none;opacity:.35}
 .think i{animation:none!important}.spin{animation:none!important}
 html{scroll-behavior:auto}
}
/* sketches */
.sketch{margin:0 0 20px}
.sketch figcaption{font:400 13.5px/18px var(--font);color:var(--text-2);margin-bottom:8px;display:flex;gap:8px;flex-wrap:wrap;align-items:center}
.tag{font:500 11.5px/14px var(--mono);letter-spacing:.7px;text-transform:uppercase;color:var(--warn);background:var(--warn-soft);padding:2px 6px;border-radius:var(--r-chip)}
.stage{background:var(--bg);border:1px dashed var(--line-3);border-radius:var(--r-card);padding:16px;overflow-x:auto}
.chat{max-width:720px;margin:0 auto;display:grid;gap:24px}
.msg{display:flex;gap:10px;align-items:flex-start}
.msg>div{min-width:0;flex:1}
.avatar{flex:none;width:32px;height:32px;border-radius:var(--r-full);display:grid;place-items:center;font:600 13.5px/18px var(--font)}
.avatar.u{background:var(--surface-3);color:var(--text-2)}
.avatar.a{background:var(--brand-gradient);color:var(--accent-ink)}
.bubble{background:var(--surface);border:1px solid var(--line-2);border-radius:var(--r-card);padding:12px 16px;display:inline-block;max-width:100%}
.ts{font:400 12.5px/16px var(--font);color:var(--text-4);margin-top:4px}
.who{font:600 13.5px/18px var(--font);display:flex;gap:8px;align-items:center;flex-wrap:wrap;margin-bottom:6px}
.body{margin:0 0 8px}
.act{display:flex;gap:8px;align-items:center;font:400 12.5px/16px var(--font);color:var(--text-3);margin:4px 0;flex-wrap:wrap}
.act .ts{margin:0 0 0 auto}.lnk2{color:var(--text-2);text-underline-offset:2px}
.fmark{display:inline-grid;place-items:center;border-radius:var(--r-chip);font:600 9.5px/1 var(--font);flex:none}
.mchip{display:inline-flex;align-items:center;gap:6px;height:24px;padding:0 8px 0 3px;background:var(--surface-select);border-radius:var(--r-control);font:500 12.5px/16px var(--font);color:var(--text)}
.mchip .caret{color:var(--text-3);font-size:10px}
.think{display:inline-flex;align-items:center;gap:6px;background:var(--surface-2);border-radius:var(--r-control);padding:3px 10px;font:400 12.5px/16px var(--font);color:var(--text-3);margin-top:8px}
.think i{width:4px;height:4px;border-radius:var(--r-full);background:var(--text-3);animation:blink 1.4s infinite}
.think i:nth-of-type(2){animation-delay:.2s}.think i:nth-of-type(3){animation-delay:.4s}
@keyframes blink{0%,80%,100%{opacity:.25}40%{opacity:1}}
.composer{max-width:720px;margin:0 auto;background:var(--surface);border:1px solid var(--line-3);border-radius:var(--r-composer);box-shadow:var(--shadow-float);padding:8px 8px 8px 16px}
.ctop{display:flex;align-items:center;gap:8px;flex-wrap:wrap;padding-left:0}
.sp{flex:1}
.permchip{display:inline-flex;gap:4px;align-items:center;height:24px;padding:0 8px;border:1px solid var(--line-3);border-radius:var(--r-control);font:500 12.5px/16px var(--font)}
.permchip.warn{color:var(--warn)}
.ghostic{width:32px;height:32px;border-radius:var(--r-full);border:0;background:transparent;color:var(--text-3);font-size:15px;cursor:pointer}
.ghostic:hover{background:var(--surface-3)}
.send{width:32px;height:32px;border-radius:var(--r-full);border:0;background:var(--accent-fill);color:var(--accent-ink);font:600 16px/1 var(--font);cursor:pointer}
.composer textarea{width:100%;resize:none;border:0;background:transparent;color:var(--text);font:400 15px/22px var(--font);padding:8px 8px 8px 0;outline-offset:2px;max-height:168px}
.composer textarea::placeholder{color:var(--text-4)}
.split{display:grid;gap:16px;grid-template-columns:1fr}
.tcard{background:var(--surface);border:1px solid var(--line-2);border-radius:var(--r-card);overflow:hidden}
.thead{display:flex;gap:8px;align-items:center;padding:10px 12px;font:600 13.5px/18px var(--font);border-bottom:1px solid var(--line-1)}
.trw{display:flex;gap:8px;align-items:center;min-height:36px;padding:4px 12px;font:500 13.5px/18px var(--font);flex-wrap:wrap}
.trw .pill{margin:0}
.dot10{width:10px;height:10px;border-radius:var(--r-full);flex:none}
.tt{flex:1;min-width:110px}
.plan{font:500 11.5px/14px var(--mono);color:var(--text-3)}
.chev{color:var(--text-4);font-size:18px}
.tfoot{padding:8px 12px;border-top:1px solid var(--line-1);font:500 12.5px/16px var(--font);color:var(--accent)}
.plan-box{background:var(--surface);border:1px solid var(--line-2);border-radius:var(--r-card);padding:12px 16px}
.mlabel{font:500 11.5px/14px var(--mono);letter-spacing:.7px;text-transform:uppercase;color:var(--text-4)}
.planmeta{font:400 12.5px/16px var(--font);color:var(--text-3);margin:2px 0 8px}
.prog{height:4px;border-radius:var(--r-full);background:var(--surface-3);overflow:hidden}
.prog span{display:block;height:100%;background:var(--purple-fill);border-radius:var(--r-full)}
ul.pl{list-style:none;margin:12px 0 0;padding:0;display:grid;gap:8px;font:500 13.5px/18px var(--font)}
ul.pl li{display:flex;gap:8px;align-items:center}
li.done{color:var(--text-3)}li.cur{color:var(--text)}li.pend{color:var(--text-2)}
.tick{color:var(--ok);width:16px;text-align:center}
.ring{width:14px;height:14px;border-radius:var(--r-full);border:2px solid var(--line-3);margin:0 1px}
.spin{width:14px;height:14px;border-radius:var(--r-full);border:2px solid var(--purple-soft);border-top-color:var(--purple);margin:0 1px;animation:rot .9s linear infinite}
@keyframes rot{to{transform:rotate(360deg)}}
.usage{display:grid;gap:20px;grid-template-columns:1fr;max-width:820px;margin:0 auto}
.seg{display:inline-flex;background:var(--surface-2);border:1px solid var(--line-2);border-radius:10px;padding:3px;gap:2px}
.seg button{border:0;background:transparent;color:var(--text-2);font:500 13.5px/18px var(--font);min-height:30px;padding:0 14px;border-radius:7px;cursor:pointer}
.seg button[aria-checked=true]{background:var(--surface);color:var(--text);box-shadow:var(--shadow-sm)}
.total{font:600 28px/34px var(--font);letter-spacing:-.4px;font-variant-numeric:tabular-nums;margin:14px 0 2px;display:flex;gap:10px;align-items:center;flex-wrap:wrap}
.total .pill{margin:0}
.cap3{font:400 12.5px/16px var(--font);color:var(--text-3)}
.donut{width:180px;height:180px;display:block;margin:12px 0}
.klist{background:var(--surface);border:1px solid var(--line-2);border-radius:var(--r-card);margin-bottom:16px}
.krow{display:flex;align-items:center;gap:10px;min-height:44px;padding:0 12px;border-top:1px solid var(--line-1)}
.krow:first-child{border-top:0}
.kdisc{width:12px;height:12px;border-radius:var(--r-full);flex:none}
.kid{flex:1;font:500 13.5px/18px var(--font)}
.kcost{font:600 13.5px/18px var(--font);font-variant-numeric:tabular-nums}
.kshare{font:400 12.5px/16px var(--font);color:var(--text-3);width:34px;text-align:right}
.meter{background:var(--surface);border:1px solid var(--line-2);border-radius:var(--r-card);padding:12px;margin-bottom:12px}
.mtop{display:flex;justify-content:space-between;gap:8px;flex-wrap:wrap;font:500 12.5px/16px var(--font);margin-bottom:8px}
.tab{font-variant-numeric:tabular-nums}
.mbar{height:6px;border-radius:var(--r-full);background:var(--surface-3);margin-bottom:6px;overflow:hidden}
.mbar span{display:block;height:100%;border-radius:var(--r-full)}
.palette{max-width:640px;margin:0 auto;background:var(--surface);border:1px solid var(--line-3);border-radius:var(--r-sheet);box-shadow:var(--shadow-lg);overflow:hidden}
.psearch{height:48px;display:flex;align-items:center;padding:0 16px;color:var(--text-4);border-bottom:1px solid var(--line-1)}
.pg{padding:10px 16px 4px}
.prow{display:flex;align-items:center;gap:10px;min-height:36px;padding:0 16px;font:500 13.5px/18px var(--font);flex-wrap:wrap}
.prow.sel{background:var(--surface-select)}
.prow .pl1{color:var(--text)}
.keys{margin-left:auto;display:flex;gap:4px}
kbd{display:inline-grid;place-items:center;min-width:20px;height:20px;padding:0 5px;border-radius:var(--r-chip);background:var(--surface-2);border:1px solid var(--line-3);box-shadow:var(--keycap-hl);font:500 11.5px/14px var(--mono);color:var(--text-2)}
.pfoot{display:flex;gap:16px;flex-wrap:wrap;padding:8px 16px;margin-top:6px;border-top:1px solid var(--line-1);font:500 11.5px/14px var(--font);color:var(--text-3)}
.pfoot kbd{margin-right:2px}
.statusline{display:flex;align-items:center;gap:16px;min-height:26px;padding:4px 12px;background:var(--glass-solid);border-top:1px solid var(--line-1);font:500 11.5px/14px var(--mono);color:var(--text-3);flex-wrap:wrap;border-radius:var(--r-chip)}
.sitem{display:inline-flex;align-items:center;gap:6px;color:var(--text-3);text-decoration:none}
.smeter{display:inline-block;width:32px;height:4px;border-radius:var(--r-full);background:var(--surface-3);overflow:hidden}
.smeter span{display:block;height:100%}
.t4{color:var(--text-4)}
/* tables */
.tscroll{overflow-x:auto;border:1px solid var(--line-2);border-radius:var(--r-card);background:var(--surface)}
table{border-collapse:collapse;width:100%;min-width:640px;font-size:13.5px;line-height:18px}
th,td{text-align:left;vertical-align:top;padding:10px 12px;border-top:1px solid var(--line-1)}
thead th{border-top:0;font:600 12.5px/16px var(--font);color:var(--text-2);background:var(--surface-2);white-space:nowrap}
tbody th{font-weight:600;white-space:nowrap}
td{color:var(--text-2)}
tr.me th,tr.me td{background:var(--surface-select);color:var(--text)}
table.adopt{min-width:760px}
.pn,.an{display:grid;gap:8px;margin:0;padding:0;list-style:none;counter-reset:n}
.pn li{counter-increment:n;background:var(--surface);border:1px solid var(--line-2);border-radius:var(--r-control);padding:10px 12px 10px 40px;position:relative;color:var(--text-2)}
.pn li::before{content:counter(n);position:absolute;left:12px;top:10px;font:600 11.5px/22px var(--mono);color:var(--purple)}
.an li{background:var(--surface);border:1px solid var(--line-2);border-radius:var(--r-control);padding:8px 12px;color:var(--text-2)}
.an li::before{content:"\00d7 ";color:var(--danger);font-weight:600}
.cols2{display:grid;gap:16px;grid-template-columns:1fr}
ol.dec{margin:0;padding:0;list-style:none;display:grid;gap:12px;counter-reset:d}
ol.dec li{counter-increment:d;background:var(--surface);border:1px solid var(--line-2);border-radius:var(--r-card);padding:14px 16px 14px 48px;position:relative;color:var(--text-2)}
ol.dec li::before{content:counter(d);position:absolute;left:14px;top:14px;width:24px;height:24px;display:grid;place-items:center;border-radius:var(--r-full);background:var(--purple-soft);color:var(--purple);font:600 12.5px/16px var(--font)}
.diagram{background:var(--surface);border:1px solid var(--line-2);border-radius:var(--r-card);padding:16px;overflow-x:auto}
.diagram svg{max-width:none;height:auto}
details.src{margin-top:12px}
details.src summary{cursor:pointer;color:var(--text-2);font:500 13.5px/18px var(--font);min-height:32px}
pre.msrc{background:var(--surface-2);border:1px solid var(--line-2);border-radius:var(--r-control);padding:12px;overflow:auto;font:400 12px/18px var(--mono);color:var(--text-2);margin:8px 0 0}
footer{max-width:1180px;margin:0 auto;padding:24px 16px 48px;color:var(--text-4);font-size:12.5px;line-height:16px;border-top:1px solid var(--line-1)}
@media(min-width:700px){
 .refgrid{grid-template-columns:1fr 1fr}
 .rcols{grid-template-columns:1fr 1fr 1fr}
 .hrow{grid-template-columns:1fr 100px 90px 1fr;align-items:center}
 .hline{margin:0}
 .trowt{grid-template-columns:minmax(0,1fr) 260px;gap:24px;align-items:center}
 .split{grid-template-columns:1.3fr 1fr}
 .usage{grid-template-columns:auto 1fr}
 .cols2{grid-template-columns:1fr 1fr}
}
@media(min-width:900px){.duo{grid-template-columns:1fr 1fr}.hero h1,.hero .gradtext{font-size:28px}.hero{padding-top:56px}}
@media(max-width:560px){.shots.n2{grid-template-columns:1fr}.swgrid .wide{grid-column:span 1}.hsw .swh{font-size:10.5px}}
@media(forced-colors:active){.chipblock{forced-color-adjust:none}}
"""

JS = r"""
(function(){
  var root=document.documentElement,btn=document.getElementById('theme');
  function setTheme(t,save){root.setAttribute('data-theme',t);btn.textContent=t==='dark'?'Light theme':'Dark theme';btn.setAttribute('aria-pressed',String(t==='light'));
    if(save){try{localStorage.setItem('grouper-board-theme',t)}catch(e){}}
    if(window.__renderMermaid)window.__renderMermaid();}
  var t='dark';try{t=localStorage.getItem('grouper-board-theme')||'dark'}catch(e){}
  setTheme(t==='light'?'light':'dark',false);
  btn.addEventListener('click',function(){setTheme(root.getAttribute('data-theme')==='dark'?'light':'dark',true)});
  document.querySelectorAll('[role=radiogroup]').forEach(function(g){
    g.addEventListener('click',function(e){var b=e.target.closest('[role=radio]');if(!b)return;
      g.querySelectorAll('[role=radio]').forEach(function(x){x.setAttribute('aria-checked',x===b?'true':'false')});
      if(b.dataset.d){document.getElementById('modemo').style.setProperty('--d',b.dataset.d)}});
  });
  var mo=document.getElementById('modemo');
  mo.addEventListener('click',function(){mo.classList.toggle('go')});
  function run(){mo.style.setProperty('--run',Math.max(40,mo.clientWidth-64)+'px')}
  run();window.addEventListener('resize',run);
  var rm=window.matchMedia&&window.matchMedia('(prefers-reduced-motion: reduce)').matches;
  document.getElementById('rm-note').textContent=rm?'Reduced motion is on: the dot only changes opacity, as the system specifies.':'';
  /* mermaid, optional */
  var src=document.getElementById('mm-raw').textContent,box=document.getElementById('mm-out'),det=document.getElementById('mm-det');
  function cv(n){return getComputedStyle(root).getPropertyValue(n).trim()}
  window.__renderMermaid=function(){
    if(!window.mermaid)return;
    try{
      window.mermaid.initialize({startOnLoad:false,securityLevel:'strict',theme:'base',themeVariables:{
        fontFamily:cv('--font'),primaryColor:cv('--surface-2'),primaryTextColor:cv('--text'),primaryBorderColor:cv('--line-3'),
        lineColor:cv('--text-3'),secondaryColor:cv('--surface'),tertiaryColor:cv('--bg'),clusterBkg:cv('--surface'),clusterBorder:cv('--line-3'),
        edgeLabelBackground:cv('--surface'),textColor:cv('--text'),mainBkg:cv('--surface-2'),nodeBorder:cv('--line-3')}});
      window.mermaid.render('mm-svg-'+Date.now(),src).then(function(r){box.innerHTML=r.svg;box.hidden=false;det.open=false;});
    }catch(e){}
  };
  var s=document.createElement('script');
  s.src='https://cdn.jsdelivr.net/npm/mermaid@11.4.1/dist/mermaid.min.js';
  s.onload=function(){window.__renderMermaid()};
  s.onerror=function(){};
  document.head.appendChild(s);
})();
"""

NAV = [("refs", "References"), ("compare", "Comparison"), ("tokens", "Tokens"), ("sketches", "Components"),
       ("structure", "Structure"), ("borrowed", "Borrowed from"), ("adoption", "Adoption"), ("decisions", "Decisions")]


def build():
    p = []
    ap = p.append
    ap("<!doctype html><html lang='en' data-theme='dark'><head><meta charset='utf-8'>"
       "<meta name='viewport' content='width=device-width,initial-scale=1'>"
       "<meta name='color-scheme' content='dark light'>"
       "<title>Grouper Design Board</title>")
    ap("<style>" + CSS.replace("/*@@DARK@@*/", block(dark)).replace("/*@@LIGHT@@*/", block(light)).replace("/*@@FIXED@@*/", fixed_css) + "</style></head><body>")
    ap("<a class='skip' href='#main'>Skip to content</a>")
    ap("<header class='top'><div class='topin'><span class='brandmini'><b>Grouper</b> design board</span><nav class='sec' aria-label='Sections'>"
       + "".join(f"<a href='#{i}'>{t}</a>" for i, t in NAV) + "</nav><button id='theme' type='button' aria-pressed='false'>Light theme</button></div></header>")
    ap("<main id='main'>")
    # 1 header
    ap(f"""<section class='hero' aria-labelledby='h-title'>
<span class='gradtext'>One conversation, many models.</span>
<h1 id='h-title'>Grouper design system: design board</h1>
<p><strong>Navy, not grey.</strong> Depth by tone, then hairline. Glass on chrome only. Teal is the one action colour, with dark ink on the fill; purple is the second brand accent. Colour means identity or state, never decoration.</p>
<p class='status'><strong>Status:</strong> {inline(status_line)}</p>
<ul aria-label='Contents of this board'><li>8 references</li><li>10 identity-hue ramps</li><li>7-step type scale</li><li>7 component sketches</li><li>10-step adoption plan</li></ul>
</section>""")
    # 2 gallery
    ap("<section class='blk' id='refs' aria-labelledby='h-refs'><h2 id='h-refs'>1. Reference gallery</h2>"
       "<p class='lead'>Eight sites read live on 27 November 2026. Research material only: structure and principles are borrowed, never pixels or brand assets. Swatches are values read from each site.</p>"
       "<div class='refgrid'>" + "".join(ref_card2(r) for r in REFS) + "</div></section>")
    # 3 comparison
    ap("<section class='blk' id='compare' aria-labelledby='h-cmp'><h2 id='h-cmp'>2. Comparison</h2>"
       "<p class='lead'>From the profiles, with Grouper added. Scroll the table sideways on a phone.</p>" + cmp_html + "</section>")
    # 4 tokens
    ap("<section class='blk' id='tokens' aria-labelledby='h-tok'><h2 id='h-tok'>3. Tokens</h2>"
       "<p class='lead'>Every colour below is shown in both themes at once, labelled with its token name and hex. Existing names stay the public names; new tokens sit beside them.</p>")
    ap("<h3 class='sub' id='t-neutral'>Neutral ramp and surfaces</h3>" + two(neutral_panel))
    ap("<h3 class='sub' id='t-text'>Text: four tiers, on three surfaces</h3>" + two(text_panel))
    ap("<h3 class='sub' id='t-lines'>Hairlines</h3><p class='lead' style='margin-top:-4px'>One ink per theme at fixed alphas, quoted here as the resolved colour on <code>--surface</code>. <code>--border-control</code> is the 3:1 boundary for inputs.</p>" + two(hair_panel))
    ap("<h3 class='sub' id='t-brand'>Brand and action</h3>" + two(brand_panel))
    ap("<h3 class='sub' id='t-sem'>Semantic colours</h3><p class='lead' style='margin-top:-4px'>Each is a text value on a soft background. State is always a word and an icon as well.</p>" + two(sem_panel))
    ap("<h3 class='sub' id='t-hues'>Identity hues: ten ramps, four steps each</h3>"
       "<p class='lead' style='margin-top:-4px'>Step 1 tints a pill, step 2 outlines it, step 3 fills dots, bars and donut segments, step 4 is text (today&rsquo;s <code>--fam-*</code> value, unchanged).</p>" + two(hue_panel))
    ap("<h3 class='sub' id='t-type'>Type scale</h3><p class='lead' style='margin-top:-4px'>System font stacks, three weights (400, 500, 600). Specimens are rendered at the stated size, line height, weight and tracking.</p>"
       "<div class='card'>" + type_html() + "</div>")
    ap("<h3 class='sub' id='t-space'>Spacing and sizes</h3><div class='card'>" + space_html() + "</div>")
    ap("<h3 class='sub' id='t-rad'>Radii</h3><div class='card'>" + radii_html() + "</div>")
    ap("<h3 class='sub' id='t-elev'>Elevation</h3><p class='lead' style='margin-top:-4px'>The 1px ring does most of the work. The document adds a soft blur &ldquo;today&rsquo;s value&rdquo; that is not quoted, so only the ring is drawn here. Dark mode has no card shadows.</p>"
       + two(lambda th: elev_html(th)))
    ap("<h3 class='sub' id='t-motion'>Motion</h3><div class='card'>" + motion_html() + "</div>")
    ap("</section>")
    # 5 components
    ap("<section class='blk' id='sketches' aria-labelledby='h-comp'><h2 id='h-comp'>4. Component sketches</h2>"
       "<p class='lead'>Plain HTML and CSS built from the tokens above, in whichever theme the board is showing. Sample data is invented for the sketch. These are sketches, not production components.</p>"
       + components_html() + "</section>")
    # 6 mermaid
    ap("<section class='blk' id='structure' aria-labelledby='h-str'><h2 id='h-str'>5. Structure</h2>"
       "<p class='lead'>Tokens feed components, components compose patterns, patterns make screens. Rendered with Mermaid when online; the source below always works.</p>"
       "<div class='diagram'><div id='mm-out' hidden></div>"
       "<details class='src' id='mm-det' open><summary>Mermaid source</summary><pre class='msrc' id='mm-raw'>" + esc(mermaid_src) + "</pre></details></div></section>")
    # 7 borrowed
    ap("<section class='blk' id='borrowed' aria-labelledby='h-bor'><h2 id='h-bor'>6. What we borrowed from whom</h2>"
       + table_html(borrow_rows) +
       "<h3 class='sub'>What this system adds</h3>" + table_html(adds_rows) +
       "<h3 class='sub'>Principles and anti-patterns</h3><div class='cols2'><div><h4>Principles</h4><ol class='pn'>"
       + "".join(f"<li>{inline(x)}</li>" for x in principles) + "</ol></div><div><h4>Anti-patterns</h4><ul class='an'>"
       + "".join(f"<li>{inline(x)}</li>" for x in anti) + "</ul></div></div></section>")
    # 8 adoption + decisions
    ap("<section class='blk' id='adoption' aria-labelledby='h-ad'><h2 id='h-ad'>7. Adoption plan</h2>"
       "<p class='lead'>Aliases first, values second, components last. Steps 1&ndash;2 are safe to merge at any time; steps 3, 5 and 6 are the ones the owner will notice; step 9 can wait or be dropped item by item.</p>"
       + table_html(adopt_rows, "adopt") + "</section>")
    ap("<section class='blk' id='decisions' aria-labelledby='h-dec'><h2 id='h-dec'>8. Decisions for the owner</h2><ol class='dec'>"
       + "".join(f"<li>{inline(x)}</li>" for x in decisions) + "</ol></section>")
    ap("</main><footer>Generated from <code>docs/design-system/design-system.md</code> and <code>research/profiles.md</code>. "
       "Screenshots are research material only. Nothing here has been seen on a real device yet.</footer>")
    ap("<script>" + JS + "</script></body></html>")
    return "".join(p)


out = build()
OUT.write_text(out, encoding="utf-8")

# ---- validation: every hex in the output (minus data URIs) must appear in the documents
stripped = re.sub(r"data:image/jpeg;base64,[A-Za-z0-9+/=]+", "", out)
used = {h.lower() for h in HEX.findall(stripped)}
bad = sorted(used - DOC_HEX)
print("file:", OUT, "size:", OUT.stat().st_size, "bytes")
print("hex used:", len(used), "not in documents:", bad)
print("hue ramps:", len(HUES), "images:", len(IMG))
