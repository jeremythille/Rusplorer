"""
Subset Iosevka fonts for Rusplorer.

Keeps only character ranges actually needed:
  - Basic Latin + Latin Extended (covers English/French/German/Spanish/Polish/etc. filenames)
  - Cyrillic (Russian/Ukrainian filenames)
  - Greek (Greek filenames)
  - General Punctuation, Arrows, Technical Symbols, Braille (UI elements)
  - Specific emoji used by the app

Characters used in the UI (confirmed):
  ▲▼▶↑↓  arrows/triangles
  ✕×     close/multiply
  ⭐      star (favorites)
  …—–    ellipsis, dashes
  ⏳⏸    hourglass, pause
  ⣾⣽⣻⢿⡿⣟⣯⣷  braille spinner
  🖼💾📋📁📄📦🎦  emoji (picture/floppy/clipboard/folder/file/package/cinema)
"""

import subprocess, sys, os

FONTS_DIR = os.path.join(os.path.dirname(__file__), "..", "src", "fonts")

# fmt: off
UNICODE_RANGES = ",".join([
    # Basic Latin (U+0020–U+007E) + C0/C1 controls we might need
    "U+0020-007E",
    # Latin-1 Supplement: ×, ©, ¼ ½ ¾, etc.
    "U+00A0-00FF",
    # Latin Extended-A (Polish, Czech, Hungarian, Romanian, …)
    "U+0100-017F",
    # Latin Extended-B
    "U+0180-024F",
    # IPA Extensions
    "U+0250-02AF",
    # Cyrillic (Russian, Ukrainian, Serbian, Bulgarian, …)
    "U+0400-04FF",
    # Cyrillic Supplement
    "U+0500-052F",
    # Greek and Coptic
    "U+0370-03FF",
    # General Punctuation: …,–,—,' " " „ ‹ › • †
    "U+2000-206F",
    # Superscripts and Subscripts
    "U+2070-209F",
    # Currency Symbols: €
    "U+20A0-20CF",
    # Letterlike Symbols
    "U+2100-214F",
    # Number Forms: ¼ ½ ¾
    "U+2150-218F",
    # Arrows: ↑ ↓ ←→ etc.
    "U+2190-21FF",
    # Mathematical Operators
    "U+2200-22FF",
    # Miscellaneous Technical: ⏳ (U+23F3) ⏸ (U+23F8)
    "U+2300-23FF",
    # Box Drawing (tree lines if any)
    "U+2500-257F",
    # Block Elements
    "U+2580-259F",
    # Geometric Shapes: ▲ (U+25B2) ▼ (U+25BC) ▶ (U+25B6)
    "U+25A0-25FF",
    # Miscellaneous Symbols: ⭐ (U+2B50)
    "U+2600-26FF",
    # Dingbats: ✕ (U+2715)
    "U+2700-27BF",
    # Braille Patterns: ⣾⣽⣻⢿⡿⣟⣯⣷
    "U+2800-28FF",
    # Supplemental Arrows-B
    "U+2900-297F",
    # Miscellaneous Mathematical Symbols-B
    "U+2980-29FF",
    # Supplemental Arrows-C / Misc Symbols and Arrows: ⭐ (U+2B50)
    "U+2B00-2BFF",
    # Mathematical Alphanumeric Symbols: 𝍖 (U+1D356)
    "U+1D300-1D35F",
    # Enclosed Alphanumeric Supplement
    "U+1F100-1F1FF",
    # Enclosed Ideographic Supplement
    "U+1F200-1F2FF",
    # Miscellaneous Symbols and Pictographs: 🎦 (U+1F3A6) 🖼 (U+1F5BC)
    "U+1F300-1F5FF",
    # Transport and Map Symbols
    "U+1F680-1F6FF",
    # Emoticons / faces
    "U+1F600-1F64F",
    # Supplemental Symbols and Pictographs: 💾 (U+1F4BE) 📋 (U+1F4CB) 📁 (U+1F4C1) 📄 (U+1F4C4) 📦 (U+1F4E6)
    "U+1F4A0-1F4FF",
    # Regional Indicator Symbols
    "U+1F1E0-1F1FF",
    # Variation Selectors (needed for proper emoji rendering)
    "U+FE00-FE0F",
    "U+E0100-E01EF",
])
# fmt: on

