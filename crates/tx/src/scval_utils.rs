//! Shared ScVal construction helpers for events and transaction meta.

use stellar_xdr::{ScString, ScSymbol, ScVal, StringM};

/// Create a ScVal::Symbol from a string.
pub(crate) fn make_symbol_scval(value: &str) -> ScVal {
    let sym = ScSymbol(StringM::try_from(value).expect("symbol must fit in XDR StringM"));
    ScVal::Symbol(sym)
}

/// Create a ScVal::String from a string.
pub(crate) fn make_string_scval(value: &str) -> ScVal {
    ScVal::String(ScString(
        StringM::try_from(value).expect("string must fit in XDR StringM"),
    ))
}
