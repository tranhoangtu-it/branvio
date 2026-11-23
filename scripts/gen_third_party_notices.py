#!/usr/bin/env python3
"""Generate THIRD_PARTY_NOTICES.md and THIRD_PARTY_LICENSES.md from `cargo metadata`.

Usage (from the repository root):
    python3 scripts/gen_third_party_notices.py            # write both files
    python3 scripts/gen_third_party_notices.py --audit    # print license audit, write nothing
    python3 scripts/gen_third_party_notices.py --check    # exit 1 if either file is stale

Outputs:
  THIRD_PARTY_NOTICES.md   inventory (crate / version / license / repository) + license notes.
  THIRD_PARTY_LICENSES.md  the full attribution file that must accompany binary distributions:
                           every license / copyright / NOTICE file shipped in each crate's
                           source package (deduplicated by text), the license files of bundled
                           C libraries (libgit2, FreeType, AWS-LC, ring/BoringSSL, zlib), and
                           canonical license texts for crates that ship no license file.

Scope: crates reachable from the release binary package (`app-ui`) through *normal*
dependency edges (dev- and build-dependencies are excluded). The union over all target
platforms is used, so the list is a superset for any single OS. Procedural-macro crates
run only at compile time and are listed separately (they are not linked into the binary).

License files are read from each package's manifest directory (the extracted crate sources
in ~/.cargo/registry/src); `cargo metadata` downloads them if missing. Output depends only on
Cargo.lock and this script, so `--check` is deterministic across machines.

Why not cargo-about: it only reads license files at a crate's root and does not collect
Apache NOTICE files or the license files of C libraries bundled in nested directories
(libgit2's COPYING, FreeType's FTL), which are the attribution obligations that matter most
here. This script needs nothing beyond Python 3 and cargo.

This is an engineering inventory, not legal advice.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
from collections import defaultdict
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
OUT = ROOT / "THIRD_PARTY_NOTICES.md"
OUT_LICENSES = ROOT / "THIRD_PARTY_LICENSES.md"
CANONICAL_DIR = Path(__file__).resolve().parent / "license-texts"
BINARY_PACKAGE = "app-ui"

# Single place to change the product / copyright holder name.
PROJECT_NAME = "Branvio"
COPYRIGHT_HOLDER = "Branvio Contributors"

# ---------------------------------------------------------------------------
# License file discovery
# ---------------------------------------------------------------------------
LICENSE_NAME = re.compile(r"^(LICEN[CS]E|COPYING|NOTICE|COPYRIGHT|UNLICENSE)", re.I)
LICENSE_DIRS = {"licenses", "license", "licences"}
# Nested directories never compiled into the binary.
NESTED_SKIP = re.compile(
    r"(^|/)(tests?|testdata|examples?|benches|fuzz|docs?|\.github|contrib)(/|$)", re.I
)
SOURCE_EXT = {".rs", ".c", ".h", ".cc", ".cpp", ".py", ".js", ".toml", ".json", ".xml", ".html"}


def _section(start: str, end: str):
    """Extractor: the lines between a `start` heading and an `end` heading (exclusive)."""

    def extract(text: str) -> str:
        m = re.search(rf"^{re.escape(start)}\s*\n\*+\s*\n(.*?)^{re.escape(end)}\s*\n", text, re.S | re.M)
        if not m:
            raise SystemExit(f"license excerpt '{start}' not found; update NATIVE_LIBS")
        return m.group(1)

    return extract


def _freetype_credit(pkg_dir: Path) -> str:
    header = (pkg_dir / "freetype2/include/freetype/freetype.h").read_text(encoding="utf-8")
    m = re.search(r"Copyright \(C\) (\d{4}-\d{4})", header)
    years = m.group(1) if m else "1996"
    return (
        f"Portions of this software are copyright © {years} The FreeType Project "
        "(www.freetype.org). All rights reserved."
    )


# C/C++ code compiled from crate sources. Paths are relative to the crate's manifest dir.
# These crates' license files are rendered in their own section (not deduplicated), and
# their nested directories are not auto-scanned (e.g. libgit2's deps/winhttp COPYING.GPL /
# COPYING.LGPL are already reproduced verbatim inside libgit2/COPYING).
NATIVE_LIBS = {
    "libgit2-sys": {
        "title": "libgit2 (bundled C library, statically linked via `git2` / `libgit2-sys`)",
        "notes": [
            "libgit2 is GPL-2.0 *with the libgit2 linking exception*: linking it into a program",
            "and distributing the result under any license is permitted; modifications to libgit2",
            "itself stay under its license. `COPYING` below is reproduced verbatim and also covers",
            "the bundled third-party code (zlib, PCRE2, wildmatch, SHA1DC, ntlmclient, llhttp,",
            "xdiff, the OpenSSL-licensed parts, and the LGPL-2.1 winhttp headers).",
            "The LGPL-2.1 winhttp import headers (`deps/winhttp`) are only used for MinGW builds;",
            "the MSVC Windows build links the system `winhttp.lib` from the Windows SDK. If a",
            "MinGW build is ever distributed, the LGPL-2.1 obligations for those headers apply",
            "(offer their source and allow relinking).",
        ],
        "files": [("libgit2/COPYING", None)],
    },
    "freetype-sys": {
        "title": "FreeType (bundled C library; compiled from source on Linux when no suitable system FreeType is found, and on Android)",
        "credit": _freetype_credit,
        "notes": [
            "FreeType is dual-licensed (FTL or GPL-2.0); it is used under **The FreeType License**",
            "(FTL), which requires the credit line above in the product documentation. The",
            "bundled BDF and PCF drivers carry the MIT-style notices reproduced below.",
        ],
        "files": [
            ("freetype2/LICENSE.TXT", None),
            ("freetype2/docs/FTL.TXT", None),
            ("freetype2/src/bdf/README", _section("License", "Credits")),
            ("freetype2/src/pcf/README", _section("License", "Credits")),
        ],
    },
    "aws-lc-sys": {
        "title": "AWS-LC (bundled C library via `aws-lc-sys`, used by rustls)",
        "notes": [
            "AWS-LC is Apache-2.0 / ISC with OpenSSL- and SSLeay-licensed portions; its LICENSE",
            "also lists the third-party code compiled into libcrypto (fiat-crypto, s2n-bignum,",
            "Jitter Entropy).",
        ],
        "files": [("LICENSE", None), ("aws-lc/LICENSE", None), ("aws-lc/third_party/fiat/LICENSE", None)],
    },
    "ring": {
        "title": "ring (Rust + BoringSSL-derived C/assembly)",
        "notes": ["ring contains code derived from BoringSSL / OpenSSL; all its license files follow."],
        "files": [
            ("LICENSE", None),
            ("LICENSE-BoringSSL", None),
            ("LICENSE-other-bits", None),
            ("third_party/fiat/LICENSE", None),
            ("src/polyfill/once_cell/LICENSE-MIT", None),
            ("src/polyfill/once_cell/LICENSE-APACHE", None),
        ],
    },
    "libz-sys": {
        "title": "zlib (bundled C library via `libz-sys`, built when no system zlib is used)",
        "notes": [],
        "files": [("src/zlib/LICENSE", None)],
        "feature_files": {"zlib-ng": [("src/zlib-ng/LICENSE.md", None)]},
    },
}

# Preference order when a crate ships no license file and its expression offers a choice.
CANONICAL_PREFERENCE = ["MIT", "Apache-2.0", "ISC", "BSD-3-Clause", "BSD-2-Clause", "Zlib", "CC0-1.0", "MPL-2.0"]

# ---------------------------------------------------------------------------
# Design assets (NOT bundled in the current build; used by design mockups only)
# ---------------------------------------------------------------------------
DESIGN_ASSETS = [
    {
        "name": "Lucide icons",
        "license": "ISC (Feather-derived portions: MIT)",
        "url": "https://lucide.dev",
        "notice": [
            "ISC License",
            "",
            "Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2022 as part of",
            "Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2022.",
            "",
            "(Newer upstream releases state \"Copyright (c) 2026 Lucide Icons and Contributors\";",
            "copy the LICENSE of the exact Lucide version if the icons are ever bundled.)",
        ],
        "texts": [
            ("ISC", None),
            ("MIT", "MIT License (Feather portions)\n\nCopyright (c) 2013-2022 Cole Bemis"),
        ],
    },
    {
        "name": "Atkinson Hyperlegible Next",
        "license": "OFL-1.1",
        "url": "https://github.com/googlefonts/atkinson-hyperlegible-next",
        "notice": [
            "Copyright 2020-2024 The Atkinson Hyperlegible Next Project Authors",
            "(https://github.com/googlefonts/atkinson-hyperlegible-next)",
            "",
            "Reserved Font Name: none declared upstream.",
            "",
            "This Font Software is licensed under the SIL Open Font License, Version 1.1.",
        ],
        "texts": [("OFL-1.1", None)],
    },
    {
        "name": "Atkinson Hyperlegible Mono",
        "license": "OFL-1.1",
        "url": "https://github.com/googlefonts/atkinson-hyperlegible-next-mono",
        "notice": [
            "Copyright 2020-2024 The Atkinson Hyperlegible Mono Project Authors",
            "(https://github.com/googlefonts/atkinson-hyperlegible-next-mono)",
            "",
            "Reserved Font Name: none declared upstream.",
            "",
            "This Font Software is licensed under the SIL Open Font License, Version 1.1.",
        ],
        "texts": [("OFL-1.1", None)],
    },
]

PERMISSIVE = {
    "MIT", "MIT-0", "Apache-2.0", "BSD-2-Clause", "BSD-3-Clause", "ISC", "Zlib",
    "Unicode-3.0", "Unicode-DFS-2016", "0BSD", "BSL-1.0", "CC0-1.0", "Unlicense",
    "BlueOak-1.0.0", "CDLA-Permissive-2.0", "NCSA", "bzip2-1.0.6", "OFL-1.1",
    "LLVM-exception", "Apache-2.0 WITH LLVM-exception",
}
COPYLEFT_PATTERNS = ("GPL", "AGPL", "LGPL", "MPL", "EPL", "CC-BY-SA", "SSPL", "BUSL", "CDDL", "OSL", "EUPL")
ACCEPTED_EXCEPTIONS = {"GPL-2.0 WITH GCC-exception-2.0", "GPL-2.0-only WITH GCC-exception-2.0"}


def cargo_metadata() -> dict:
    out = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--locked"],
        cwd=ROOT, check=True, capture_output=True, text=True,
    ).stdout
    return json.loads(out)


def is_proc_macro(pkg: dict) -> bool:
    kinds = [k for t in pkg["targets"] for k in t["kind"]]
    return "proc-macro" in kinds and "lib" not in kinds


def binary_closure(meta: dict) -> tuple[list[dict], list[dict]]:
    """Return (linked packages, compile-time-only proc-macro packages)."""
    pkgs = {p["id"]: p for p in meta["packages"]}
    nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
    workspace = set(meta["workspace_members"])
    root = next(i for i in workspace if pkgs[i]["name"] == BINARY_PACKAGE)
    linked, macros, stack, seen = set(), set(), [root], set()
    while stack:
        cur = stack.pop()
        if cur in seen:
            continue
        seen.add(cur)
        if is_proc_macro(pkgs[cur]):
            macros.add(cur)
            continue  # its deps run inside rustc, not in our binary
        linked.add(cur)
        for dep in nodes[cur]["deps"]:
            if any(dk["kind"] is None for dk in dep["dep_kinds"]):
                stack.append(dep["pkg"])
    third = lambda ids: sorted(
        (pkgs[i] for i in ids if i not in workspace),
        key=lambda p: (p["name"], p["version"]),
    )
    return third(linked), third(macros)


def license_of(pkg: dict) -> str:
    if pkg.get("license"):
        return re.sub(r"\s*/\s*", " OR ", pkg["license"].strip())
    if pkg.get("license_file"):
        return "LicenseRef-file"
    return "UNKNOWN"


def _spdx_permissive(expr: str) -> bool:
    """Evaluate an SPDX expression: True if it can be satisfied using only PERMISSIVE terms.

    Grammar (precedence WITH > AND > OR, parentheses allowed).
    """
    tokens = re.findall(r"\(|\)|[A-Za-z0-9.+\-]+", expr)
    pos = 0

    def peek() -> str | None:
        return tokens[pos] if pos < len(tokens) else None

    def take() -> str:
        nonlocal pos
        pos += 1
        return tokens[pos - 1]

    def atom() -> bool:
        tok = take()
        if tok == "(":
            val = or_expr()
            take()  # ")"
            return val
        if peek() == "WITH":
            take()
            exc = take()
            return f"{tok} WITH {exc}" in PERMISSIVE or tok in PERMISSIVE or f"{tok} WITH {exc}" in ACCEPTED_EXCEPTIONS
        return tok in PERMISSIVE

    def and_expr() -> bool:
        val = atom()
        while peek() == "AND":
            take()
            val = atom() and val
        return val

    def or_expr() -> bool:
        val = and_expr()
        while peek() == "OR":
            take()
            val = and_expr() or val
        return val

    try:
        return or_expr() and pos == len(tokens)
    except IndexError:
        return False


def classify(expr: str) -> str:
    """'ok', 'ok-exception', or 'review'."""
    if expr in ACCEPTED_EXCEPTIONS or "WITH linking-exception" in expr or "WITH GCC-exception" in expr:
        return "ok-exception"
    if expr in ("UNKNOWN", "LicenseRef-file"):
        return "review"
    return "ok" if _spdx_permissive(expr) else "review"


def md_escape(s: str) -> str:
    return s.replace("|", "\\|")


# ---------------------------------------------------------------------------
# License text collection
# ---------------------------------------------------------------------------
def read_text(path: Path) -> str:
    data = path.read_bytes()
    try:
        text = data.decode("utf-8")
    except UnicodeDecodeError:
        text = data.decode("latin-1")
    return normalize(text)


def normalize(text: str) -> str:
    lines = [ln.rstrip() for ln in text.replace("\r\n", "\n").replace("\r", "\n").split("\n")]
    while lines and not lines[0]:
        lines.pop(0)
    while lines and not lines[-1]:
        lines.pop()
    return "\n".join(lines)


def pkg_dir(pkg: dict) -> Path:
    return Path(pkg["manifest_path"]).parent


def discover_license_files(pkg: dict) -> list[str]:
    """Relative paths of license/copyright/NOTICE files shipped in a crate's package."""
    base = pkg_dir(pkg)
    found: set[str] = set()
    for entry in sorted(os.listdir(base)):
        p = base / entry
        if p.is_file() and LICENSE_NAME.match(entry):
            found.add(entry)
    if pkg.get("license_file"):
        lf = (base / pkg["license_file"]).resolve()
        if lf.is_file():
            found.add(os.path.relpath(lf, base).replace(os.sep, "/"))
    nested = pkg["name"] not in NATIVE_LIBS
    for root, dirs, files in os.walk(base):
        dirs.sort()
        rel_root = os.path.relpath(root, base).replace(os.sep, "/")
        if rel_root == "." or NESTED_SKIP.search(rel_root) or rel_root.startswith("target"):
            continue
        in_license_dir = any(part.lower() in LICENSE_DIRS for part in rel_root.split("/"))
        if not (nested or in_license_dir):
            continue
        for f in sorted(files):
            if Path(f).suffix.lower() in SOURCE_EXT:
                continue
            if in_license_dir or LICENSE_NAME.match(f):
                found.add(f"{rel_root}/{f}")
    return sorted(found)


