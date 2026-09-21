"""Analyze PE section sizes of ruslorer.exe."""
import struct, os, sys

path = sys.argv[1] if len(sys.argv) > 1 else r"D:\Dev\Rusplorer\rusplorer.exe"
total = os.path.getsize(path)
print(f"Total file: {total / 1024:.0f} KB  ({total / 1024 / 1024:.2f} MB)\n")

with open(path, "rb") as f:
    # DOS stub -> PE signature offset
    f.seek(0x3C)
    pe_off = struct.unpack("<I", f.read(4))[0]
    f.seek(pe_off)
    assert f.read(4) == b"PE\0\0", "Not a PE file"

    # COFF header (20 bytes total after PE sig):
    # machine(2)+nsec(2) already read = 4 bytes
    # timestamp(4)+ptr_sym(4)+num_sym(4) = 12 bytes to skip
    # opt_hdr_sz(2) + characteristics(2) = 4 bytes
    _machine, num_sections = struct.unpack("<HH", f.read(4))
    f.read(12)  # skip timestamp, symbol table ptr, num symbols
    opt_hdr_sz = struct.unpack("<H", f.read(2))[0]
    f.read(2)   # skip characteristics

    # skip optional header
    f.seek(pe_off + 24 + opt_hdr_sz)

    LABELS = {
        ".text":   "Compiled code (functions)",
        ".rdata":  "Read-only data (strings, embedded files, vtables)",
        ".data":   "Mutable global data",
        ".pdata":  "Exception unwind tables (Win64)",
        ".tls":    "Thread-local storage",
        ".rsrc":   "Windows resources (icon, manifest)",
        ".reloc":  "Base relocation table",
    }

    sections = []
    for _ in range(num_sections):
        raw = f.read(40)  # section header is exactly 40 bytes
        name = raw[0:8].rstrip(b"\x00").decode(errors="replace")
        vsz  = struct.unpack("<I", raw[8:12])[0]
        rawsz = struct.unpack("<I", raw[16:20])[0]
        sections.append((name, vsz, rawsz))

print(f"  {'Section':<10}  {'On-disk':>9}  {'In-memory':>9}  Note")
print(f"  {'-'*10}  {'-'*9}  {'-'*9}  {'-'*40}")
for name, vsz, rawsz in sections:
    label = LABELS.get(name, "")
    print(f"  {name:<10}  {rawsz/1024:>7.0f} KB  {vsz/1024:>7.0f} KB  {label}")

print()
print("--- Key insights ---")
# fonts + logo are embedded in .rdata or .text via include_bytes!
font_dir = os.path.join(os.path.dirname(path), "source_code", "src", "fonts")
if not os.path.isdir(font_dir):
    font_dir = os.path.join(os.path.dirname(__file__), "..", "src", "fonts")
for fname in ["IosevkaAile-Regular.ttf", "IosevkaAile-Bold.ttf",
              "IosevkaAile-Regular.orig.ttf", "IosevkaAile-Bold.orig.ttf"]:
    fp = os.path.join(font_dir, fname)
    if os.path.exists(fp):
        kb = os.path.getsize(fp) / 1024
        tag = " (subsetted)" if ".orig." not in fname else " (original full)"
        print(f"  Font {fname:<35} {kb:7.1f} KB{tag}")