OPTIONS = [
    "--layout-features=*",      # keep all OpenType features (kerning, ligatures)
    "--glyph-names",            # keep glyph names (helps debugging)
    "--notdef-outline",         # keep .notdef (missing-char box)
    "--no-hinting",             # strip hinting — rendering engine handles it
    "--desubroutinize",         # flatten CFF subroutines for better compression
]


def subset_font(src_name: str, dst_name: str) -> None:
    src = os.path.join(FONTS_DIR, src_name)
    dst = os.path.join(FONTS_DIR, dst_name)
    if not os.path.exists(src):
        print(f"  SKIP  {src_name} (not found)")
        return

    before_kb = os.path.getsize(src) / 1024

    cmd = [
        sys.executable, "-m", "fontTools.subset",
        src,
        f"--output-file={dst}",
        f"--unicodes={UNICODE_RANGES}",
        *OPTIONS,
    ]
    result = subprocess.run(cmd, capture_output=True, text=True)
    if result.returncode != 0:
        print(f"  ERROR  {src_name}:\n{result.stderr}")
        return

    after_kb = os.path.getsize(dst) / 1024
    pct = (1 - after_kb / before_kb) * 100
    print(f"  {src_name:35s}  {before_kb:7.1f} KB  →  {after_kb:7.1f} KB  (-{pct:.0f}%)")


if __name__ == "__main__":
    print("Subsetting Iosevka fonts …\n")
    subset_font("IosevkaAile-Regular.orig.ttf", "IosevkaAile-Regular.ttf")
    subset_font("IosevkaAile-Bold.orig.ttf",    "IosevkaAile-Bold.ttf")

    print("\nSubsetting NotoEmoji (emoji used in the UI only) …\n")
    # Emoji actually used: ⌛⏳🖼💾📋📁📂📅📄📦⭐ — subset to just these glyphs.
    # NotoEmoji.orig.ttf must exist (copy from cargo cache or download).
    src = os.path.join(FONTS_DIR, "NotoEmoji-Regular.orig.ttf")
    dst = os.path.join(FONTS_DIR, "NotoEmoji-Regular.ttf")
    if os.path.exists(src):
        before_kb = os.path.getsize(src) / 1024
        emoji_codepoints = (
            "U+231B,"    # ⌛  hourglass done
            "U+23F3,"    # ⏳  hourglass not done
            "U+1F5BC,"   # 🖼  frame with picture (thumbnail view toggle)
            "U+1F4C1,"   # 📁  folder
            "U+1F4C2,"   # 📂  open folder (paste path from clipboard)
            "U+1F4C5,"   # 📅  calendar (show modification date)
            "U+1F4BE,"   # 💾  floppy disk
            "U+1F4CB,"   # 📋  clipboard
            "U+1F4C4,"   # 📄  page
            "U+1F4E6,"   # 📦  package
            "U+2B50,"    # ⭐  star
            "U+FE0F,"    # variation selector-16 (emoji presentation)
            "U+20E3"     # combining enclosing keycap
        )
        cmd = [
            sys.executable, "-m", "fontTools.subset", src,
            f"--output-file={dst}",
            f"--unicodes={emoji_codepoints}",
            "--layout-features=*", "--notdef-outline", "--no-hinting",
        ]
        result = subprocess.run(cmd, capture_output=True, text=True)
        if result.returncode != 0:
            print(f"  ERROR: {result.stderr}")
        else:
            after_kb = os.path.getsize(dst) / 1024
            pct = (1 - after_kb / before_kb) * 100
            print(f"  {'NotoEmoji-Regular.ttf':<35}  {before_kb:7.1f} KB  →  {after_kb:7.1f} KB  (-{pct:.0f}%)")
    else:
        print(f"  SKIP  NotoEmoji-Regular.orig.ttf not found — copy it from:")
        print(f"        %USERPROFILE%\\.cargo\\registry\\src\\...\\epaint-0.27.2\\fonts\\NotoEmoji-Regular.ttf")

    print("\nDone.")
