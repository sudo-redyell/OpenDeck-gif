"""Generate animated GIF fixtures for the OpenDeck GIF animation tests.

Run with the project virtual environment:
    .venv/bin/python tools/make_gif_fixtures.py <output_dir>

Produces deterministic, small fixtures used by `src-tauri/src/gif_animation.rs`
unit tests and by the scratch verification crate.
"""

from __future__ import annotations

import sys
from pathlib import Path

from PIL import Image


def base_canvas(colour: tuple[int, int, int]) -> Image.Image:
    return Image.new("P", (64, 64), colour_to_palette_index(colour))


def colour_to_palette_index(colour: tuple[int, int, int]) -> int:
    palette = [(255, 0, 0), (0, 0, 255), (0, 255, 0)]
    return palette.index(colour) if colour in palette else 0


def full_frame_swap(out: Path) -> None:
    """Two full frames: red background, then blue background. Delays 50ms/100ms."""
    palette = [(255, 0, 0), (0, 0, 255)]
    red = Image.new("P", (64, 64), 0)
    blue = Image.new("P", (64, 64), 1)
    red.putpalette([c for colour in palette for c in colour] + [0] * 741)
    blue.putpalette([c for colour in palette for c in colour] + [0] * 741)
    red.save(out, save_all=True, append_images=[blue], duration=[50, 100], loop=0)


def partial_frame_dispose1(out: Path) -> None:
    """Frame 2 carries only a moving rectangle; disposal=1 (do not dispose).

    The decoder must composite the rectangle onto the previous frame.
    """
    palette = [(255, 0, 0), (0, 0, 255)]
    frames = []
    background = Image.new("P", (64, 64), 0)
    moved = background.copy()
    for xy in ((4, 4), (40, 40)):
        patch = Image.new("P", (16, 16), 1)
        moved.paste(patch, xy)
        frames.append(background if not frames else moved)
    frames[0].putpalette([c for colour in palette for c in colour] + [0] * 741)
    frames[1].putpalette([c for colour in palette for c in colour] + [0] * 741)
    # Frame 1: full background; frame 2: only the delta rectangle.
    frames[1] = moved
    frames[0].save(
        out,
        save_all=True,
        append_images=[frames[1]],
        duration=[40, 40],
        disposal=[0, 1],
        loop=0,
    )


def partial_frame_dispose2(out: Path) -> None:
    """Frame 2 moves a rectangle; disposal=2 (restore to background).

    The decoder must clear the previous rectangle area back to the background.
    """
    palette = [(255, 0, 0), (0, 0, 255)]
    background = Image.new("P", (64, 64), 0)
    frames: list[Image.Image] = []
    for xy in ((4, 4), (40, 40)):
        frame = background.copy()
        frame.paste(Image.new("P", (16, 16), 1), xy)
        frames.append(frame)
    for frame in frames:
        frame.putpalette([c for colour in palette for c in colour] + [0] * 741)
    frames[0].save(
        out,
        save_all=True,
        append_images=[frames[1]],
        duration=[40, 40],
        disposal=[0, 2],
        loop=0,
    )


def static_frame(out: Path) -> None:
    """A single-frame GIF; must not be treated as an animation."""
    palette = [(255, 0, 0)]
    red = Image.new("P", (64, 64), 0)
    red.putpalette([c for colour in palette for c in colour] + [0] * 765)
    red.save(out, save_all=False, duration=50)


def tiny_delays(out: Path) -> None:
    """Frames with sub-floor delays (10ms) that must be clamped by the min delay."""
    palette = [(255, 0, 0), (0, 0, 255)]
    red = Image.new("P", (64, 64), 0)
    blue = Image.new("P", (64, 64), 1)
    for frame, index in ((red, 0), (blue, 1)):
        frame.putpalette([c for colour in palette for c in colour] + [0] * 741)
    red.save(out, save_all=True, append_images=[blue], duration=[10, 10], loop=0)


def main() -> None:
    output = Path(sys.argv[1] if len(sys.argv) > 1 else "src-tauri/tests/fixtures")
    output.mkdir(parents=True, exist_ok=True)
    full_frame_swap(output / "swap.gif")
    static_frame(output / "static.gif")
    partial_frame_dispose1(output / "disposal1.gif")
    partial_frame_dispose2(output / "disposal2.gif")
    tiny_delays(output / "tiny_delay.gif")
    print(f"fixtures written to {output}")


if __name__ == "__main__":
    main()
