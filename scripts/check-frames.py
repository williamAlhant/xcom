#!/usr/bin/env python3
"""Check frame geometry in an xcom client debug log.

The client logs one line per received damage frame under XCOM_DEBUG:

    [ui] frame 0x60000c 640x400 rects=[(0, 0, 640, 400)]

A frame whose largest rect covers most of the window is a full-window frame.
That is the bandwidth regression this helper detects (for example an OpenGL
terminal blinking its cursor and reporting the whole window as damaged).

The log is expected to contain only the sample window of interest; callers
trim startup frames before invoking it.

Exit status is 0 on success and 1 on failure, so shell tests can use it
directly. This is shared by the e2e scripts instead of embedding a parser in
each one.
"""

import argparse
import re
import sys

FRAME_RE = re.compile(
    r"\[ui\] frame 0x[0-9a-f]+ (?P<w>[0-9]+)x(?P<h>[0-9]+) "
    r"rects=\[(?P<rects>.*)\]"
)
RECT_RE = re.compile(r"\((?P<x>[0-9]+), (?P<y>[0-9]+), (?P<w>[0-9]+), (?P<h>[0-9]+)\)")


def frames(path):
    """Yield (window_width, window_height, [(x, y, w, h), ...], line)."""
    with open(path, "r", encoding="utf-8", errors="replace") as log:
        for line in log:
            match = FRAME_RE.search(line)
            if not match:
                continue
            rects = [
                (int(r["x"]), int(r["y"]), int(r["w"]), int(r["h"]))
                for r in RECT_RE.finditer(match["rects"])
            ]
            yield int(match["w"]), int(match["h"]), rects, line.rstrip()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("log", help="client log (or an extracted sample tail)")
    parser.add_argument(
        "--max-rect-fraction",
        type=float,
        default=0.5,
        help="largest allowed single rect area / window area (default: 0.5)",
    )
    parser.add_argument(
        "--expect-frames",
        type=int,
        default=1,
        help="require at least one frame; 0 allows a quiet baseline (default: 1)",
    )
    args = parser.parse_args()

    count = 0
    worst = 0.0
    worst_line = ""
    for width, height, rects, line in frames(args.log):
        count += 1
        area = max(1, width * height)
        for _, _, rect_w, rect_h in rects:
            fraction = (rect_w * rect_h) / area
            if fraction > worst:
                worst = fraction
                worst_line = line

    print(f"measured: frames={count} largest_rect={worst * 100:.0f}% of window")

    if count == 0:
        if args.expect_frames == 0:
            print("PASS: quiet baseline (no post-startup frames)")
            return 0
        print("FAIL: no post-startup frames; the app may not be drawing")
        return 1

    if worst > args.max_rect_fraction:
        print(
            "FAIL: a post-startup frame covers "
            f"{worst * 100:.0f}% of the window "
            f"(limit {args.max_rect_fraction * 100:.0f}%)"
        )
        print(f"  {worst_line}")
        return 1

    print("PASS: idle frames stay small (no continuous full-window frames)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
