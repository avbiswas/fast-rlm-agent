import pathlib
import sys


workspace = pathlib.Path(sys.argv[1])
guide = (workspace / "rust-memory-guide.md").read_text(encoding="utf-8")
lower = guide.lower()

required_phrases = [
    "ownership",
    "references and borrowing",
    "slices",
    "move",
    "borrow",
    "string slice",
    "sources",
]
for phrase in required_phrases:
    assert phrase in lower, f"missing {phrase!r}"

urls = [
    "https://doc.rust-lang.org/book/ch04-01-what-is-ownership.html",
    "https://doc.rust-lang.org/book/ch04-02-references-and-borrowing.html",
    "https://doc.rust-lang.org/book/ch04-03-slices.html",
]
for url in urls:
    assert url in guide, f"missing source URL {url}"

print("PASS research-rust-urls")
