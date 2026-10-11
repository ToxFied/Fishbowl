import math, sys
# 512 grid. Bowl = circle cut by a neck at the top and a flat base.
CX, CY, R = 256, 286, 150
NECK_HALF = 70     # half-width of the opening
BASE_Y = 412
RIM_Y, RIM_HALF = 118, 92
STROKE = 30
ORANGE = "#FF7A1A"

def circle_y_top(dx): return CY - math.sqrt(R*R - dx*dx)
def circle_dx(y): return math.sqrt(R*R - (y-CY)**2)

ny = circle_y_top(NECK_HALF)
bdx = circle_dx(BASE_Y)
L = lambda x, y: f"{x:.1f} {y:.1f}"
bowl = (f"M{L(CX-NECK_HALF+6, RIM_Y)} L{L(CX-NECK_HALF, ny)} "
        f"A{R} {R} 0 0 0 {L(CX-bdx, BASE_Y)} "
        f"L{L(CX+bdx, BASE_Y)} "
        f"A{R} {R} 0 0 0 {L(CX+NECK_HALF, ny)} L{L(CX+NECK_HALF-6, RIM_Y)}")
rim = f"M{L(CX-RIM_HALF, RIM_Y)} H{CX+RIM_HALF:.1f}"

def fish(nx, y, length, h):
    jx = nx - length            # tail joint
    tail = length*0.42
    return (f"M{L(nx, y)} C{L(nx-length*0.30, y-h*1.35)} {L(jx+length*0.22, y-h*1.25)} {L(jx, y)} "
            f"C{L(jx+length*0.22, y+h*1.25)} {L(nx-length*0.30, y+h*1.35)} {L(nx, y)} Z "
            f"M{L(jx+4, y)} L{L(jx-tail, y-h*1.05)} Q{L(jx-tail*0.62, y)} {L(jx-tail, y+h*1.05)} Z")

def mark(detail=True, bg=True, size=512, rows_opacity=0.34, stroke=None, fish_scale=1.0):
    global STROKE
    STROKE = stroke or 30
    water_y = 214
    inner_r = R - STROKE/2 - 14
    rows = [258, 300, 342, 384] if detail else []
    fx, fy, flen, fh = 338 + (fish_scale-1)*40, 300, 118*fish_scale, 34*fish_scale
    out = [f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 512 512" width="{size}" height="{size}">',
           '<title>Fishbowl</title>']
    out.append('<defs>'
               f'<clipPath id="water"><circle cx="{CX}" cy="{CY}" r="{inner_r}"/></clipPath>'
               '<mask id="knock"><rect width="512" height="512" fill="#fff"/>'
               f'<path d="{fish(fx, fy, flen, fh)}" fill="#000" stroke="#000" stroke-width="22" stroke-linejoin="round"/></mask>'
               '</defs>')
    if bg: out.append('<rect width="512" height="512" rx="114" fill="#000"/>')
    out.append('<g transform="translate(0 -9)">')
    out.append(f'<g fill="none" stroke="#fff" stroke-width="{STROKE}" stroke-linecap="round" stroke-linejoin="round">'
               f'<path d="{bowl}"/><path d="{rim}"/></g>')
    out.append(f'<g clip-path="url(#water)" mask="url(#knock)" stroke-linecap="round">')
    out.append(f'<line x1="0" x2="512" y1="{water_y}" y2="{water_y}" stroke="#fff" stroke-width="{STROKE*0.7:.0f}"/>')
    for ry in rows:
        out.append(f'<line x1="0" x2="512" y1="{ry}" y2="{ry}" stroke="#fff" stroke-opacity="{rows_opacity}" stroke-width="12"/>')
    out.append('</g>')
    out.append(f'<path d="{fish(fx, fy, flen, fh)}" fill="{ORANGE}"/>')
    out.append(f'<circle cx="{fx-30*fish_scale:.1f}" cy="{fy-7*fish_scale:.1f}" r="{7.5*fish_scale:.1f}" fill="#000"/>')
    out.append('</g>')
    out.append('</svg>')
    return "\n".join(out)

if __name__ == "__main__":
    which = sys.argv[1]
    if which == "favicon": print(mark(detail=False, stroke=42, fish_scale=1.22))
    else: print(mark(detail=(which != "simple"), bg=(which != "mono")))