def guess_label(text: str) -> str:
    t = text[:4000]
    checks = [
        ("Apache License", "Apache-2.0"),
        ("Mozilla Public License", "MPL-2.0"),
        ("GNU LESSER GENERAL PUBLIC LICENSE", "LGPL"),
        ("GNU GENERAL PUBLIC LICENSE", "GPL"),
        ("Boost Software License", "BSL-1.0"),
        ("CC0 1.0", "CC0-1.0"),
        ("This is free and unencumbered software", "Unlicense"),
        ("UNICODE", "Unicode"),
        ("Unicode", "Unicode"),
        ("Permission to use, copy, modify, and/or distribute", "ISC"),
        ("Permission to use, copy, modify, and distribute", "ISC-style"),
        ("Permission is hereby granted, free of charge", "MIT"),
        ("Redistribution and use in source and binary forms", "BSD-style"),
        ("This software is provided 'as-is'", "Zlib"),
        ("provided 'as-is'", "Zlib"),
    ]
    for needle, label in checks:
        if needle in t:
            return label
    return "Other"


def fence(text: str) -> str:
    longest = max((len(m) for m in re.findall(r"`+", text)), default=0)
    return "`" * max(3, longest + 1)


def code_block(text: str) -> list[str]:
    f = fence(text)
    return [f + "text", text, f, ""]


