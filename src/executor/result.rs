use std::fmt;

use crate::planner::value::{Row, Value};

#[derive(Debug, Clone, PartialEq)]
pub enum StatementResult {
    Rows {
        columns: Vec<String>,
        rows: Vec<Row>,
    },
    Affected {
        verb: &'static str,
        rows: usize,
    },
    Done(&'static str),
}

impl StatementResult {
    pub fn rows(&self) -> &[Row] {
        match self {
            StatementResult::Rows { rows, .. } => rows,
            _ => &[],
        }
    }

    pub fn affected(&self) -> Option<usize> {
        match self {
            StatementResult::Affected { rows, .. } => Some(*rows),
            _ => None,
        }
    }
}

impl fmt::Display for StatementResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StatementResult::Done(tag) => f.write_str(tag),
            StatementResult::Affected { verb, rows } => write!(f, "{verb} {rows}"),
            StatementResult::Rows { columns, rows } => {
                let cells: Vec<Vec<String>> = rows
                    .iter()
                    .map(|row| row.iter().map(Value::to_string).collect())
                    .collect();
                let widths: Vec<usize> = columns
                    .iter()
                    .enumerate()
                    .map(|(i, name)| {
                        cells
                            .iter()
                            .map(|row| row[i].chars().count())
                            .chain(std::iter::once(name.chars().count()))
                            .max()
                            .unwrap_or(0)
                    })
                    .collect();
                let line = |f: &mut fmt::Formatter<'_>, values: &[String]| -> fmt::Result {
                    let padded: Vec<String> = values
                        .iter()
                        .zip(&widths)
                        .map(|(v, w)| format!("{v:<w$}"))
                        .collect();
                    writeln!(f, "{}", padded.join(" | ").trim_end())
                };
                line(f, columns)?;
                let rule: Vec<String> = widths.iter().map(|w| "-".repeat(*w)).collect();
                writeln!(f, "{}", rule.join("-+-"))?;
                for row in &cells {
                    line(f, row)?;
                }
                let noun = if rows.len() == 1 { "row" } else { "rows" };
                write!(f, "({} {noun})", rows.len())
            }
        }
    }
}
