//! Shared utility functions for gRPC services.

use consensus::state::address::Address;
use tonic::Status;

/// Parse a hex-encoded hash string into a 32-byte array.
pub fn parse_hash(hex_str: &str) -> Result<[u8; 32], Status> {
    let bytes = hex::decode(hex_str)
        .map_err(|e| Status::invalid_argument(format!("Invalid hex hash: {}", e)))?;

    if bytes.len() != 32 {
        return Err(Status::invalid_argument(format!(
            "Hash must be 32 bytes, got {}",
            bytes.len()
        )));
    }

    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    Ok(arr)
}

/// Parse a hex-encoded address string into an Address.
pub fn parse_address(hex_str: &str) -> Result<Address, Status> {
    let bytes = hex::decode(hex_str)
        .map_err(|e| Status::invalid_argument(format!("Invalid hex address: {}", e)))?;

    if bytes.len() != 32 {
        return Err(Status::invalid_argument(format!(
            "Address must be 32 bytes, got {}",
            bytes.len()
        )));
    }

    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    Ok(Address::from_bytes(arr))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hash_valid() {
        let hex = "0000000000000000000000000000000000000000000000000000000000000000";
        let result = parse_hash(hex);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), [0u8; 32]);
    }

    #[test]
    fn parse_hash_valid_nonzero() {
        let hex = "0102030405060708091011121314151617181920212223242526272829303132";
        let result = parse_hash(hex);
        assert!(result.is_ok());
    }

    #[test]
    fn parse_hash_invalid_hex() {
        let hex = "zzzz0000000000000000000000000000000000000000000000000000000000";
        let result = parse_hash(hex);
        assert!(result.is_err());
        let status = result.unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn parse_hash_wrong_length() {
        let hex = "0102030405"; // Only 5 bytes
        let result = parse_hash(hex);
        assert!(result.is_err());
    }

    #[test]
    fn parse_hash_empty() {
        let result = parse_hash("");
        assert!(result.is_err());
    }

    #[test]
    fn parse_address_valid() {
        let hex = "0000000000000000000000000000000000000000000000000000000000000000";
        let result = parse_address(hex);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().as_bytes(), &[0u8; 32]);
    }

    #[test]
    fn parse_address_valid_nonzero() {
        let hex = "0102030405060708091011121314151617181920212223242526272829303132";
        let result = parse_address(hex);
        assert!(result.is_ok());
    }

    #[test]
    fn parse_address_invalid_hex() {
        let hex = "zzzz0000000000000000000000000000000000000000000000000000000000";
        let result = parse_address(hex);
        assert!(result.is_err());
        let status = result.unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert!(status.message().contains("Invalid hex address"));
    }

    #[test]
    fn parse_address_too_short() {
        let hex = "0102030405"; // Only 5 bytes
        let result = parse_address(hex);
        assert!(result.is_err());
        let status = result.unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert!(status.message().contains("32 bytes"));
    }

    #[test]
    fn parse_address_too_long() {
        let hex = "000000000000000000000000000000000000000000000000000000000000000000"; // 33 bytes
        let result = parse_address(hex);
        assert!(result.is_err());
        let status = result.unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn parse_address_empty() {
        let result = parse_address("");
        assert!(result.is_err());
    }
}
