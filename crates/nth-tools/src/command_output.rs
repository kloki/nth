//! Terminal escape cleanup for command output, with one parser per stream.

#[derive(Default)]
pub(crate) struct Cleaner {
    parser: vte::Parser,
}

impl Cleaner {
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut output = Text(Vec::new());
        self.parser.advance(&mut output, bytes);
        output.0
    }
}

struct Text(Vec<u8>);

impl vte::Perform for Text {
    fn print(&mut self, c: char) {
        let mut buf = [0; 4];
        self.0.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
    }

    fn execute(&mut self, byte: u8) {
        if matches!(byte, b'\n' | b'\r' | b'\t') {
            self.0.push(byte);
        }
    }
}

pub(crate) fn clean(bytes: &[u8]) -> String {
    String::from_utf8_lossy(&Cleaner::default().push(bytes)).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_sequences_and_unicode() {
        let mut cleaner = Cleaner::default();
        let mut out = Vec::new();
        for chunk in [
            b"\x1b[3".as_slice(),
            b"1m\xe2\x82",
            b"\xac\x1b]8;;http://exam",
            b"ple\x1b\\link\x1b]8;;\x07\tend\n",
        ] {
            out.extend(cleaner.push(chunk));
        }
        assert_eq!(String::from_utf8(out).unwrap(), "€link\tend\n");
    }
}
