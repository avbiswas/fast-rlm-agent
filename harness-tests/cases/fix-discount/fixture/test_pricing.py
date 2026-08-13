import unittest

from pricing import discounted_total


class DiscountedTotalTests(unittest.TestCase):
    def test_applies_percentage_discount(self):
        self.assertEqual(discounted_total([20.0, 30.0], 10), 45.0)

    def test_zero_discount(self):
        self.assertEqual(discounted_total([4.25, 5.75], 0), 10.0)

    def test_rejects_invalid_percentage(self):
        with self.assertRaises(ValueError):
            discounted_total([10.0], 101)


if __name__ == "__main__":
    unittest.main()
