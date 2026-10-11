# Types and affinity

Most databases give each column a strict type and reject values that do not fit.
SQLite, and so Fishbowl, works differently: **each value carries its own type**.
A value is NULL, an integer, a real (floating-point number) or text. Blobs come
later.

A column's declared type only gives it an **affinity**, a preferred type. When a
value is stored, the database converts it toward that preference if it can do
so without losing information:

- Insert the text `'42'` into an `INTEGER` column and it is stored as the
  integer 42.
- Insert `'abc'` into the same column and it stays text. No error.
- Insert 3 into a `REAL` column and it is stored as 3.0.

The affinity comes from the declared type name, using five rules checked in
order. Contains "INT": integer. Contains "CHAR", "CLOB" or "TEXT": text. Contains
"BLOB" or no type at all: none. Contains "REAL", "FLOA" or "DOUB": real.
Anything else: numeric. So `VARCHAR(10)` is text, and the odd name `CHARINT` is
integer because the INT rule comes first.

## Arithmetic follows the same spirit

- `7 / 2` is 3. Both sides are integers, so the division is an integer division.
- `7.0 / 2` is 3.5.
- `1 / 0` is NULL, not an error.
- `'12abc' + 1` is 13. Text in arithmetic uses its leading number.
- If an integer sum overflows 64 bits, the result becomes a real.

Fishbowl copies these rules because the sqllogictest expected answers come from
SQLite. Try the "expressions" example in the UI.

Code: `crates/engine/src/value.rs` (`Affinity`, `apply_affinity`) and
`crates/engine/src/expr.rs` (`arithmetic`).
