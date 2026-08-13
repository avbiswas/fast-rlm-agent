# Add an inventory report command

Turn the starter script into a useful inventory report:

1. Create `report.py` with a `build_report(products)` function.
2. Return Markdown with `# Inventory`, one product per line sorted by name
   case-insensitively, and a final total.
3. Format each product as `- NAME: QUANTITY @ $PRICE = $LINE_TOTAL`, with all
   money values at two decimal places.
4. Update `inventory.py` to accept a JSON input path and an optional
   `--output PATH`. Print the report when `--output` is absent; otherwise write
   exactly the same report to that file.

Use only the Python standard library. Run the script against `products.json`
to check your work.
