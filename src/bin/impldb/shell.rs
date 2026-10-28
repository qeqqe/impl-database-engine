use crate::format::Mode;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    Continue,
    Stop,
    Quit,
}

#[derive(Debug, Copy, Clone)]
pub struct Settings {
    pub mode: Mode,
    pub headers: bool,
    pub timer: bool,
    pub bail: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            mode: Mode::Table,
            headers: true,
            timer: false,
            bail: false,
        }
    }
}
