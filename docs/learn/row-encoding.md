# Row encoding

A row in memory is a list of values. On a page it has to be a run of bytes.
**Row encoding** is the rule that turns one into the other and back.

Fishbowl writes the number of columns, then each value as a one-byte type tag
followed by its bytes:

| Tag | Type | Bytes after the tag |
| ---: | --- | --- |
| 0 | NULL | none |
| 1 | INTEGER | 8, big-endian |
| 2 | REAL | 8, an IEEE 754 double |
| 3 | TEXT | a 4-byte length, then the UTF-8 text |

So `(7, NULL, 'hi')` becomes 19 bytes:

```text
00 03                         3 columns
01 00 00 00 00 00 00 00 07    INTEGER 7
00                            NULL
03 00 00 00 02 68 69          TEXT, 2 bytes: "hi"
```

The tag is needed because a SQLite column can hold any type, so the bytes must
say which type each value is. See [types and affinity](types-and-affinity.md).

## What SQLite does differently

SQLite puts all the type tags first, in a header, and the values after. Its
tags also encode the size, so it can jump straight to column 5 without reading
columns 1 to 4. It also stores small integers in fewer bytes. Fishbowl keeps
tag and value side by side and integers at 8 bytes. That uses more space, but a
row is easy to read in a hex dump, which matters for a database built to be
looked at.

## Rows bigger than a page

A row of 10,000 characters cannot fit in a 4 KiB page. Rows over 1,000 bytes go
to **overflow pages** instead. The leaf keeps the rowid, the total length and
the number of the first overflow page. Each overflow page holds about 4 KiB of
the row and the number of the next one, like a chain.

The 1,000-byte limit means a leaf always fits at least four rows. That
guarantees a full leaf can be split into two halves that both fit. See
[splits and merges](splits-and-merges.md).

Code: `crates/engine/src/storage/record.rs` (rows) and
`crates/engine/src/storage/btree.rs` (`store_payload`, overflow chains).
