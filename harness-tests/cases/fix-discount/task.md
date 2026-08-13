# Fix percentage discounts

`pricing.py` subtracts the discount percentage as a flat amount instead of
applying it to the subtotal.

Fix `discounted_total` so it:

- applies `discount_percent` as a percentage of the subtotal;
- rejects percentages below 0 or above 100 with `ValueError`;
- preserves its existing two-decimal rounding behavior.

Run the existing tests when you are done.
