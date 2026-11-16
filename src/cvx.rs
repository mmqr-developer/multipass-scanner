//! The OpenCV calls whose names or arguments differ between OpenCV versions.
//!
//! Linux distributions ship OpenCV 4; Homebrew on macOS ships OpenCV 5, which
//! moved some functions into a new `geometry` module.  OpenCV 4.11 gave
//! `cvtColor` and `GaussianBlur` an extra `AlgorithmHint` argument, and
//! OpenCV 5 gave it to `remap` and `warpAffine` too.  Each wrapper here has the
//! OpenCV 4.10 signature.  Where the hint exists it asks for
//! `ALGO_HINT_ACCURATE`, the generic implementation, so that results don't
//! depend on how OpenCV was built or on the processor.

use opencv::core::{Mat, Scalar, Size, ToInputArray, ToOutputArray};
use opencv::{imgproc, Result};

// The opencv crate turns this on for OpenCV 4.11 and later, 5 included.
opencv::opencv_has_inherent_feature_algorithm_hint! {
    use opencv::core::AlgorithmHint::ALGO_HINT_ACCURATE as HINT;
}
opencv::opencv_branch_5! {
    use opencv::geometry;
}
opencv::not_opencv_branch_5! {
    use opencv::calib3d;
}

opencv::opencv_branch_5! {
    pub const DIST_L2: i32 = geometry::DIST_L2;
    pub const RANSAC: i32 = geometry::RANSAC;
}
opencv::not_opencv_branch_5! {
    pub const DIST_L2: i32 = imgproc::DIST_L2;
    pub const RANSAC: i32 = calib3d::RANSAC;
}

/// `dst_cn` 0: as many channels as `code` implies.
pub fn cvt_color(src: &impl ToInputArray, dst: &mut impl ToOutputArray, code: i32, dst_cn: i32) -> Result<()> {
    opencv::opencv_has_inherent_feature_algorithm_hint! {
        {
            imgproc::cvt_color(src, dst, code, dst_cn, HINT)
        } else {
            imgproc::cvt_color(src, dst, code, dst_cn)
        }
    }
}

pub fn gaussian_blur(
    src: &impl ToInputArray,
    dst: &mut impl ToOutputArray,
    ksize: Size,
    sigma_x: f64,
    sigma_y: f64,
    border_type: i32,
) -> Result<()> {
    opencv::opencv_has_inherent_feature_algorithm_hint! {
        {
            imgproc::gaussian_blur(src, dst, ksize, sigma_x, sigma_y, border_type, HINT)
        } else {
            imgproc::gaussian_blur(src, dst, ksize, sigma_x, sigma_y, border_type)
        }
    }
}

pub fn remap(
    src: &impl ToInputArray,
    dst: &mut impl ToOutputArray,
    map1: &impl ToInputArray,
    map2: &impl ToInputArray,
    interpolation: i32,
    border_mode: i32,
    border_value: Scalar,
) -> Result<()> {
    opencv::opencv_branch_5! {
        {
            imgproc::remap(src, dst, map1, map2, interpolation, border_mode, border_value, HINT)
        } else {
            imgproc::remap(src, dst, map1, map2, interpolation, border_mode, border_value)
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn warp_affine(
    src: &impl ToInputArray,
    dst: &mut impl ToOutputArray,
    m: &impl ToInputArray,
    dsize: Size,
    flags: i32,
    border_mode: i32,
    border_value: Scalar,
) -> Result<()> {
    opencv::opencv_branch_5! {
        {
            imgproc::warp_affine(src, dst, m, dsize, flags, border_mode, border_value, HINT)
        } else {
            imgproc::warp_affine(src, dst, m, dsize, flags, border_mode, border_value)
        }
    }
}

pub fn convex_hull(points: &impl ToInputArray, hull: &mut impl ToOutputArray, clockwise: bool, return_points: bool) -> Result<()> {
    opencv::opencv_branch_5! {
        {
            geometry::convex_hull(points, hull, clockwise, return_points)
        } else {
            imgproc::convex_hull(points, hull, clockwise, return_points)
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn estimate_affine_partial_2d(
    from: &impl ToInputArray,
    to: &impl ToInputArray,
    inliers: &mut impl ToOutputArray,
    method: i32,
    ransac_reproj_threshold: f64,
    max_iters: usize,
    confidence: f64,
    refine_iters: usize,
) -> Result<Mat> {
    opencv::opencv_branch_5! {
        {
            geometry::estimate_affine_partial_2d(from, to, inliers, method, ransac_reproj_threshold, max_iters, confidence, refine_iters)
        } else {
            calib3d::estimate_affine_partial_2d(from, to, inliers, method, ransac_reproj_threshold, max_iters, confidence, refine_iters)
        }
    }
}
