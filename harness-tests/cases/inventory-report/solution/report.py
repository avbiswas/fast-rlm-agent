def build_report(products):
    lines = ["# Inventory", ""]
    total = 0
    for product in sorted(products, key=lambda item: item["name"].lower()):
        line_total = product["quantity"] * product["price"]
        total += line_total
        lines.append(
            f'- {product["name"]}: {product["quantity"]} @ '
            f'${product["price"]:.2f} = ${line_total:.2f}'
        )
    lines.extend(["", f"Total: ${total:.2f}"])
    return "\n".join(lines) + "\n"
