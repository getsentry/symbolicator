use moka::Equivalent;
use symbolic::common::{Language, Name};
use symbolic::demangle::{Demangle, DemangleOptions};
use symbolic::symcache::Function;

/// Options for demangling all symbols.
const DEMANGLE_OPTIONS: DemangleOptions = DemangleOptions::complete().return_type(false);

/// A cache for demangled symbols.
#[derive(Debug, Clone)]
pub struct DemangleCache {
    cache: moka::sync::Cache<MangleKey, Option<String>>,
}

impl DemangleCache {
    pub fn new(max_capacity: u64) -> Self {
        let cache = moka::sync::Cache::builder()
            .max_capacity(max_capacity) // 10 MiB, considering key and value:
            .weigher(|k: &MangleKey, v: &Option<String>| {
                (k.symbol.len() + v.as_ref().map_or(0, |v| v.len()))
                    .try_into()
                    .unwrap_or(u32::MAX)
            })
            .build();

        Self { cache }
    }

    /// Demangles the name of the given [`Function`].
    ///
    /// Returns `None` if the function can't be de-mangled.
    pub fn demangle_function(&self, func: &Function<'_>) -> Option<String> {
        self.demangle(&func.name_for_demangling())
    }

    /// Demangles the given [`Name`].
    ///
    /// Returns `None` if the symbol can't be de-mangled.
    pub fn demangle(&self, name: &Name<'_>) -> Option<String> {
        let key = MangleKeyRef {
            symbol: name.as_str(),
            language: name.language(),
        };

        if let Some(demangled) = self.cache.get(&key) {
            return demangled;
        }

        let demangled = name.demangle(DEMANGLE_OPTIONS);
        self.cache.insert(key.to_owned(), demangled.clone());

        demangled
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MangleKey {
    symbol: String,
    language: Language,
}

impl std::hash::Hash for MangleKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        MangleKeyRef {
            symbol: &self.symbol,
            language: self.language,
        }
        .hash(state);
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
struct MangleKeyRef<'a> {
    symbol: &'a str,
    language: Language,
}

impl MangleKeyRef<'_> {
    fn to_owned(self) -> MangleKey {
        MangleKey {
            symbol: self.symbol.to_owned(),
            language: self.language,
        }
    }
}

impl<'a> Equivalent<MangleKey> for MangleKeyRef<'a> {
    fn equivalent(&self, key: &MangleKey) -> bool {
        let MangleKey { symbol, language } = key;
        self.symbol == symbol && self.language == *language
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_demangle_cache() {
        let cache = DemangleCache::new(1024);

        let name = Name::from("_ZN4core3fmt9Formatter3pad17h0123456789abcdefE");
        let result = cache.demangle(&name);
        assert_eq!(result.as_deref(), Some("core::fmt::Formatter::pad"));

        let result = cache.demangle(&name);
        assert_eq!(result.as_deref(), Some("core::fmt::Formatter::pad"));
    }

    #[test]
    fn test_ref_key_hash_equivalent() {
        use std::hash::{BuildHasher, RandomState};

        let state = RandomState::new();
        let owned = MangleKey {
            symbol: "_ZN4core3fmt9Formatter3pad17h0123456789abcdefE".to_owned(),
            language: Language::Rust,
        };
        let borrowed = MangleKeyRef {
            symbol: &owned.symbol,
            language: owned.language,
        };

        assert_eq!(state.hash_one(&owned), state.hash_one(borrowed));
        assert!(borrowed.equivalent(&owned));
    }
}
