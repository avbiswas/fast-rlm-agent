import unittest

from text_utils import slugify


class SlugifyTests(unittest.TestCase):
    def test_words_and_punctuation(self):
        self.assertEqual(slugify("Hello, World!"), "hello-world")

    def test_accents(self):
        self.assertEqual(slugify("Café Crème"), "cafe-creme")

    def test_empty_fallback(self):
        self.assertEqual(slugify("---"), "item")


if __name__ == "__main__":
    unittest.main()
