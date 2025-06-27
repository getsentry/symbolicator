use symbolic::common::{Language, Name};
use symbolic::demangle::{Demangle, DemangleOptions};
use symbolic::symcache::Function;

/// Options for demangling all symbols.
pub const DEMANGLE_OPTIONS: DemangleOptions = DemangleOptions::complete().return_type(false);

/// A cache for demangled symbols
pub type DemangleCache = moka::sync::Cache<(String, Language), String>;

/// Demangles the name of the given [`Function`].
pub fn demangle_symbol(cache: &DemangleCache, func: &Function) -> (String, String) {
    let symbol = func.name();
    let language = func.language();
    let key = (symbol.to_string(), language);

    let init = || {
        func.name_for_demangling()
            .demangle(DEMANGLE_OPTIONS)
            .unwrap_or_else(|| report_demangling_failure(symbol.to_string(), language))
    };

    let entry = cache.entry_by_ref(&key).or_insert_with(init);

    (key.0, entry.into_value())
}

#[allow(unused)] // we early return `symbol` here for now, but we might change that in the future
fn report_demangling_failure(symbol: String, language: Language) -> String {
    return symbol;

    // Detect the language from the bare name, ignoring any pre-set language. There are a few
    // languages that we should always be able to demangle. Only complain about those that we
    // detect explicitly, but silently ignore the rest. For instance, there are C-identifiers
    // reported as C++, which are expected not to demangle.
    let detected_language = Name::from(symbol).detect_language();
    let should_demangle = match (language, detected_language) {
        (_, Language::Unknown) => false, // can't demangle what we cannot detect
        (Language::ObjCpp, Language::Cpp) => true, // C++ demangles even if it was in ObjC++
        (Language::Unknown, _) => true,  // if there was no language, then rely on detection
        (lang, detected) => lang == detected, // avoid false-positive detections
    };

    if should_demangle {
        sentry::with_scope(
            |scope| scope.set_extra("identifier", symbol.clone().into()),
            || {
                let message = format!("Failed to demangle {language} identifier");
                sentry::capture_message(&message, sentry::Level::Error);
            },
        );
    }
    symbol
}
