use std::collections::BTreeSet;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Write};
use std::path::Path;
use std::sync::Arc;

#[derive(Debug, Clone, Default)]
pub struct SymstoreIndexBuilder(BTreeSet<String>);

impl SymstoreIndexBuilder {
    fn parse_line(line: &str) -> Option<String> {
        let entry = line.split('"').nth(1)?;
        let (name, id) = entry.split_once('\\')?;
        Some(format!("{name}/{id}/{name}"))
    }

    pub fn extend_from_reader<R: BufRead>(&mut self, reader: R) -> Result<(), io::Error> {
        for line in reader.lines() {
            let line = line?;
            if let Some(parsed) = Self::parse_line(&line) {
                self.0.insert(parsed);
            }
        }

        Ok(())
    }

    pub fn extend_from_file<P: AsRef<Path>>(&mut self, path: P) -> Result<(), io::Error> {
        let f = File::open(path)?;
        let f = BufReader::new(f);
        self.extend_from_reader(f)
    }

    pub fn build(self) -> SymstoreIndex {
        SymstoreIndex {
            files: Arc::new(self.0),
        }
    }

    #[cfg(test)]
    pub fn insert(mut self, path: &str) -> Self {
        self.0.insert(path.to_string());
        self
    }

    pub fn append(&mut self, other: Self) {
        self.0.extend(other.0)
    }

    pub fn write(&self, mut destination: impl Write) -> io::Result<()> {
        for line in &self.0 {
            writeln!(destination, "{line}")?;
        }

        Ok(())
    }
}

#[derive(Debug, Clone, Default)]
pub struct SymstoreIndex {
    pub files: Arc<BTreeSet<String>>,
}

impl SymstoreIndex {
    pub fn builder() -> SymstoreIndexBuilder {
        SymstoreIndexBuilder::default()
    }

    pub fn load(data: &[u8]) -> io::Result<Self> {
        let reader = BufReader::new(data);
        let mut files = BTreeSet::new();
        for line in reader.lines() {
            files.insert(line?);
        }

        Ok(Self {
            files: Arc::new(files),
        })
    }

    pub fn contains(&self, path: &str) -> bool {
        self.files.contains(path)
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
            SymstoreIndexBuilder::parse_line(line).unwrap(),
            "igdail32.dll/5BA3F2382a000/igdail32.dll"
        )
    }

    #[test]
    fn test_extend_from() {
        let mut builder = SymstoreIndex::builder();
        for i in 1..=209 {
            let path = format!("/Users/sebastian/Downloads/Intel/000Admin/{i:0>10}");
            builder.extend_from_file(&path).unwrap();
        }

        let index = builder.build();

        let mut out = String::new();
        for file in &*index.files {
            writeln!(&mut out, "{file}").unwrap();
        }

        let index2 = SymstoreIndex::load(out.as_bytes()).unwrap();
        assert_eq!(index.files, index2.files);
    }
}
