const MAX_JSON_DEPTH: usize = 256;

pub fn is_json(text: &str) -> bool {
    let mut parser = Json {
        bytes: text.as_bytes(),
        pos: 0,
    };
    parser.skip_space();
    if !parser.value(0) {
        return false;
    }
    parser.skip_space();
    parser.pos == parser.bytes.len()
}

struct Json<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl Json<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn eat(&mut self, byte: u8) -> bool {
        if self.peek() == Some(byte) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn skip_space(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn word(&mut self, word: &[u8]) -> bool {
        if self.bytes[self.pos..].starts_with(word) {
            self.pos += word.len();
            true
        } else {
            false
        }
    }

    fn value(&mut self, depth: usize) -> bool {
        if depth > MAX_JSON_DEPTH {
            return false;
        }
        match self.peek() {
            Some(b'{') => self.object(depth),
            Some(b'[') => self.array(depth),
            Some(b'"') => self.string(),
            Some(b't') => self.word(b"true"),
            Some(b'f') => self.word(b"false"),
            Some(b'n') => self.word(b"null"),
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ => false,
        }
    }

    fn object(&mut self, depth: usize) -> bool {
        self.pos += 1;
        self.skip_space();
        if self.eat(b'}') {
            return true;
        }
        loop {
            self.skip_space();
            if !self.string() {
                return false;
            }
            self.skip_space();
            if !self.eat(b':') {
                return false;
            }
            self.skip_space();
            if !self.value(depth + 1) {
                return false;
            }
            self.skip_space();
            if self.eat(b'}') {
                return true;
            }
            if !self.eat(b',') {
                return false;
            }
        }
    }

    fn array(&mut self, depth: usize) -> bool {
        self.pos += 1;
        self.skip_space();
        if self.eat(b']') {
            return true;
        }
        loop {
            self.skip_space();
            if !self.value(depth + 1) {
                return false;
            }
            self.skip_space();
            if self.eat(b']') {
                return true;
            }
            if !self.eat(b',') {
                return false;
            }
        }
    }

    fn string(&mut self) -> bool {
        if !self.eat(b'"') {
            return false;
        }
        while let Some(byte) = self.peek() {
            self.pos += 1;
            match byte {
                b'"' => return true,
                b'\\' => match self.peek() {
                    Some(b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') => self.pos += 1,
                    Some(b'u') => {
                        self.pos += 1;
                        if !self.digits(4, |b| b.is_ascii_hexdigit()) {
                            return false;
                        }
                    }
                    _ => return false,
                },
                0x00..=0x1f => return false,
                _ => {}
            }
        }
        false
    }

    fn digits(&mut self, exact: usize, ok: impl Fn(u8) -> bool) -> bool {
        let end = self.pos + exact;
        if end > self.bytes.len() || !self.bytes[self.pos..end].iter().all(|&b| ok(b)) {
            return false;
        }
        self.pos = end;
        true
    }

    fn some_digits(&mut self) -> bool {
        let start = self.pos;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
        self.pos > start
    }

    fn number(&mut self) -> bool {
        self.eat(b'-');
        if !self.eat(b'0') && !self.some_digits() {
            return false;
        }
        if self.eat(b'.') && !self.some_digits() {
            return false;
        }
        if self.eat(b'e') || self.eat(b'E') {
            if !self.eat(b'+') {
                self.eat(b'-');
            }
            if !self.some_digits() {
                return false;
            }
        }
        true
    }
}

pub fn canonical_timestamp(text: &str) -> Option<String> {
    let s = text.trim().as_bytes();
    let number = |range: std::ops::Range<usize>| -> Option<u32> {
        let part = s.get(range)?;
        if !part.iter().all(u8::is_ascii_digit) {
            return None;
        }
        std::str::from_utf8(part).ok()?.parse().ok()
    };

    let year = number(0..4)?;
    let month = number(5..7)?;
    let day = number(8..10)?;
    if s.get(4) != Some(&b'-') || s.get(7) != Some(&b'-') {
        return None;
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return None,
    };
    if year == 0 || day == 0 || day > days {
        return None;
    }

    let (mut hour, mut minute, mut second, mut fraction) = (0, 0, 0, "");
    if s.len() > 10 {
        if !matches!(s[10], b' ' | b'T') || s.get(13) != Some(&b':') {
            return None;
        }
        hour = number(11..13)?;
        minute = number(14..16)?;
        let mut pos = 16;
        if s.get(pos) == Some(&b':') {
            second = number(17..19)?;
            pos = 19;
            if s.get(pos) == Some(&b'.') {
                let digits = &s[pos + 1..];
                if digits.is_empty() || digits.len() > 9 || !digits.iter().all(u8::is_ascii_digit) {
                    return None;
                }
                fraction = std::str::from_utf8(digits).ok()?.trim_end_matches('0');
                pos = s.len();
            }
        }
        if pos != s.len() || hour > 23 || minute > 59 || second > 59 {
            return None;
        }
    }

    let mut out = format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}");
    if !fraction.is_empty() {
        out.push('.');
        out.push_str(fraction);
    }
    Some(out)
}

pub fn canonical_uuid(text: &str) -> Option<String> {
    let mut s = text.trim();
    if let Some(inner) = s.strip_prefix('{').and_then(|t| t.strip_suffix('}')) {
        s = inner;
    }
    let hex: String = match s.len() {
        32 => s.to_string(),
        36 if [8, 13, 18, 23].iter().all(|&i| s.as_bytes()[i] == b'-') => s.replace('-', ""),
        _ => return None,
    };
    if hex.len() != 32 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let hex = hex.to_ascii_lowercase();
    Some(format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_accepts_the_grammar_and_nothing_else() {
        for good in [
            "{}",
            "[]",
            " {\"a\": [1, 2.5, -0, 1e10, 2E-3, true, false, null, \"x\\n\\u00e9\"]} ",
            "\"plain\"",
            "0",
            "-12.5e+3",
            "[[[]]]",
        ] {
            assert!(is_json(good), "{good}");
        }
        for bad in [
            "",
            "{",
            "{bad",
            "[1,]",
            "{\"a\" 1}",
            "{a: 1}",
            "01",
            "1.",
            ".5",
            "-",
            "1e",
            "tru",
            "nul",
            "[1] 2",
            "'x'",
            "\"\\x\"",
            "\"\\u12\"",
            "\"open",
            "\"tab\there\"",
            "NaN",
        ] {
            assert!(!is_json(bad), "{bad}");
        }
    }

    #[test]
    fn json_nesting_is_bounded() {
        let deep = "[".repeat(100_000);
        assert!(!is_json(&deep));
        let ok = format!("{}{}", "[".repeat(200), "]".repeat(200));
        assert!(is_json(&ok));
    }

    #[test]
    fn timestamps_are_checked_and_canonical() {
        let cases = [
            ("2024-01-02", "2024-01-02 00:00:00"),
            ("2024-01-02 03:04", "2024-01-02 03:04:00"),
            ("2024-01-02T03:04:05", "2024-01-02 03:04:05"),
            (" 2024-02-29 23:59:59.120 ", "2024-02-29 23:59:59.12"),
            ("2000-02-29 00:00:00.000", "2000-02-29 00:00:00"),
            (
                "9999-12-31 23:59:59.999999999",
                "9999-12-31 23:59:59.999999999",
            ),
        ];
        for (input, want) in cases {
            assert_eq!(canonical_timestamp(input).as_deref(), Some(want), "{input}");
        }
        for bad in [
            "2024-13-01",
            "2023-02-29",
            "1900-02-29",
            "2024-04-31",
            "0000-01-01",
            "2024-1-2",
            "24-01-02",
            "2024-01-02 24:00:00",
            "2024-01-02 23:60",
            "2024-01-02 23:59:60",
            "2024-01-02 3:04:05",
            "2024-01-02 03:04:05.",
            "2024-01-02 03:04:05.1234567890",
            "2024-01-02 03:04:05Z",
            "2024-01-02x03:04",
            "now",
            "",
        ] {
            assert_eq!(canonical_timestamp(bad), None, "{bad}");
        }
    }

    #[test]
    fn canonical_timestamps_sort_as_text_in_time_order() {
        let ordered = [
            "0001-01-01",
            "2024-01-02",
            "2024-01-02 00:00:00.000001",
            "2024-01-02 00:00:00.1",
            "2024-01-02 00:00:00.10001",
            "2024-01-02 00:00:00.5",
            "2024-01-02 00:00:01",
            "2024-01-02T10:00",
        ];
        let canonical: Vec<String> = ordered
            .iter()
            .map(|t| canonical_timestamp(t).unwrap())
            .collect();
        let mut sorted = canonical.clone();
        sorted.sort();
        assert_eq!(canonical, sorted);
    }

    #[test]
    fn uuids_are_checked_and_canonical() {
        let want = "123e4567-e89b-12d3-a456-426614174000";
        for input in [
            want,
            "123E4567-E89B-12D3-A456-426614174000",
            "123e4567e89b12d3a456426614174000",
            "{123e4567-e89b-12d3-a456-426614174000}",
            " 123e4567-e89b-12d3-a456-426614174000 ",
        ] {
            assert_eq!(canonical_uuid(input).as_deref(), Some(want), "{input}");
        }
        for bad in [
            "nope",
            "123e4567-e89b-12d3-a456-42661417400",
            "123e4567-e89b-12d3-a456-4266141740000",
            "123e4567xe89b-12d3-a456-426614174000",
            "123e4567-e89b-12d3-a456-42661417400g",
            "{123e4567-e89b-12d3-a456-426614174000",
            "12-3e4567e89b-12d3-a456-426614174000",
            "é23e4567-e89b-12d3-a456-42661417400",
        ] {
            assert_eq!(canonical_uuid(bad), None, "{bad}");
        }
    }
}
