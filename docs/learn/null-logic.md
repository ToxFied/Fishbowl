# NULL and three-valued logic

`NULL` in SQL means "unknown". It is not zero and not an empty string. That one
idea has surprising consequences.

If a fish's weight is unknown, is it greater than 1? Unknown. So `NULL > 1` is
`NULL`, not false. Even `NULL = NULL` is `NULL`: two unknown values are not
known to be equal. To test for NULL you write `weight IS NULL`, which always
answers true or false.

Logic therefore has three values: true, false and unknown.

| a | b | a AND b | a OR b |
| --- | --- | --- | --- |
| true | unknown | unknown | true |
| false | unknown | false | unknown |
| unknown | unknown | unknown | unknown |

The rule of thumb: if the known side already decides the answer, the unknown
side does not matter. `false AND anything` is false. `true OR anything` is true.

## Where it bites

`WHERE` keeps a row only when the condition is **true**. Unknown counts as not
true. So `SELECT * FROM fish WHERE weight > 1` skips fish with no weight, and so
does `WHERE NOT (weight > 1)`. A row can vanish from both a query and its
opposite. That is correct SQL, and a common source of bugs.

Try it in the UI with the "nulls" example.

Code: `crates/engine/src/expr.rs` (`eval_binary`) and `Value::truthiness` in
`crates/engine/src/value.rs`.
