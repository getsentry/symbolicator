use std::collections::BTreeSet;
use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::Path;

#[derive(Debug, Clone, Default)]
struct SymstoreIndex {
    files: BTreeSet<String>,
}

impl SymstoreIndex {
    fn parse_line(line: &str) -> Option<String> {
        let entry = line.split('"').nth(1)?;
        let (name, id) = entry.split_once('\\')?;
        Some(format!("{name}/{id}/{name}"))
    }

    fn extend_from_reader<R: BufRead>(&mut self, reader: R) -> Result<(), io::Error> {
        for line in reader.lines() {
            let line = line?;
            if let Some(parsed) = Self::parse_line(&line) {
                self.files.insert(parsed);
            }
        }

        Ok(())
    }

    fn extend_from_file<P: AsRef<Path>>(&mut self, path: P) -> Result<(), io::Error> {
        let f = File::open(path)?;
        let f = BufReader::new(f);
        self.extend_from_reader(f)
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Write;

    use super::*;

    #[test]
    fn parse_line() {
        let line = r#""igdail32.dll\5BA3F2382a000","D:\DllServers\temp\prod-rs5-pv-2018-09-06-1006323\igdail32.dll""#;
        assert_eq!(
            SymstoreIndex::parse_line(line).unwrap(),
            "igdail32.dll/5BA3F2382a000/igdail32.dll"
        )
    }

    #[test]
    fn test_extend_from() {
        let mut index = SymstoreIndex::default();
        for i in 1..=209 {
            let path = format!("/Users/sebastian/Downloads/Intel/000Admin/{i:0>10}");
            index.extend_from_file(&path).unwrap();
        }

        let mut out = String::new();
        for file in &index.files {
            writeln!(&mut out, "{file}").unwrap();
        }

        dbg!(out.len());
    }
}
