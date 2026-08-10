#!/usr/bin/env python3
"""Refuses a paragraph or list item over LIMIT characters, or a list over ITEMS.

A wall of text is what makes somebody stop reading a reference page, and
word count per page does not measure it. These two do. Tables, code
fences, headings and image alt text are exempt: none is read as prose.

Two limits because a list fails in two ways. Items that are each a
paragraph is the first, which LIMIT catches. Thirty entries of six words
is the second, which it does not: that is a wall however short each line
is, and ITEMS is what catches it. Deliberately not a ceiling on a list's
total length, because the densest list on this site is the latency
checklist and its density is the reason it is worth reading.

ITEMS sits just above the longest list here, which has ten. It is a
tripwire against drift rather than a demand to split anything today.
"""
import pathlib
import sys

LIMIT = 320
ITEMS = 12


def units(text):
    """Each prose unit in a page: a paragraph, or one item of a list."""
    fence = False
    para, item = [], []
    for line in text.split("\n"):
        stripped = line.strip()
        if stripped.startswith("```"):
            fence = not fence
            continue
        if fence or line.startswith("    "):
            continue
        starts_item = stripped[:2] in {"- ", "* "} or (
            stripped[:1].isdigit() and stripped[1:3] in {". ", ") "}
        )
        if stripped == "":
            if para:
                yield " ".join(para)
                para = []
            if item:
                yield " ".join(item)
                item = []
        elif starts_item:
            if para:
                yield " ".join(para)
                para = []
            if item:
                yield " ".join(item)
            item = [stripped]
        elif item:
            item.append(stripped)
        elif stripped[:1] in {"|", "#", ">"} or stripped.startswith("!["):
            continue
        else:
            para.append(stripped)
    for last in (para, item):
        if last:
            yield " ".join(last)


def list_lengths(text):
    """How many items each list on the page holds."""
    fence, run = False, 0
    for line in text.split("\n"):
        st = line.strip()
        if st.startswith("```"):
            fence = not fence
            continue
        if fence:
            continue
        if st[:2] in {"- ", "* "} or (st[:1].isdigit() and st[1:3] in {". ", ") "}):
            run += 1
        elif st == "" or line.startswith(("  ", "\t")):
            continue
        else:
            if run:
                yield run
            run = 0
    if run:
        yield run


over, long_lists = [], []
for page in sorted(pathlib.Path("site/src").rglob("*.md")):
    text = page.read_text()
    for unit in units(text):
        if len(unit) > LIMIT:
            over.append((len(unit), page, unit))
    for count in list_lengths(text):
        if count > ITEMS:
            long_lists.append((count, page))

for count, page, unit in sorted(over, reverse=True):
    print(f"{page}: {count} characters, ceiling is {LIMIT}")
    print(f"  {unit[:100]}...")
for count, page in sorted(long_lists, reverse=True):
    print(f"{page}: a list of {count} items, ceiling is {ITEMS}")
if over or long_lists:
    print("\nSplit them, or cut them. The numbers are a proxy for whether somebody")
    print("reads the page, and shorter is the only fix either way.")
    sys.exit(1)
print(f"every paragraph and list item is inside {LIMIT} characters, every list inside {ITEMS} items")
