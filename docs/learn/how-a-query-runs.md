# How a query runs

A database turns a sentence of SQL into rows. Fishbowl does it in four stages.
Each stage hands a more precise description to the next.

1. **Parse.** The text `SELECT name FROM fish WHERE id > 1` becomes a syntax
   tree: a SELECT with one output column, one table and one condition. At this
   point nobody has checked that `fish` exists. Fishbowl uses the `sqlparser`
   crate for this step.
2. **Plan.** The planner looks names up in the catalog, the list of tables and
   their columns. `id` becomes "column 0 of fish". Then it decides which steps
   will produce the answer and in what order. The result is a plan: a small tree
   of steps. See [query plans](query-plans.md).
3. **Execute.** The executor runs the plan. Rows flow up from the table through
   each step. See [the iterator model](iterator-model.md).
4. **Store.** Rows live in storage. For now that is a list in memory. Later it
   becomes pages in a file, organized as B-trees.

Splitting the work this way means each stage can be tested and replaced on its
own. It also means mistakes surface early: a misspelled column fails during
planning, before any row is read.

Code: `crates/engine/src/lib.rs` (`Database::execute`) wires the stages
together.
