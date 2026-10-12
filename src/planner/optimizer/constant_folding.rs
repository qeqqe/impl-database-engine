//! If an expression has no column reference then we can say that the it has the
//! same value for every row, so instead of computing the expression every row
//! we can compute it once and optimize the query execution with less computation.
