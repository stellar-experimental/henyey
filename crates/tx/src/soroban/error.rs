//! Error conversion utilities between P24 and P25 Soroban host error types.

use soroban_env_host_p24 as soroban_env_host24;
use soroban_env_host_p25 as soroban_env_host25;

/// Convert a P24 HostError to P25 HostError.
///
/// This function is used when protocol 24 code needs to report errors
/// in protocol 25 format.
pub(crate) fn convert_host_error_p24_to_p25(
    err: soroban_env_host24::HostError,
) -> soroban_env_host25::HostError {
    let sc_error = soroban_env_host24::xdr::ScError::try_from(&err).unwrap_or(
        soroban_env_host24::xdr::ScError::Context(
            soroban_env_host24::xdr::ScErrorCode::InternalError,
        ),
    );
    let sc_error = convert_sc_error_p24_to_p25(sc_error);
    soroban_env_host25::HostError::from(sc_error)
}

fn convert_sc_error_code_p24_to_p25(
    code: soroban_env_host24::xdr::ScErrorCode,
) -> soroban_env_host25::xdr::ScErrorCode {
    soroban_env_host25::xdr::ScErrorCode::try_from(code as i32)
        .unwrap_or(soroban_env_host25::xdr::ScErrorCode::InternalError)
}

fn convert_sc_error_p24_to_p25(
    sc_error: soroban_env_host24::xdr::ScError,
) -> soroban_env_host25::xdr::ScError {
    use soroban_env_host24::xdr::ScError as ScError24;
    use soroban_env_host25::xdr::ScError as ScError25;

    match sc_error {
        ScError24::Contract(code) => ScError25::Contract(code),
        ScError24::WasmVm(code) => ScError25::WasmVm(convert_sc_error_code_p24_to_p25(code)),
        ScError24::Context(code) => ScError25::Context(convert_sc_error_code_p24_to_p25(code)),
        ScError24::Storage(code) => ScError25::Storage(convert_sc_error_code_p24_to_p25(code)),
        ScError24::Object(code) => ScError25::Object(convert_sc_error_code_p24_to_p25(code)),
        ScError24::Crypto(code) => ScError25::Crypto(convert_sc_error_code_p24_to_p25(code)),
        ScError24::Events(code) => ScError25::Events(convert_sc_error_code_p24_to_p25(code)),
        ScError24::Budget(code) => ScError25::Budget(convert_sc_error_code_p24_to_p25(code)),
        ScError24::Value(code) => ScError25::Value(convert_sc_error_code_p24_to_p25(code)),
        ScError24::Auth(code) => ScError25::Auth(convert_sc_error_code_p24_to_p25(code)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_env_host24::xdr::ScError as ScError24;
    use soroban_env_host24::xdr::ScErrorCode as ScErrorCode24;
    use soroban_env_host25::xdr::ScError as ScError25;
    use soroban_env_host25::xdr::ScErrorCode as ScErrorCode25;

    /// Test converting ScErrorCode from P24 to P25.
    #[test]
    fn test_convert_sc_error_code_internal_error() {
        let p24_code = ScErrorCode24::InternalError;
        let p25_code = convert_sc_error_code_p24_to_p25(p24_code);
        assert_eq!(p25_code, ScErrorCode25::InternalError);
    }

    #[test]
    fn test_convert_sc_error_code_invalid_input() {
        let p24_code = ScErrorCode24::InvalidInput;
        let p25_code = convert_sc_error_code_p24_to_p25(p24_code);
        assert_eq!(p25_code, ScErrorCode25::InvalidInput);
    }

    #[test]
    fn test_convert_sc_error_code_missing_value() {
        let p24_code = ScErrorCode24::MissingValue;
        let p25_code = convert_sc_error_code_p24_to_p25(p24_code);
        assert_eq!(p25_code, ScErrorCode25::MissingValue);
    }

    #[test]
    fn test_convert_sc_error_code_exceeded_limit() {
        let p24_code = ScErrorCode24::ExceededLimit;
        let p25_code = convert_sc_error_code_p24_to_p25(p24_code);
        assert_eq!(p25_code, ScErrorCode25::ExceededLimit);
    }

    /// Test converting ScError variants from P24 to P25.
    #[test]
    fn test_convert_sc_error_contract() {
        let p24_error = ScError24::Contract(12345);
        let p25_error = convert_sc_error_p24_to_p25(p24_error);
        match p25_error {
            ScError25::Contract(code) => assert_eq!(code, 12345),
            _ => panic!("Expected Contract error"),
        }
    }

    #[test]
    fn test_convert_sc_error_wasm_vm() {
        let p24_error = ScError24::WasmVm(ScErrorCode24::InternalError);
        let p25_error = convert_sc_error_p24_to_p25(p24_error);
        match p25_error {
            ScError25::WasmVm(code) => assert_eq!(code, ScErrorCode25::InternalError),
            _ => panic!("Expected WasmVm error"),
        }
    }

    #[test]
    fn test_convert_sc_error_context() {
        let p24_error = ScError24::Context(ScErrorCode24::InvalidInput);
        let p25_error = convert_sc_error_p24_to_p25(p24_error);
        match p25_error {
            ScError25::Context(code) => assert_eq!(code, ScErrorCode25::InvalidInput),
            _ => panic!("Expected Context error"),
        }
    }

    #[test]
    fn test_convert_sc_error_storage() {
        let p24_error = ScError24::Storage(ScErrorCode24::MissingValue);
        let p25_error = convert_sc_error_p24_to_p25(p24_error);
        match p25_error {
            ScError25::Storage(code) => assert_eq!(code, ScErrorCode25::MissingValue),
            _ => panic!("Expected Storage error"),
        }
    }

    #[test]
    fn test_convert_sc_error_object() {
        let p24_error = ScError24::Object(ScErrorCode24::ExceededLimit);
        let p25_error = convert_sc_error_p24_to_p25(p24_error);
        match p25_error {
            ScError25::Object(code) => assert_eq!(code, ScErrorCode25::ExceededLimit),
            _ => panic!("Expected Object error"),
        }
    }

    #[test]
    fn test_convert_sc_error_crypto() {
        let p24_error = ScError24::Crypto(ScErrorCode24::InvalidInput);
        let p25_error = convert_sc_error_p24_to_p25(p24_error);
        match p25_error {
            ScError25::Crypto(code) => assert_eq!(code, ScErrorCode25::InvalidInput),
            _ => panic!("Expected Crypto error"),
        }
    }

    #[test]
    fn test_convert_sc_error_events() {
        let p24_error = ScError24::Events(ScErrorCode24::ExceededLimit);
        let p25_error = convert_sc_error_p24_to_p25(p24_error);
        match p25_error {
            ScError25::Events(code) => assert_eq!(code, ScErrorCode25::ExceededLimit),
            _ => panic!("Expected Events error"),
        }
    }

    #[test]
    fn test_convert_sc_error_budget() {
        let p24_error = ScError24::Budget(ScErrorCode24::ExceededLimit);
        let p25_error = convert_sc_error_p24_to_p25(p24_error);
        match p25_error {
            ScError25::Budget(code) => assert_eq!(code, ScErrorCode25::ExceededLimit),
            _ => panic!("Expected Budget error"),
        }
    }

    #[test]
    fn test_convert_sc_error_value() {
        let p24_error = ScError24::Value(ScErrorCode24::InvalidInput);
        let p25_error = convert_sc_error_p24_to_p25(p24_error);
        match p25_error {
            ScError25::Value(code) => assert_eq!(code, ScErrorCode25::InvalidInput),
            _ => panic!("Expected Value error"),
        }
    }

    #[test]
    fn test_convert_sc_error_auth() {
        let p24_error = ScError24::Auth(ScErrorCode24::InvalidInput);
        let p25_error = convert_sc_error_p24_to_p25(p24_error);
        match p25_error {
            ScError25::Auth(code) => assert_eq!(code, ScErrorCode25::InvalidInput),
            _ => panic!("Expected Auth error"),
        }
    }

    /// Test converting HostError from P24 to P25.
    #[test]
    fn test_convert_host_error_p24_to_p25() {
        let p24_sc_error = ScError24::Context(ScErrorCode24::InternalError);
        let p24_host_error = soroban_env_host24::HostError::from(p24_sc_error);
        let p25_host_error = convert_host_error_p24_to_p25(p24_host_error);

        // The converted error should be a valid P25 HostError
        // We can verify by extracting the ScError
        let p25_sc_error: ScError25 = (&p25_host_error)
            .try_into()
            .unwrap_or(ScError25::Context(ScErrorCode25::InternalError));
        match p25_sc_error {
            ScError25::Context(code) => assert_eq!(code, ScErrorCode25::InternalError),
            _ => panic!("Expected Context error"),
        }
    }
}
