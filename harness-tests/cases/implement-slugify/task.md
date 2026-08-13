# Implement slugify

Implement `slugify` in `text_utils.py` using only the Python standard library.

The result must:

- be lowercase ASCII;
- transliterate accented characters when possible (`"Café"` becomes
  `"cafe"`);
- replace each run of non-alphanumeric characters with one hyphen;
- contain no leading or trailing hyphen;
- return `"item"` when no alphanumeric characters remain.

Run the existing tests when you are done.
