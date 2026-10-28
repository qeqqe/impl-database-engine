#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Table,
    List,
    Csv,
    Line,
}

impl Mode {
    pub const NAMES: [&'static str; 4] = ["table", "list", "csv", "line"];

    pub fn parse(name: &str) -> Option<Mode> {
        match name.to_ascii_lowercase().as_str() {
            "table" => Some(Mode::Table),
            "list" => Some(Mode::List),
            "csv" => Some(Mode::Csv),
            "line" => Some(Mode::Line),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Mode::Table => "table",
            Mode::List => "list",
            Mode::Csv => "csv",
            Mode::Line => "line",
        }
    }
}
