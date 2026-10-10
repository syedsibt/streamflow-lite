#!/usr/bin/env python3
"""Generate assets/luts/identity-17.cube — a 17^3 identity 3D LUT.

Identity = output lattice values normalized 0..1, i.e. the LUT maps every
input color to itself. Data order per the .cube spec: red changes fastest,
then green, then blue (17^3 = 4913 data lines).
"""
import os

SIZE = 17
OUT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "identity-17.cube")

lines = [
    'TITLE "Identity 17"',
    "",
    f"LUT_3D_SIZE {SIZE}",
    "",
    "DOMAIN_MIN 0.0 0.0 0.0",
    "DOMAIN_MAX 1.0 1.0 1.0",
    "",
]
n = SIZE - 1
for b in range(SIZE):
    for g in range(SIZE):
        for r in range(SIZE):
            lines.append(f"{r / n:.6f} {g / n:.6f} {b / n:.6f}")

with open(OUT, "w") as f:
    f.write("\n".join(lines) + "\n")

print(f"wrote {OUT} ({SIZE ** 3} data lines)")
