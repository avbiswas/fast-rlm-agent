import importlib.util
import pathlib
import sys


workspace = pathlib.Path(sys.argv[1])
spec = importlib.util.spec_from_file_location("pricing", workspace / "pricing.py")
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)

checks = [
    (module.discounted_total([19.99, 5.01], 20), 20.0),
    (module.discounted_total([], 50), 0.0),
    (module.discounted_total([1.0], 100), 0.0),
]
for actual, expected in checks:
    assert actual == expected, (actual, expected)
for invalid in (-0.01, 100.01):
    try:
        module.discounted_total([10.0], invalid)
    except ValueError:
        pass
    else:
        raise AssertionError(f"expected ValueError for {invalid}")

print("PASS fix-discount")
