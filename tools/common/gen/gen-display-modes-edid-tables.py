#!/usr/bin/env python3
"""Print the tables of src/lib/drivers/display/modes/src/edid_tables.rs from a
Linux tree's drivers/gpu/drm/drm_edid.c (v6.18 was used): the established
timings, Established Timings III, DMT and CTA-861 video codes, interlaced,
doubled-clock and doubled-line modes left out.

Usage: gen-display-modes-edid-tables.py <linux>/drivers/gpu/drm/drm_edid.c
The output is the file's part after its header, each table marked
#[rustfmt::skip]."""
import re, sys
src = open(sys.argv[1]).read()
def table(name):
    start = src.index(name)
    body = src[src.index('{', start)+1:]
    # find end: line starting with "};"
    end = body.index('\n};')
    return body[:end]
mode_re = re.compile(r'DRM_MODE\(\s*"([^"]*)"\s*,\s*([^,]+),\s*(\d+)\s*,\s*(\d+)\s*,\s*(\d+)\s*,\s*(\d+)\s*,\s*(\d+)\s*,\s*(\d+)\s*,\s*(\d+)\s*,\s*(\d+)\s*,\s*(\d+)\s*,\s*(\d+)\s*,\s*(\d+)\s*,\s*([^)]*)\)', re.S)
def modes(name):
    t = table(name)
    out = []
    # split into entries by top-level "{ DRM_MODE" or "[n] = { DRM_MODE"
    for m in mode_re.finditer(t):
        nm, typ, clock, hd, hss, hse, ht, hsk, vd, vss, vse, vt, vscan, flags = m.groups()
        # find comment after
        rest = t[max(0,m.start()-120):m.start()]
        cs = list(re.finditer(r'/\*\s*(\d+)\s*-\s*([^*]*)\*/', rest))
        c = cs[-1] if cs else None
        out.append(dict(name=nm, clock=int(clock), h=(int(hd),int(hss),int(hse),int(ht)), v=(int(vd),int(vss),int(vse),int(vt)), flags=flags, vic=(int(c.group(1)) if c else None), cmt=rest))
    return out
def fl(f):
    ph = 'PHSYNC' in f; pv = 'PVSYNC' in f
    skip = 'INTERLACE' in f or 'DBLSCAN' in f or 'DBLCLK' in f
    return ph, pv, skip
def row(m, extra=''):
    ph,pv,skip = fl(m['flags'])
    h=m['h']; v=m['v']
    return f"    {extra}t({m['clock']}, [{h[0]}, {h[1]}, {h[2]}, {h[3]}], [{v[0]}, {v[1]}, {v[2]}, {v[3]}], {str(ph).lower()}, {str(pv).lower()}),", skip

est = modes('static const struct drm_display_mode edid_est_modes[]')
print(f"/// `edid_est_modes`: bit `i` of the established timings word; `None` for an\n/// interlaced one, which no driver here lists.")
print(f"#[rustfmt::skip]\npub(crate) const ESTABLISHED: [Option<Timing>; {len(est)}] = [")
for m in est:
    r, skip = row(m)
    print("    None," if skip else r.replace("    t(", "    Some(t(").rstrip(',') + "),")
print("];")
e3 = table('static const struct minimode est3_modes[]')
mm = re.findall(r'\{\s*(\d+),\s*(\d+),\s*(\d+),\s*(\d+)\s*\}', e3)
print(f"/// `est3_modes`: width, height, refresh, reduced blanking.")
print(f"#[rustfmt::skip]\npub(crate) const ESTABLISHED3: [(u16, u16, u16, bool); {len(mm)}] = [")
for w,h,r,rb in mm:
    print(f"    ({w}, {h}, {r}, {'true' if rb!='0' else 'false'}),")
print("];")
dmt = modes('static const struct drm_display_mode drm_dmt_modes[]')
keep = [m for m in dmt if not fl(m['flags'])[2]]
print(f"/// `drm_dmt_modes` without the interlaced ones.")
print(f"#[rustfmt::skip]\npub(crate) const DMT: [Timing; {len(keep)}] = [")
for m in keep:
    print(row(m)[0])
print("];")
cea = modes('static const struct drm_display_mode edid_cea_modes_1[]')
keepc = [m for m in cea if m['vic'] and not fl(m['flags'])[2]]
print(f"/// `edid_cea_modes_1` by VIC, without the interlaced and pixel-repeated ones.")
print(f"#[rustfmt::skip]\npub(crate) const CTA: [(u8, Timing); {len(keepc)}] = [")
for m in keepc:
    print(row(m, f"({m['vic']}, ")[0].rstrip(',') + "),")
print("];")
