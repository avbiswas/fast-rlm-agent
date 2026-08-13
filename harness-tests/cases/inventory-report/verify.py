import importlib.util
import json
import pathlib
import subprocess
import sys
import tempfile


workspace = pathlib.Path(sys.argv[1])
spec = importlib.util.spec_from_file_location("report", workspace / "report.py")
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)

products = [
    {"name": "Zipper", "quantity": 2, "price": 1.25},
    {"name": "bolt", "quantity": 4, "price": 0.5},
]
expected = "# Inventory\n\n- bolt: 4 @ $0.50 = $2.00\n- Zipper: 2 @ $1.25 = $2.50\n\nTotal: $4.50\n"
assert module.build_report(products) == expected

with tempfile.TemporaryDirectory() as tmp:
    source = pathlib.Path(tmp) / "products.json"
    output = pathlib.Path(tmp) / "report.md"
    source.write_text(json.dumps(products), encoding="utf-8")

    printed = subprocess.run(
        [sys.executable, str(workspace / "inventory.py"), str(source)],
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    assert printed == expected

    subprocess.run(
        [
            sys.executable,
            str(workspace / "inventory.py"),
            str(source),
            "--output",
            str(output),
        ],
        check=True,
    )
    assert output.read_text(encoding="utf-8") == expected

print("PASS inventory-report")