def crate_ref(pkg: dict) -> str:
    return f"`{pkg['name']}` {pkg['version']}"


def resolved_features(meta: dict) -> dict[str, set[str]]:
    return {n["id"]: set(n.get("features", [])) for n in meta["resolve"]["nodes"]}


def canonical_text(spdx: str) -> str:
    path = CANONICAL_DIR / f"{spdx}.txt"
    if not path.exists():
        raise SystemExit(f"missing canonical license text {path.relative_to(ROOT)}")
    return read_text(path)


def choose_canonical(expr: str) -> str:
    tokens = set(re.findall(r"[A-Za-z0-9.+\-]+", expr))
    for spdx in CANONICAL_PREFERENCE:
        if spdx in tokens:
            return spdx
    raise SystemExit(f"no canonical license text for expression {expr!r}; add one to {CANONICAL_DIR}")


def authors_of(pkg: dict) -> str:
    names = [re.sub(r"\s*<[^>]*>", "", a).strip() for a in pkg.get("authors") or []]
    names = [n for n in names if n]
    return ", ".join(names) if names else f"the {pkg['name']} developers"


def render_licenses(meta: dict, linked: list[dict]) -> str:
    features = resolved_features(meta)
    lines = [
        "# Third-Party Licenses",
        "",
        f"{PROJECT_NAME} is licensed under `MIT OR Apache-2.0` (see `LICENSE-MIT` and `LICENSE-APACHE`).",
        "This file reproduces the license texts, copyright notices and NOTICE files of the",
        f"third-party code compiled into the {PROJECT_NAME} binary, as required by those licenses",
        "for binary distribution. The inventory and license notes are in `THIRD_PARTY_NOTICES.md`.",
        "The crate set is the union over all target platforms.",
        "",
        "<!-- Generated by scripts/gen_third_party_notices.py; do not edit by hand. -->",
        "",
        "## Contents",
        "",
        "1. Bundled native (C/C++) libraries",
        "2. Rust crates: license, copyright and NOTICE files (deduplicated)",
        "3. Rust crates that ship no license file (canonical license texts)",
        "4. Design assets (not bundled in the current build)",
        "",
        "## 1. Bundled native (C/C++) libraries",
        "",
    ]
    emitted: set[tuple[str, str]] = set()
    by_name: dict[str, list[dict]] = defaultdict(list)
    for p in linked:
        by_name[p["name"]].append(p)
    for name in sorted(NATIVE_LIBS):
        spec = NATIVE_LIBS[name]
        for pkg in by_name.get(name, []):
            base = pkg_dir(pkg)
            lines += [f"### {spec['title']}", "", f"Crate: {crate_ref(pkg)}.", ""]
            if "credit" in spec:
                lines += [f"> **{spec['credit'](base)}**", ""]
            if spec["notes"]:
                lines += [" ".join(spec["notes"]), ""]
            files = list(spec["files"])
            for feat, extra in spec.get("feature_files", {}).items():
                if feat in features.get(pkg["id"], set()):
                    files += extra
            for rel, extract in files:
                path = base / rel
                if not path.is_file():
                    raise SystemExit(f"{name} {pkg['version']}: expected license file {rel} is missing")
                text = read_text(path)
                if extract is not None:
                    text = normalize(extract(text))
                    heading = f"#### `{rel}` (license section)"
                else:
                    heading = f"#### `{rel}`"
                lines += [heading, ""] + code_block(text)
                emitted.add((pkg["id"], rel))

    # Section 2: every other license file, deduplicated by normalized text.
    # Texts that differ only in whitespace / line wrapping are merged; the variant from the
    # alphabetically first crate is rendered.
    texts: dict[str, list[tuple[dict, str]]] = defaultdict(list)
    variant: dict[str, str] = {}
    no_files: list[dict] = []
    for pkg in linked:
        files = [f for f in discover_license_files(pkg) if (pkg["id"], f) not in emitted]
        if not files and not any(k[0] == pkg["id"] for k in emitted):
            no_files.append(pkg)
            continue
        for rel in files:
            text = read_text(pkg_dir(pkg) / rel)
            key = " ".join(text.split())
            texts[key].append((pkg, rel))
            variant.setdefault(key, text)
    ordered = sorted(
        ((variant[k], users) for k, users in texts.items()),
        key=lambda kv: (guess_label(kv[0]), kv[1][0][0]["name"], kv[1][0][0]["version"], kv[1][0][1]),
    )
    lines += [
        "## 2. Rust crates: license, copyright and NOTICE files",
        "",
        f"{len(ordered)} distinct texts, collected from the source packages of "
        f"{len(linked) - len(no_files)} crates. Each text lists the crates (and the file within",
        "each crate) it was taken from. Apache-2.0 NOTICE files, where a crate ships one, are",
        "reproduced here as well.",
        "",
    ]
    for idx, (text, users) in enumerate(ordered, 1):
        lines += [f"### T{idx:04d} ({guess_label(text)})", "", "Used by: " + ", ".join(
            f"{crate_ref(p)} (`{rel}`)" for p, rel in sorted(users, key=lambda u: (u[0]["name"], u[0]["version"], u[1]))
        ), ""]
        lines += code_block(text)

    # Section 3: canonical texts for crates without license files.
    groups: dict[str, list[dict]] = defaultdict(list)
    for pkg in no_files:
        groups[choose_canonical(license_of(pkg))].append(pkg)
    lines += [
        "## 3. Rust crates that ship no license file",
        "",
        "These crates declare a license in `Cargo.toml` but include no license file in their",
        "source package. The canonical text of the (chosen) license is reproduced below; the",
        "copyright holders are the authors declared in each crate's manifest.",
        "",
    ]
    for spdx in sorted(groups):
        lines += [f"### {spdx}", ""]
        for pkg in groups[spdx]:
            lines.append(
                f"- {crate_ref(pkg)} (declared `{md_escape(license_of(pkg))}`): "
                f"Copyright (c) {authors_of(pkg)}"
            )
        lines.append("")
        lines += code_block(canonical_text(spdx))

    # Section 4: design assets.
    lines += [
        "## 4. Design assets (not bundled in the current build)",
        "",
        "The following assets are used by the design mockups only and are **not** included in",
        f"the current {PROJECT_NAME} binaries or installers. Their notices are recorded here so",
        "they are ready if the assets are ever bundled.",
        "",
    ]
    for asset in DESIGN_ASSETS:
        lines += [f"### {asset['name']} ({asset['license']})", "", f"Source: <{asset['url']}>", ""]
        lines += code_block("\n".join(asset["notice"]))
        for spdx, header in asset["texts"]:
            body = canonical_text(spdx)
            lines += code_block(f"{header}\n\n{body}" if header else body)
    return "\n".join(lines).rstrip("\n") + "\n"


