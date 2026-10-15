//! V4L2 structures generated with `bindgen /usr/include/linux/videodev2.h` (allowlist: the
//! capability, format, frame-size and frame-interval records and their enum constants).
//! Regenerate with the same allowlist if the kernel ABI ever grows; layouts are checked in tests.
#![allow(non_camel_case_types, dead_code)]

pub const V4L2_CAP_VIDEO_CAPTURE: u32 = 1;
pub const V4L2_CAP_VIDEO_CAPTURE_MPLANE: u32 = 4096;
pub const V4L2_CAP_VIDEO_M2M_MPLANE: u32 = 16384;
pub const V4L2_CAP_VIDEO_M2M: u32 = 32768;
pub const V4L2_CAP_DEVICE_CAPS: u32 = 2147483648;
pub type __u8 = ::core::ffi::c_uchar;
pub type __u32 = ::core::ffi::c_uint;
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct v4l2_fract {
    pub numerator: __u32,
    pub denominator: __u32,
}
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct v4l2_capability {
    pub driver: [__u8; 16usize],
    pub card: [__u8; 32usize],
    pub bus_info: [__u8; 32usize],
    pub version: __u32,
    pub capabilities: __u32,
    pub device_caps: __u32,
    pub reserved: [__u32; 3usize],
}
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct v4l2_fmtdesc {
    pub index: __u32,
    pub type_: __u32,
    pub flags: __u32,
    pub description: [__u8; 32usize],
    pub pixelformat: __u32,
    pub mbus_code: __u32,
    pub reserved: [__u32; 3usize],
}
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct v4l2_frmsize_discrete {
    pub width: __u32,
    pub height: __u32,
}
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct v4l2_frmsize_stepwise {
    pub min_width: __u32,
    pub max_width: __u32,
    pub step_width: __u32,
    pub min_height: __u32,
    pub max_height: __u32,
    pub step_height: __u32,
}
#[repr(C)]
#[derive(Copy, Clone)]
pub struct v4l2_frmsizeenum {
    pub index: __u32,
    pub pixel_format: __u32,
    pub type_: __u32,
    pub __bindgen_anon_1: v4l2_frmsizeenum__bindgen_ty_1,
    pub reserved: [__u32; 2usize],
}
#[repr(C)]
#[derive(Copy, Clone)]
pub union v4l2_frmsizeenum__bindgen_ty_1 {
    pub discrete: v4l2_frmsize_discrete,
    pub stepwise: v4l2_frmsize_stepwise,
}
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct v4l2_frmival_stepwise {
    pub min: v4l2_fract,
    pub max: v4l2_fract,
    pub step: v4l2_fract,
}
#[repr(C)]
#[derive(Copy, Clone)]
pub struct v4l2_frmivalenum {
    pub index: __u32,
    pub pixel_format: __u32,
    pub width: __u32,
    pub height: __u32,
    pub type_: __u32,
    pub __bindgen_anon_1: v4l2_frmivalenum__bindgen_ty_1,
    pub reserved: [__u32; 2usize],
}
#[repr(C)]
#[derive(Copy, Clone)]
pub union v4l2_frmivalenum__bindgen_ty_1 {
    pub discrete: v4l2_fract,
    pub stepwise: v4l2_frmival_stepwise,
}
