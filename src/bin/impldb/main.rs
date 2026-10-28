use std::path::PathBuf;

use crate::format::Mode;

mod format;
mod shell;
mod split;

const USAGE: &str = "\
usage: impldb [OPTIONS] DIR
rest uhh... let me think..
";

#[derive(Clone, PartialEq, Eq, Debug)]
enum Input {
    Sql(String),
    File(String),
}

#[derive(PartialEq, Eq)]
struct Args {
    dir: PathBuf,
    inputs: Vec<Input>,
    mode: Mode,
    headers: bool,
    bail: bool,
}

fn main() {}
