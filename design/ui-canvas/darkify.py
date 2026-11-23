"""Generate dark-mode copies of the Branvio UI artboards by mapping each light color to a dark token."""
import pathlib
import re

ROOT = pathlib.Path(__file__).parent / "project"
NAMES = ["Main", "Commit", "Conflict", "System", "Menus", "Dialogs", "Palette", "Coverage"]

# Specific cases applied before the general color map.
PRE = [
    ("a{color:#1F5FD1}a:hover{color:#174AA6}", "a{color:#7AA7F5}a:hover{color:#A9C6F8}"),
    ("body{margin:0;", "body{margin:0;color-scheme:dark;"),
    ("background: #18212B; color: #FFFFFF", "background: #26303C; color: #FFFFFF"),
    # Danger buttons keep white text, so they need a red that still reaches 4.5:1 on dark.
    ("background: #B8322E; border-color: #B8322E; color: #FFFFFF",
     "background: #C23B36; border-color: #C23B36; color: #FFFFFF"),
    ("color: #FFFFFF", "color: @@WHITE@@"),
    ("text, 16:1", "text, 14:1"),
    ("secondary text, 6:1", "secondary text, 7:1"),
    ("one primary button, 5.9:1", "one primary button, 4.7:1"),
    ("rgba(24,33,43,0.12)", "rgba(230,235,240,0.14)"),
]

CMAP = {
    "#F4F6F8": "#0F141A", "#FFFFFF": "#161C24", "#18212B": "#E6EBF0", "#566373": "#9AA7B5",
    "#DCE2E8": "#2A333E", "#EEF1F4": "#1F262F", "#E8EDF2": "#1E2630", "#E3EAF6": "#1E2A3D",
    "#EAF1FC": "#18263A", "#F7F9FB": "#1A212A", "#FAFBFC": "#131920", "#EEF2F6": "#222A34",
    "#1F5FD1": "#2F6FE0", "#174AA6": "#A9C6F8", "#DCEBFB": "#1E3354",
    "#17804A": "#4ADE80", "#E6F4EC": "#12301F", "#B8322E": "#F87171", "#FBEAEA": "#3A1A1A",
    "#E8F6FD": "#0F2A38", "#9FD9F5": "#1F5A75", "#075985": "#7DD3FC",
    "#F4ECFE": "#2A1B3D", "#C9A6F4": "#5B3A85", "#6B21A8": "#D8B4FE",
    "#E7F8F1": "#0F2E24", "#9EE2C6": "#1F6B50", "#065F46": "#6EE7B7", "#3E4A57": "#B6C2CE",
    "#FDF0D5": "#3A2C10", "#8A5300": "#FBBF24", "#F1F4F8": "#1C232C", "#7A8796": "#6B7886",
    "#C5CED8": "#3A4552", "#4A5664": "#5B6878", "#FFF7E6": "#2B2412", "#F1D9A6": "#5A4520",
    "#6B4A12": "#F3D9A4", "#B45309": "#F59E0B", "#FFFBF2": "#211C10", "#E8F1FD": "#16253D",
    "#8A96A3": "#6B7886", "#7C3AED": "#C4A1FF",
    "#E9EDF1": "#0B1015", "#F2F7FE": "#15243A", "#FDF4F4": "#2E1616", "#8E2B27": "#FCA5A5",
    "#FAF6FF": "#221832", "#C9D1DA": "#070A0E", "#0284C7": "#38BDF8",
}
PATTERN = re.compile("|".join(re.escape(k) for k in CMAP), re.IGNORECASE)

for name in NAMES:
    text = (ROOT / f"{name}.dc.html").read_text(encoding="utf-8")
    for old, new in PRE:
        text = text.replace(old, new)
    text = PATTERN.sub(lambda m: CMAP[m.group(0).upper()], text)
    text = text.replace("@@WHITE@@", "#FFFFFF")
    for target in NAMES:
        text = text.replace(f'href="{target}.dc.html"', f'href="{target}-dark.dc.html"')
    text = re.sub(r"<title>(.*?)</title>", r"<title>\1 (dark)</title>", text, count=1)
    (ROOT / f"{name}-dark.dc.html").write_text(text, encoding="utf-8")
    leftover = sorted(set(re.findall(r"#[0-9A-Fa-f]{6}", text)) - set(CMAP.values()) - {"#FFFFFF"})
    print(name, "colors not remapped:", leftover)
