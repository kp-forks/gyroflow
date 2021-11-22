use enterpolation::{ Curve, Merge };
use enterpolation::bspline::BSpline;

use crate::core::StabilizationManager;
use crate::core::Undistortion as Undistortion;
type Quat64 = nalgebra::UnitQuaternion<f64>;

#[derive(Default, Clone, Copy, Debug)]
pub struct Point2D(f64, f64);
impl Merge<f64> for Point2D {
    fn merge(self, other: Self, factor: f64) -> Self {
        Point2D(
            self.0 * (1.0 - factor) + other.0 * factor,
            self.1 * (1.0 - factor) + other.1 * factor
        )
    }
}

#[derive(Default, Clone)]
pub struct AdaptiveZoom {
    calib_dimension: (f64, f64),
    camera_matrix: nalgebra::Matrix3<f64>,
    distortion_coeffs: Vec<f64>
}

impl AdaptiveZoom {
    pub fn from_manager(mgr: &StabilizationManager) -> Self {
        Self {
            calib_dimension: mgr.lens.calib_dimension,
            camera_matrix: nalgebra::Matrix3::from_row_slice(&mgr.camera_matrix_or_default()),
            distortion_coeffs: mgr.lens.distortion_coeffs.clone()
        }
    }

    fn min_rolling(a: &[f64], window: usize) -> Vec<f64> {
        a.windows(window).map(|window| {
            window.iter().copied().reduce(f64::min).unwrap()
        }).collect()
    }

    fn find_fcorr(&self, center: Point2D, polygon: &[Point2D], output_dim: (usize, usize)) -> (f64, usize) {
        let (output_width, output_height) = (output_dim.0 as f64, output_dim.1 as f64);
        let angle_output = (output_height as f64 / 2.0).atan2(output_width / 2.0);

        // fig, ax = plt.subplots()

        let polygon: Vec<Point2D> = polygon.iter().map(|p| Point2D(p.0 - center.0, p.1 - center.1)).collect();
        // ax.scatter(polygon[:,0], polygon[:,1])

        let dist_p: Vec<f64> = polygon.iter().map(|pt| nalgebra::Vector2::new(pt.0, pt.1).norm()).collect();
        let angles: Vec<f64> = polygon.iter().map(|pt| pt.1.atan2(pt.0).abs()).collect();

        // ax.plot(distP*np.cos(angles), distP*np.sin(angles), 'ro')
        // ax.plot(distP[mask]*np.cos(angles[mask]), distP[mask]*np.sin(angles[mask]), 'yo')
        // ax.add_patch(matplotlib.patches.Rectangle((-output_width/2,-output_height/2), output_width, output_height,color="yellow"))
        let d_width : Vec<f64> = angles.iter().map(|a| ((output_width / 2.0) / a.cos()).abs()).collect();
        let d_height: Vec<f64> = angles.iter().map(|a| ((output_height / 2.0) / a.sin()).abs()).collect();

        let mut ffactor: Vec<f64> = d_width.iter().zip(dist_p.iter()).map(|(v, d)| v / d).collect();

        ffactor.iter_mut().enumerate().for_each(|(i, v)| {
            if angle_output <= angles[i].abs() && angles[i].abs() < (std::f64::consts::PI - angle_output) {
                *v = d_height[i] / dist_p[i];
            }
        });

        // Find max value and it's index
        let max = ffactor.iter().enumerate().fold((0, 0.0), |max, (ind, &val)| if val > max.1 { (ind, val) } else { max });

        (max.1, max.0)
    }

