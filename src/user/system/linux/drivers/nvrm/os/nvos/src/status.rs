//! RM's `NV_STATUS` codes this layer returns, from NVIDIA's
//! `nvstatuscodes.h` at 580.173.02, and its other C types.

/// `NV_STATUS`.
pub type NvStatus = u32;
/// `NvBool`.
pub type NvBool = u8;

/// `NV_TRUE`.
pub const TRUE: NvBool = 1;
/// `NV_FALSE`.
pub const FALSE: NvBool = 0;

/// `NV_OK`.
pub const OK: NvStatus = 0x0000_0000;
/// `NV_ERR_ILLEGAL_ACTION`.
pub const ILLEGAL_ACTION: NvStatus = 0x0000_0016;
/// `NV_ERR_INSUFFICIENT_RESOURCES`.
pub const INSUFFICIENT_RESOURCES: NvStatus = 0x0000_001A;
/// `NV_ERR_INVALID_ADDRESS`.
pub const INVALID_ADDRESS: NvStatus = 0x0000_001E;
/// `NV_ERR_INVALID_ARGUMENT`.
pub const INVALID_ARGUMENT: NvStatus = 0x0000_001F;
/// `NV_ERR_INVALID_PARAMETER`.
pub const INVALID_PARAMETER: NvStatus = 0x0000_003B;
/// `NV_ERR_INVALID_REQUEST`.
pub const INVALID_REQUEST: NvStatus = 0x0000_003F;
/// `NV_ERR_INVALID_STATE`.
pub const INVALID_STATE: NvStatus = 0x0000_0040;
/// `NV_ERR_NO_MEMORY`.
pub const NO_MEMORY: NvStatus = 0x0000_0051;
/// `NV_ERR_NOT_READY`.
pub const NOT_READY: NvStatus = 0x0000_0055;
/// `NV_ERR_NOT_SUPPORTED`.
pub const NOT_SUPPORTED: NvStatus = 0x0000_0056;
/// `NV_ERR_OBJECT_NOT_FOUND`.
pub const OBJECT_NOT_FOUND: NvStatus = 0x0000_0057;
/// `NV_ERR_OPERATING_SYSTEM`.
pub const OPERATING_SYSTEM: NvStatus = 0x0000_0059;
/// `NV_ERR_TIMEOUT_RETRY`.
pub const TIMEOUT_RETRY: NvStatus = 0x0000_0066;

/// `NvBool` from `bool`.
pub const fn bool(value: bool) -> NvBool {
    if value { TRUE } else { FALSE }
}
