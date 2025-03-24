use std::collections::BTreeSet;
use std::io::{self, BufRead};
use std::sync::Arc;

use symbolic::common::CodeId;

struct SymstoreIndex {
    files: BTreeSet<String>,
}

impl SymstoreIndex {
    fn parse_line(line: &str) -> Option<String> {
        let entry = line.split('"').nth(1)?;
        let (name, id) = entry.split_once('\\')?;
        Some(format!("{name}/{id}/{name}"))
    }

    fn extend_from_reader<R: BufRead>(reader: R) -> io::Error {
        for line in reader.lines {
            let line = line?;
            if let Some(parsed) = Self::parse_line(&line) {
                self.files.insert(parsed)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_line() {
        let line = r#""igdail32.dll\5BA3F2382a000","D:\DllServers\temp\prod-rs5-pv-2018-09-06-1006323\igdail32.dll""#;
        assert_eq!(
            SymstoreIndex::parse_line(line).unwrap(),
            "igdail32.dll/5BA3F2382a000/igdail32.dll"
        )
    }
}
