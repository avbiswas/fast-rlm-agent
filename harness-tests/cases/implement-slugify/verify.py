import importlib.util
import pathlib
import sys


workspace = pathlib.Path(sys.argv[1])
spec = importlib.util.spec_from_file_location("text_utils", workspace / "text_utils.py")
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)

cases = {
    "  Multiple   spaces  ": "multiple-spaces",
    "Déjà_vu.txt": "deja-vu-txt",
    "99 Luftballons": "99-luftballons",
    "東京": "item",
    "a---b___c": "a-b-c",
}
for value, expected in cases.items():
    actual = module.slugify(value)
    assert actual == expected, (value, actual, expected)

print("PASS implement-slugify")
