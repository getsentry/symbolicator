/// Ensures at compile time that the condition is true.
///
/// See <https://github.com/rust-lang/rfcs/issues/2790>
#[macro_export]
macro_rules! static_assert {
    ($condition:expr) => {
        const _: &() = &[()][1 - ($condition) as usize];
    };
}
