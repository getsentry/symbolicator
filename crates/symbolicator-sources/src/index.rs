use std::collections::BTreeSet;
use std::io::{self, BufRead, BufReader, Write};
use std::sync::Arc;

/// An index for a debug file source.
///
/// This can be used to check whether the source
/// contains a file with a given path. The answer is
/// taken as authoritative; if the index doesn't contain
/// the path we will not attempt to fetch the file from the source.
#[derive(Debug, Clone)]
pub enum SourceIndex {
    /// A Symstore index.
    Symstore(SymstoreIndex),
}

impl SourceIndex {
    /// Checks if this index contains the given path.
    pub fn contains(&self, path: &str) -> bool {
        match self {
            SourceIndex::Symstore(symstore_index) => symstore_index.contains(path),
        }
    }
}

/// A Microsoft Symstore index.
///
/// This index resides on the source itself in the
/// directory `000Admin`. Each upload of symbols creates
/// a numbered text file containing the names and IDs of the files
/// that were uploaded. The file `lastid.txt` contains the number
/// of the most current text file.
///
/// Lines in the upload text files look like this:
/// ```
/// "difx64.dll\4549B50183000","D:\DllServers\temp\prod-rs5-pv-2018-09-06-1006323\difx64.dll"
/// ```
/// The part before the comma contains the debug or code file name
/// and the debug or code ID, separated by a backslash.
///
/// See the index on Intel's symbol server for an example:
/// <https://software.intel.com/sites/downloads/symbols/000Admin>
#[derive(Debug, Clone, Default)]
pub struct SymstoreIndex {
    files: BTreeSet<Arc<str>>,
}

impl SymstoreIndex {
    /// Parses a line in a Symstore upload text file.
    ///
    /// A line of the form
    /// ```
    /// "<name>\<ID>","<other data>"
    /// is transformed into
    /// ```
    /// <name>/<ID>/<name>
    /// ```
    /// which corresponds to the path format for Symstore sources.
    fn parse_line(line: &str) -> Option<Arc<str>> {
        let entry = line.split('"').nth(1)?;
        let (name, id) = entry.split_once('\\')?;
        Some(format!("{name}/{id}/{name}").into())
    }

    /// Loads a `SymstoreIndex` from bytes.
    pub fn load(data: &[u8]) -> io::Result<Self> {
        let mut out = Self::default();
        let reader = BufReader::new(data);
        for line in reader.lines() {
            out.files.insert(line?.into());
        }
        Ok(out)
    }

    /// Parses a `SymstoreIndex` from a reader.
    pub fn parse_from_reader<R: BufRead>(reader: R) -> io::Result<Self> {
        let mut out = Self::default();
        for line in reader.lines() {
            let line = line?;
            if let Some(parsed) = Self::parse_line(&line) {
                out.files.insert(parsed);
            }
        }

        Ok(out)
    }

    /// Checks whether this index contains the given path.
    pub fn contains(&self, path: &str) -> bool {
        self.files.contains(path)
    }

    /// Returns an iterator over the paths contained in this index.
    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.files.iter().map(|s| &s[..])
    }

    /// Writes this index to a writer.
    pub fn write(&self, mut destination: impl Write) -> io::Result<()> {
        for line in &self.files {
            writeln!(destination, "{line}")?;
        }

        Ok(())
    }

    /// Appends the context of another index to this one.
    ///
    /// The other index is consumed.
    pub fn append(&mut self, other: Self) {
        self.files.extend(other.files)
    }

    /// Inserts a path into this index.
    #[cfg(test)]
    pub fn insert(mut self, path: &str) -> Self {
        self.files.insert(path.into());
        self
    }
}

impl From<SymstoreIndex> for SourceIndex {
    fn from(value: SymstoreIndex) -> Self {
        Self::Symstore(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_line() {
        let line = r#""igdail32.dll\5BA3F2382a000","D:\DllServers\temp\prod-rs5-pv-2018-09-06-1006323\igdail32.dll""#;
        assert_eq!(
            &*SymstoreIndex::parse_line(line).unwrap(),
            "igdail32.dll/5BA3F2382a000/igdail32.dll"
        )
    }
}