def render(meta: dict, linked: list[dict], macros: list[dict]) -> str:
    groups: dict[str, list[dict]] = defaultdict(list)
    for p in linked:
        groups[license_of(p)].append(p)
    freetype = next((p for p in linked if p["name"] == "freetype-sys"), None)
    lines = [
        "# Third-Party Notices",
        "",
        f"{PROJECT_NAME} is licensed under `MIT OR Apache-2.0` (see `LICENSE-MIT` and",
        "`LICENSE-APACHE`). It incorporates the third-party components listed below, each under",
        "its own license. Where a component is offered under several licenses (`A OR B`), it is",
        "used under the permissive option. **The full license texts, copyright notices and NOTICE",
        "files of every component are reproduced in `THIRD_PARTY_LICENSES.md`**, which ships with",
        "every binary distribution alongside this file.",
        "",
        "<!-- Generated by scripts/gen_third_party_notices.py; do not edit by hand. -->",
        "",
    ]
    if freetype is not None:
        lines += [
            "## FreeType credit",
            "",
            f"{_freetype_credit(pkg_dir(freetype))}",
            "",
        ]
    lines += [
        "## Notable license notes",
        "",
        "- **libgit2** (via `libgit2-sys`, statically linked through `git2`) is licensed under",
        "  GPL-2.0 *with the libgit2 linking exception*, which permits linking into programs",
        "  under any license and distributing the result without the GPL applying to them.",
        "  Modifications to libgit2 itself would remain under its license. (The `libgit2-sys`",
        "  crate's own Rust code is `MIT OR Apache-2.0`; the bundled C library is not.) libgit2's",
        "  `COPYING` (reproduced verbatim in `THIRD_PARTY_LICENSES.md`) also covers its bundled",
        "  zlib, PCRE2, wildmatch, SHA1DC, ntlmclient, llhttp, xdiff and OpenSSL-licensed code.",
        "- **libgit2 winhttp headers (LGPL-2.1)**: libgit2 bundles LGPL-2.1 winhttp import",
        "  headers used only for MinGW builds. The MSVC Windows build links the Windows SDK's",
        "  `winhttp.lib` instead. If a MinGW build is ever distributed, the LGPL-2.1 obligations",
        "  for those headers apply (provide their source and permit relinking).",
        "- **FreeType** (via `freetype-sys`, compiled from source on Linux when no suitable system",
        "  FreeType is found) is used under The FreeType License (FTL), not its GPL alternative;",
        "  the FTL requires the credit line above in the product documentation.",
        "- **AWS-LC / ring** contain OpenSSL-, SSLeay- and BoringSSL-derived code; their license",
        "  files are reproduced in full in `THIRD_PARTY_LICENSES.md`.",
        "- **wayland-protocols-plasma** (Linux only, via gpui) generates Rust bindings from KDE",
        "  protocol XML files whose headers declare `LGPL-2.1-or-later`, although the crate itself",
        "  declares `MIT`. The XML sources are available from the crate's repository; the LGPL",
        "  text (`COPYING.LIB`) is reproduced in `THIRD_PARTY_LICENSES.md`. Pending legal review.",
        "- **MPL-2.0 crates** (listed under `MPL-2.0` below) are used unmodified. MPL-2.0 is a",
        "  file-level copyleft: the binary may be distributed under the project's license, and",
        "  the Source Code Form of those MPL-covered files is available from crates.io",
        "  (`https://crates.io/crates/<name>/<version>`) and the linked repositories. If any",
        "  MPL-covered file is modified, the modified file must be made available under MPL-2.0.",
        "- Crates offered under a choice that includes a copyleft option (e.g.",
        "  `Apache-2.0 OR GPL-2.0-only`, `MIT OR Apache-2.0 OR LGPL-2.1-or-later`) are used under",
        "  their permissive option.",
        "",
        f"## Rust crates linked into the `{BINARY_PACKAGE}` binary ({len(linked)})",
        "",
    ]
    for lic in sorted(groups, key=lambda k: (-len(groups[k]), k)):
        lines += [f"### {lic} ({len(groups[lic])})", "", "| Crate | Version | Repository |", "|---|---|---|"]
        for p in groups[lic]:
            repo = p.get("repository") or p.get("homepage") or f"https://crates.io/crates/{p['name']}"
            lines.append(f"| {md_escape(p['name'])} | {p['version']} | <{repo}> |")
        lines.append("")
    lines += [
        f"## Compile-time only: procedural macros ({len(macros)})",
        "",
        "These run inside the compiler and are not linked into the binary; listed for completeness.",
        "",
        "| Crate | Version | License |",
        "|---|---|---|",
    ]
    for p in macros:
        lines.append(f"| {md_escape(p['name'])} | {p['version']} | {md_escape(license_of(p))} |")
    lines += [
        "",
        "## Design assets (not bundled in the current build)",
        "",
        "Used by the design mockups only; **not** shipped in the current binaries or installers.",
        "Full notices and license texts: `THIRD_PARTY_LICENSES.md`, section 4.",
        "",
        "| Asset | License | Source |",
        "|---|---|---|",
    ]
    for a in DESIGN_ASSETS:
        lines.append(f"| {a['name']} | {a['license']} | <{a['url']}> |")
    lines.append("")
    return "\n".join(lines)


