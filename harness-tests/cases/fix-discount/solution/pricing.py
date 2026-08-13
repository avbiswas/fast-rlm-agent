def discounted_total(prices, discount_percent):
    """Return the total after applying a percentage discount."""
    if not 0 <= discount_percent <= 100:
        raise ValueError("discount_percent must be between 0 and 100")
    subtotal = sum(prices)
    return round(subtotal * (1 - discount_percent / 100), 2)