    fn find_fov(&self, center: Point2D, polygon: &[Point2D], output_dim: (usize, usize)) -> f64 {
        let num_int_points = 20;
        // let (original_width, original_height) = self.calib_dimension;
        let (fcorr, idx) = self.find_fcorr(center, polygon, output_dim);
        let n_p = polygon.len();
        let relevant_p = [
            polygon[(idx - 1) % n_p], 
            polygon[idx],
            polygon[(idx + 1) % n_p]
        ];

        // TODO: `distance` is not used for the interpolation. It should be used for more accurate results. It's the x axis for `scipy.interp1d`
        // let distance = {
        //     let mut sum = 0.0;
        //     let mut d: Vec<f64> = relevant_p[1..].iter().enumerate().map(|(i, v)| {
        //         sum += ((v.0 - relevant_p[i].0).powf(2.0) + (v.1 - relevant_p[i].1).powf(2.0)).sqrt();
        //         sum
        //     }).collect();
        //     d.insert(0, 0.0);
        //     d.iter_mut().for_each(|v| *v /= sum);
        //     d
        // };

        let bspline = BSpline::builder()
                    .clamped()
                    .elements(&relevant_p)
                    .equidistant::<f64>()
                    .degree(2) // 1 - linear, 2 - quadratic, 3 - cubic
                    .normalized()
                    .constant::<3>()
                    .build().unwrap();

        // let alpha: Vec<f64> = (0..numIntPoints).map(|i| i as f64 * (1.0 / numIntPoints as f64)).collect();
        let interpolated_points: Vec<Point2D> = bspline.take(num_int_points).collect();

        let (fcorr_i, _) = self.find_fcorr(center, &interpolated_points, output_dim);

        // plt.plot(polygon[:,0], polygon[:,1], 'ro')
        // plt.plot(relevantP[:,0], relevantP[:,1], 'bo')
        // plt.plot(interpolated_points[:,0], interpolated_points[:,1], 'yo')
        // plt.show()

        1.0 / fcorr.max(fcorr_i)
    }
    
    pub fn compute(&self, quaternions: &[Quat64], output_dim: (usize, usize), fps: f64, smoothing_focus: Option<f64>, tstart: Option<f64>, tend: Option<f64>) -> Vec<(f64, Point2D)> { // Vec<fovValues, focalCenters>
        let smoothing_focus = smoothing_focus.unwrap_or(2.0);
        // if smoothing_focus == -1: Totally disable
        // if smoothing_focus == -2: Find minimum sufficient crop

        // let mut smoothing_num_frames = (smoothing_center * fps).floor() as usize;
        // if smoothing_num_frames % 2 == 0 {
        //     smoothing_num_frames += 1;
        // }

        let mut smoothing_focus_frames = (smoothing_focus * fps).floor() as usize;
        if smoothing_focus_frames % 2 == 0 {
            smoothing_focus_frames += 1;
        }

        let boundary_polygons: Vec<Vec<Point2D>> = quaternions.iter().map(|q| self.bounding_polygon(*q, 9)).collect();
        // let focus_windows: Vec<Point2D> = boundaryBoxes.iter().map(|b| self.find_focal_center(b, output_dim)).collect();

        // TODO: implement smoothing of position of crop, s.t. cropping area can "move" anywhere within bounding polygon
        let crop_center_positions: Vec<Point2D> = quaternions.iter().map(|_| Point2D(self.calib_dimension.0 / 2.0, self.calib_dimension.1 / 2.0)).collect();

        // if smoothing_center > 0 {
        //     let mut focus_windows_pad = pad_edge(&focus_windows, (smoothing_num_frames / 2, smoothing_num_frames / 2));
        //     let mut filter_coeff = gaussian_window(smoothing_num_frames, smoothing_num_frames as f64 / 6.0);
        //     let sum: f64 = filter_coeff.iter().sum();
        //     filter_coeff.iter_mut().for_each(|v| *v /= sum);
        //     focus_windows = convolve(&focus_windows_pad.map(|v| v.0).collect(), &filter_coeff).iter().zip(
        //         convolve(&focus_windows_pad.map(|v| v.1).collect(), &filter_coeff).iter()
        //     ).collect()
        // }
        let mut fov_values: Vec<f64> = crop_center_positions.iter().zip(boundary_polygons.iter()).map(|(center, polygon)| self.find_fov(*center, polygon, output_dim)).collect();

        if tend.is_some() {
            // Only within render range.
            let max_fov = fov_values.iter().copied().reduce(f64::max).unwrap();
            let l = (quaternions.len() - 1) as f64;
            let first_ind = (l * tstart.unwrap()) as usize;
            let last_ind  = (l * tend.unwrap()) as usize;
            fov_values[0..first_ind].iter_mut().for_each(|v| *v = max_fov);
            fov_values[last_ind..].iter_mut().for_each(|v| *v = max_fov);
        }

        if smoothing_focus > 0.0 {
            let mut filter_coeff_focus = gaussian_window(smoothing_focus_frames, smoothing_focus_frames as f64 / 6.0);
            let sum: f64 = filter_coeff_focus.iter().sum();
            filter_coeff_focus.iter_mut().for_each(|v| *v /= sum);

            let fov_values_pad = pad_edge(&fov_values, (smoothing_focus_frames / 2, smoothing_focus_frames / 2));
            let fov_min = Self::min_rolling(&fov_values_pad, smoothing_focus_frames);
            let fov_min_pad = pad_edge(&fov_min, (smoothing_focus_frames / 2, smoothing_focus_frames / 2));

            fov_values = convolve(&fov_min_pad, &filter_coeff_focus);
        } else if smoothing_focus == -1.0 { // disabled
            let max_f = fov_values.iter().copied().reduce(f64::min).unwrap();
            fov_values.iter_mut().for_each(|v| *v = max_f);
        } else if smoothing_focus == -2.0 { // apply nothing
            fov_values.iter_mut().for_each(|v| *v = 1.0);
        }

        fov_values.iter().copied().zip(crop_center_positions.iter().copied()).collect()
    }

