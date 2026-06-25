#!/usr/bin/env python3
"""트레이 모래시계 아이콘 프레임 생성 (단색 template PNG).

윗모래 높이 = 잔여율. level-00(다 흐름)~level-20(가득) 21단계 +
위험(빨강) danger / danger-dim. `rsvg-convert` 필요.

    python3 design/gen-tray-frames.py
"""
import os
import subprocess

HERE = os.path.dirname(os.path.abspath(__file__))
OUT = os.path.join(HERE, "..", "src-tauri", "icons", "anim")
os.makedirs(OUT, exist_ok=True)

TOP_Y, NECK_Y, BOT_NECK_Y, BOT_Y = 8, 22, 23, 37
TL, TR, NECK = (12, TOP_Y), (32, TOP_Y), (22, NECK_Y)
BN, BL, BR = (22, BOT_NECK_Y), (12, BOT_Y), (32, BOT_Y)


def hourglass(frac, color="#000000", opacity=1.0):
    surf = TOP_Y + (1 - frac) * (NECK_Y - TOP_Y)
    top_h = NECK_Y - surf
    bot_h = (1 - frac) * (BOT_Y - BOT_NECK_Y)
    bot_top = BOT_Y - bot_h
    op = f'opacity="{opacity}"'
    p = ['<svg width="44" height="44" viewBox="0 0 44 44" xmlns="http://www.w3.org/2000/svg">', "<defs>"]
    p.append(f'<clipPath id="t"><polygon points="{TL[0]},{TL[1]} {TR[0]},{TR[1]} {NECK[0]},{NECK[1]}"/></clipPath>')
    p.append(f'<clipPath id="b"><polygon points="{BN[0]},{BN[1]} {BR[0]},{BR[1]} {BL[0]},{BL[1]}"/></clipPath>')
    p.append("</defs>")
    if top_h > 0.3:
        p.append(f'<rect x="11" y="{surf:.2f}" width="22" height="{top_h:.2f}" fill="{color}" clip-path="url(#t)" {op}/>')
    if bot_h > 0.3:
        p.append(f'<rect x="11" y="{bot_top:.2f}" width="22" height="{bot_h:.2f}" fill="{color}" clip-path="url(#b)" {op}/>')
    p.append(f'<path d="M11 7 H33" stroke="{color}" stroke-width="2.4" stroke-linecap="round" {op}/>')
    p.append(f'<path d="M11 37 H33" stroke="{color}" stroke-width="2.4" stroke-linecap="round" {op}/>')
    p.append(f'<polygon points="{TL[0]},{TL[1]} {TR[0]},{TR[1]} {NECK[0]},{NECK[1]}" fill="none" stroke="{color}" stroke-width="2.2" stroke-linejoin="round" {op}/>')
    p.append(f'<polygon points="{BN[0]},{BN[1]} {BR[0]},{BR[1]} {BL[0]},{BL[1]}" fill="none" stroke="{color}" stroke-width="2.2" stroke-linejoin="round" {op}/>')
    p.append("</svg>")
    return "\n".join(p)


def render(svg, out):
    sp = out.replace("@2x.png", ".svg")
    with open(sp, "w") as f:
        f.write(svg)
    subprocess.run(["rsvg-convert", "-w", "44", "-h", "44", sp, "-o", out], check=True)
    os.remove(sp)


def main():
    n = 20
    for i in range(n + 1):
        render(hourglass(i / n), os.path.join(OUT, f"level-{i:02d}@2x.png"))
    render(hourglass(0.16, "#FF3B30"), os.path.join(OUT, "danger@2x.png"))
    render(hourglass(0.16, "#FF3B30", 0.4), os.path.join(OUT, "danger-dim@2x.png"))
    print(f"생성 완료: {len(os.listdir(OUT))} files → {OUT}")


if __name__ == "__main__":
    main()
