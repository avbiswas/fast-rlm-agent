# Rust Memory Guide

## Ownership

Every value has one owner, and dropping that owner releases the value. Assigning
heap-backed data can move it, leaving the old binding unusable. Practical rule:
expect a function argument to move a value unless it borrows it.

## References and Borrowing

References let code borrow a value without taking ownership. Many immutable
borrows or one mutable borrow may exist at a time. Practical rule: use `&T` for
shared access and `&mut T` only when mutation is required.

## Slices

A slice borrows a contiguous portion of a collection. A string slice, `&str`,
contains a pointer and length rather than owning the text. Practical rule:
accept `&str` when a function only needs to read string data.

## Sources

- https://doc.rust-lang.org/book/ch04-01-what-is-ownership.html
- https://doc.rust-lang.org/book/ch04-02-references-and-borrowing.html
- https://doc.rust-lang.org/book/ch04-03-slices.html