    fn bounding_polygon(&self, quat: nalgebra::UnitQuaternion<f64>, num_points: usize) -> Vec<Point2D> {
        let (w, h) = (self.calib_dimension.0, self.calib_dimension.1);

        let mut r = *quat.to_rotation_matrix().matrix();
        r[(0, 1)] *= -1.0; r[(0, 2)] *= -1.0;
        r[(1, 0)] *= -1.0; r[(2, 0)] *= -1.0;

        let pts = num_points - 1;
        let dim_ratio = ((w / pts as f64), (h / pts as f64));
        let mut distorted_points: Vec<(f64, f64)> = Vec::with_capacity(pts * 4);
        for i in 0..pts { distorted_points.push((i as f64 * dim_ratio.0,              0.0)); }
        for i in 0..pts { distorted_points.push((w,                                   i as f64 * dim_ratio.1)); }
        for i in 0..pts { distorted_points.push(((pts - i) as f64 * dim_ratio.0,      h)); }
        for i in 0..pts { distorted_points.push((0.0,                                 (pts - i) as f64 * dim_ratio.1)); }

        let k = self.camera_matrix;
        let undistorted_points = Undistortion::<()>::undistort_points(&distorted_points, k, &self.distortion_coeffs, r, k);

        undistorted_points.into_iter().map(|v| Point2D(v.0, v.1)).collect()
    }

    /*fn find_focal_center(&self, box_: (f64, f64, f64, f64), output_dim: (usize, usize)) -> Point2D {
        let (mleft, mright, mtop, mbottom) = box_;
        let (mut window_width, mut window_height) = (output_dim.0 as f64, output_dim.1 as f64);

        let maxX = mright - mleft;
        let maxY = mbottom - mtop;

        let ratio = maxX / maxY;
        let output_ratio = output_dim.0 as f64 / output_dim.1 as f64;

        let mut fX = 0.0;
        let mut fY = 0.0;
        if maxX / output_ratio < maxY {
            window_width = maxX;
            window_height = maxX / output_ratio;
            fX = mleft + window_width / 2.0;
            fY = self.calib_dimension.1 as f64 / 2.0;
            if fY + window_height / 2.0 > mbottom {
                fY = mbottom - window_height / 2.0;
            } else if fY - window_height / 2.0 < mtop {
                fY = mtop + window_height / 2.0;
            }
        } else {
            window_height = maxY;
            window_width = maxY * output_ratio;
            fY = mtop + window_height / 2.0;
            fX = self.calib_dimension.0 as f64 / 2.0;
            if fX + window_width / 2.0 > mright {
                fX = mright - window_width / 2.0;
            } else if fX - window_width / 2.0 < mleft {
                fX = mleft + window_width / 2.0;
            }
        }
        Point2D(fX, fY) //, window_width, window_height)
    }*/
}

fn convolve(v: &[f64], filter: &[f64]) -> Vec<f64> {
    v.windows(filter.len()).map(|window| {
        window.iter().zip(filter).map(|(x, y)| x * y).sum()
    }).collect()
}

fn gaussian_window(m: usize, std: f64) -> Vec<f64> {
    let step = 1.0 / m as f64;
    let n: Vec<f64> = (0..m).map(|i| (i as f64 * step) - (m as f64 - 1.0) / 2.0).collect();
    let sig2 = 2.0 * std * std;
    n.iter().map(|v| (-*v).powf(2.0) / sig2).collect()
}

fn pad_edge(arr: &[f64], pad_to: (usize, usize)) -> Vec<f64> {
    let first = *arr.first().unwrap();
    let last = *arr.last().unwrap();

    let mut new_arr = vec![0.0; arr.len() + pad_to.0 + pad_to.1];
    new_arr[pad_to.0..pad_to.0 + arr.len()].copy_from_slice(arr);

    for i in 0..pad_to.0 { new_arr[i] = first; }
    for i in pad_to.0 + arr.len()..new_arr.len() { new_arr[i] = last; }

    new_arr
}
