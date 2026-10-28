use sqlparser::{
    dialect::GenericDialect,
    tokenizer::{Location, Token, TokenWithSpan, Tokenizer, TokenizerError},
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Empty,
    Complete,
    Incomplete,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Chunk {
    pub sql: String,
    pub line: usize,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SplitError {
    pub message: String,
    pub line: usize,
}

pub fn status(text: &str) -> Status {
    match tokenize(text) {
        Err(e) if is_unterminated(&e) => Status::Incomplete,
        Err(_) => Status::Complete,
        Ok(tokens) => match tokens.iter().rev().find(|t| !is_trivia(&t.token)) {
            None => Status::Empty,
            Some(t) if t.token == Token::SemiColon => Status::Complete,
            Some(_) => Status::Incomplete,
        },
    }
}

pub fn split(text: &str) -> Result<Vec<Chunk>, SplitError> {
    let tokens = tokenize(text).map_err(|e| SplitError {
        message: e.message,
        line: e.location.line as usize,
    })?;
    let offsets = Offsets::new(text);
    let mut chunks = Vec::new();
    let mut start: Option<Location> = None;
    for token in &tokens {
        if token.token == Token::SemiColon {
            if let Some(first) = start.take() {
                let from = offsets.byte(first);
                let to = offsets.byte(token.span.start);
                chunks.push(Chunk {
                    sql: text[from..to].trim().to_string(),
                    line: first.line as usize,
                });
            }
        } else if start.is_none() && !is_trivia(&token.token) {
            start = Some(token.span.start);
        }
    }
    if let Some(first) = start {
        chunks.push(Chunk {
            sql: text[offsets.byte(first)..].trim().to_string(),
            line: first.line as usize,
        });
    }
    Ok(chunks)
}

fn tokenize(text: &str) -> Result<Vec<TokenWithSpan>, TokenizerError> {
    Tokenizer::new(&GenericDialect {}, text).tokenize_with_location()
}

fn is_trivia(token: &Token) -> bool {
    matches!(token, Token::Whitespace(_) | Token::EOF)
}

fn is_unterminated(error: &TokenizerError) -> bool {
    error.message.contains("EOF") || error.message.starts_with("Unterminated")
}

struct Offsets<'a> {
    text: &'a str,
    line_starts: Vec<usize>,
}

impl<'a> Offsets<'a> {
    fn new(text: &'a str) -> Self {
        let line_starts = std::iter::once(0)
            .chain(text.match_indices('\n').map(|(i, _)| i + 1))
            .collect();
        Self { text, line_starts }
    }

    fn byte(&self, location: Location) -> usize {
        let line_start = self.line_starts[location.line as usize - 1];
        self.text[line_start..]
            .char_indices()
            .nth(location.column as usize - 1)
            .map_or(self.text.len(), |(i, _)| line_start + i)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sqls(text: &str) -> Vec<String> {
        split(text).unwrap().into_iter().map(|c| c.sql).collect()
    }

    #[test]
    fn splits_on_semicolons_outside_literals_and_comments() {
        assert_eq!(
            sqls("SELECT 1; SELECT 'a;b'; SELECT \"x;y\" FROM t -- c;d\n; /* e;f */ SELECT 2"),
            vec![
                "SELECT 1",
                "SELECT 'a;b'",
                "SELECT \"x;y\" FROM t -- c;d",
                "SELECT 2"
            ]
        );
    }

    #[test]
    fn keeps_doubled_quotes_and_multi_line_strings() {
        assert_eq!(
            sqls("INSERT INTO t VALUES ('it''s;\nfine');"),
            vec!["INSERT INTO t VALUES ('it''s;\nfine')"]
        );
    }

    #[test]
    fn skips_empty_and_comment_only_chunks() {
        assert!(sqls(";;  -- nothing\n/* here */ ;").is_empty());
        assert_eq!(sqls("-- lead\nSELECT 1;"), vec!["SELECT 1"]);
    }

    #[test]
    fn reports_the_line_each_statement_starts_on() {
        let chunks = split("SELECT 1;\n\n  SELECT\n2;").unwrap();
        assert_eq!(chunks[0].line, 1);
        assert_eq!(chunks[1].line, 3);
        assert_eq!(chunks[1].sql, "SELECT\n2");
    }
    #[test]
    fn status_tracks_completeness() {
        assert_eq!(status(""), Status::Empty);
        assert_eq!(
            status("  -- just a comment goo goo gaa gaa\n"),
            Status::Empty
        );
        assert_eq!(status("SELECT 1"), Status::Incomplete);
        assert_eq!(status("SELECT 1;"), Status::Complete);
        assert_eq!(status("SELECT 1; -- done"), Status::Complete);
        assert_eq!(status("SELECT 'a;"), Status::Incomplete);
        assert_eq!(status("SELECT \"a;"), Status::Incomplete);
        assert_eq!(status("SELECT 1 /* ; "), Status::Incomplete);
        assert_eq!(status("SELECT 1 /* /* */ ; "), Status::Incomplete);
        assert_eq!(status("SELECT 1 /* /* */ */ ;"), Status::Complete);
        assert_eq!(status("SELECT 1; SELECT"), Status::Incomplete);
    }

    #[test]
    fn input_the_tokenizer_rejects_is_complete_so_its_error_shows() {
        assert_eq!(status("SELECT 1 + ._x"), Status::Complete);
        let err = split("SELECT 1 + ._x").unwrap_err();
        assert_eq!(err.message, "Unexpected character '_'");
    }

    #[test]
    fn unterminated_input_is_a_split_error_with_a_line() {
        let err = split("SELECT 1;\nSELECT 'oops").unwrap_err();
        assert_eq!(err.line, 2);
    }
}