def audit(meta: dict) -> int:
    flagged = 0
    for p in sorted(meta["packages"], key=lambda p: p["name"]):
        if p["source"] is None:
            continue
        lic = license_of(p)
        verdict = classify(lic)
        if verdict != "ok":
            flagged += 1
            print(f"{verdict:13} {p['name']} {p['version']}: {lic}"
                  + (f" (license_file={p['license_file']})" if p.get("license_file") else ""))
    print(f"-- {flagged} package(s) need attention out of "
          f"{sum(1 for p in meta['packages'] if p['source'])} third-party packages in Cargo.lock")
    return flagged


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--audit", action="store_true")
    ap.add_argument("--check", action="store_true")
    args = ap.parse_args()
    meta = cargo_metadata()
    linked, macros = binary_closure(meta)
    if args.audit:
        audit(meta)
        print(f"-- binary closure: {len(linked)} linked crates, {len(macros)} proc-macros")
        return 0
    outputs = {
        OUT: render(meta, linked, macros),
        OUT_LICENSES: render_licenses(meta, linked),
    }
    if args.check:
        stale = [
            path.name for path, content in outputs.items()
            if (path.read_text(encoding="utf-8") if path.exists() else "") != content
        ]
        if stale:
            print(f"{', '.join(stale)} stale; run scripts/gen_third_party_notices.py", file=sys.stderr)
            return 1
        return 0
    for path, content in outputs.items():
        path.write_text(content, encoding="utf-8", newline="\n")
        print(f"wrote {path.relative_to(ROOT)} ({len(content) // 1024} KiB)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
