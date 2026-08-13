def discounted_total(prices, discount_percent):
    """Return the total after applying a percentage discount."""
    subtotal = sum(prices)
    return round(subtotal - discount_percent, 2)
